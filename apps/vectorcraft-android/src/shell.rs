//! The Android window: one eframe app around [`VectorcraftApp`], with the system file picker and
//! the app's private folders behind its [`Services`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use android_activity::AndroidApp;
use vectorcraft_engine::Session;
use vectorcraft_ui_egui::place::{PlaceArrival, PlaceInbox};
use vectorcraft_ui_egui::touch::{self, Source};
use vectorcraft_ui_egui::{Services, VectorcraftApp};

use crate::jni_bridge::Java;

type Inbox = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// How often the window asks Java whether a picked file has arrived, while one is awaited.
const POLL: Duration = Duration::from_millis(150);
/// Frames between asks for the system bars' size.
const INSETS_EVERY: u64 = 30;

/// What the file picker has given back, shared between the services and the window.
struct Picks {
    java: Arc<Java>,
    inbox: Inbox,
    place_inbox: PlaceInbox,
    /// Requests the picker hasn't finished: each ends with a `done` line from Java.
    waiting: AtomicUsize,
    outgoing: PathBuf,
    counter: AtomicUsize,
}

impl Picks {
    fn start(&self, mode: &str) {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        if let Err(e) = self.java.call("startPick", &[mode]) {
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            log::error!("couldn't open the file picker: {e}");
        }
    }

    /// Hands a file to the system's save screen. The file is copied where the person chooses once
    /// they confirm; `Ok` means the screen was shown, not that they saved.
    fn save(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        let clean: String = Path::new(name).file_name().map_or_else(|| "Untitled".into(), |n| n.to_string_lossy().into_owned());
        let folder = self.outgoing.join(self.counter.fetch_add(1, Ordering::SeqCst).to_string());
        std::fs::create_dir_all(&folder).map_err(|e| format!("couldn't prepare the file: {e}"))?;
        let path = folder.join(&clean);
        std::fs::write(&path, bytes).map_err(|e| format!("couldn't prepare the file: {e}"))?;
        self.java.call("startSave", &[&clean, &path.to_string_lossy()]).map(drop)
    }

    /// Moves what Java has collected into the inboxes. Returns whether anything is still awaited.
    fn poll(&self) -> bool {
        if self.waiting.load(Ordering::SeqCst) == 0 {
            return false;
        }
        let events = match self.java.call("pollEvents", &[]) {
            Ok(events) => events,
            Err(e) => {
                log::error!("couldn't ask for picked files: {e}");
                return true;
            }
        };
        for line in events.lines() {
            let mut parts = line.splitn(3, '\t');
            let (kind, name, rest) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            match kind {
                "done" => {
                    let _ = self.waiting.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
                }
                "open" | "place" => match std::fs::read(rest) {
                    Ok(bytes) => {
                        if kind == "open" {
                            lock(&self.inbox).push((name.to_string(), bytes));
                        } else {
                            lock(&self.place_inbox).push(PlaceArrival { name: name.to_string(), bytes, drop: None });
                        }
                        let _ = std::fs::remove_file(rest);
                    }
                    Err(e) => log::error!("couldn't read the picked file {name}: {e}"),
                },
                "saved" => log::info!("saved {name}: {rest}"),
                "error" => log::error!("file picker: {name} {rest}"),
                _ => {}
            }
        }
        self.waiting.load(Ordering::SeqCst) > 0
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Shell {
    app: VectorcraftApp,
    picks: Arc<Picks>,
    java: Arc<Java>,
    /// The system bars' and cut-outs' size in points: top, bottom, left, right.
    insets: [f32; 4],
    /// The on-screen Shift key is latched on: the touch stand-in for holding Shift.
    shift_latched: bool,
}

impl Shell {
    fn new(cc: &eframe::CreationContext<'_>, java: Arc<Java>, root: &Path) -> Self {
        VectorcraftApp::install_ime_reset_hooks(&cc.egui_ctx);
        let outgoing = root.join("outgoing");
        let _ = std::fs::remove_dir_all(&outgoing);
        let picks = Arc::new(Picks {
            java: java.clone(),
            inbox: Inbox::default(),
            place_inbox: PlaceInbox::default(),
            waiting: AtomicUsize::new(0),
            outgoing,
            counter: AtomicUsize::new(0),
        });
        let (open, place, save, url_java) = (picks.clone(), picks.clone(), picks.clone(), java.clone());
        let services = Services {
            open_async: Some(Box::new(move || open.start("open"))),
            place_async: Some(Box::new(move || place.start("place"))),
            inbox: Some(picks.inbox.clone()),
            place_inbox: Some(picks.place_inbox.clone()),
            download: Some(Box::new(move |name: &str, bytes: &[u8]| save.save(name, bytes))),
            open_url: Some(Box::new(move |url: &str| {
                if let Err(e) = url_java.call("openUrl", &[url]) {
                    log::error!("couldn't open {url}: {e}");
                }
            })),
            ..Default::default()
        };
        let mut app = VectorcraftApp::new(Session::new(), services);
        // Libraries and Data Recovery copies live in the app's private folder.
        let folder = |name: &str| Some(root.join(name).to_string_lossy().into_owned());
        app.session.swatch_libraries.set_user_dir(folder("Swatches"));
        app.session.style_libraries.set_user_dir(folder("Graphic Styles"));
        app.session.symbol_libraries.set_user_dir(folder("Symbols"));
        app.session.brush_libraries.set_user_dir(folder("Brushes"));
        app.session.libraries.set_dir(folder("Libraries"));
        app.session.recovery.set_default_folder(folder("Data Recovery"));
        vectorcraft_ui_egui::panels::library_panel::restore(&mut app);
        Self { app, picks, java, insets: [0.0; 4], shift_latched: false }
    }

    fn read_insets(&mut self, ctx: &egui::Context) {
        let Ok(text) = self.java.call("insets", &[]) else { return };
        let px: Vec<f32> = text.split(',').filter_map(|v| v.trim().parse().ok()).collect();
        if let [top, bottom, left, right] = px[..] {
            let scale = ctx.pixels_per_point().max(0.1);
            self.insets = [top / scale, bottom / scale, left / scale, right / scale];
        }
    }
}

impl eframe::App for Shell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.cumulative_frame_nr() % INSETS_EVERY == 0 {
            self.read_insets(ctx);
        }
        if self.picks.poll() {
            ctx.request_repaint_after(POLL);
        }
        self.app.tick(ctx);
        self.app.finish_ime_logic(ctx);
        if self.app.ui.status == "quit" {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        // Shift, without a keyboard: the S Pen's side button, or the on-screen Shift key. It keeps
        // proportions when scaling and makes squares and circles, like the key does.
        if self.shift_latched || winit::platform::android::stylus_button_pressed() {
            hold_shift(raw_input, ctx.input(|i| i.modifiers));
        }
        // Tell the app what touches the screen. Once the pen has been used, fingers only navigate
        // (pinch, two-finger pan, the undo and redo taps), so a resting palm never draws.
        touch::set_source(match winit::platform::android::last_pointer_tool() {
            winit::platform::android::PointerTool::Stylus => Source::Pen,
            winit::platform::android::PointerTool::Finger => Source::Finger,
            winit::platform::android::PointerTool::Mouse => Source::Mouse,
        });
        self.app.raw_input_hook(ctx, raw_input);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Keep the interface clear of the status bar, the navigation bar and a camera cut-out.
        let [top, bottom, left, right] = self.insets;
        let full = ui.max_rect();
        let area = egui::Rect::from_min_max(full.min + egui::vec2(left, top), full.max - egui::vec2(right, bottom));
        let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(area));
        self.app.ui(&mut inner);
        self.shift_key(ui.ctx(), area);
    }
}

impl Shell {
    /// The on-screen Shift key: tap to hold Shift down, tap again to let go. It sits at the lower
    /// left, above the navigation bar, and shows that the S Pen's button is held too.
    fn shift_key(&mut self, ctx: &egui::Context, area: egui::Rect) {
        let pen = winit::platform::android::stylus_button_pressed();
        egui::Area::new(egui::Id::unique("android-shift-key"))
            .order(egui::Order::Foreground)
            .fixed_pos(area.left_bottom() + egui::vec2(12.0, -116.0))
            .show(ctx, |ui| {
                let on = self.shift_latched || pen;
                let label = egui::RichText::new("⇧ Shift").size(18.0);
                let button = egui::Button::new(label).selected(on).min_size(egui::vec2(96.0, 44.0));
                if ui.add(button).on_hover_text("Hold Shift: keeps proportions, makes squares and circles. The S Pen button does the same.").clicked() {
                    self.shift_latched = !self.shift_latched;
                }
                ui.add_space(6.0);
                let pen_only = touch::pen_seen();
                let label = egui::RichText::new("✎ Pen only").size(18.0);
                let button = egui::Button::new(label).selected(pen_only).min_size(egui::vec2(96.0, 44.0));
                if ui.add(button).on_hover_text("Fingers only move and zoom the view; only the pen draws. Starts on once you use the pen.").clicked() {
                    touch::set_pen_seen(!pen_only);
                }
            });
    }
}

/// Puts Shift on the frame and on every pointer, key and wheel event in it, as a keyboard's Shift
/// would be for a frame in which it is held. `current` keeps any other modifier a hardware
/// keyboard holds.
fn hold_shift(raw: &mut egui::RawInput, current: egui::Modifiers) {
    let held = egui::Modifiers { shift: true, ..current };
    raw.events.insert(0, egui::Event::ModifiersChanged(held));
    for event in &mut raw.events {
        match event {
            egui::Event::PointerButton { modifiers, .. } | egui::Event::Key { modifiers, .. } | egui::Event::MouseWheel { modifiers, .. } => {
                modifiers.shift = true;
            }
            _ => {}
        }
    }
}

/// Runs the window until the activity finishes.
pub fn run(app: AndroidApp) -> Result<(), String> {
    let root = app.internal_data_path().ok_or("Android gave the app no private folder")?;
    let java = Arc::new(Java::new(&app)?);
    let options = eframe::NativeOptions { android_app: Some(app), renderer: eframe::Renderer::Wgpu, ..Default::default() };
    eframe::run_native("VectorCraft", options, Box::new(move |cc| Ok(Box::new(Shell::new(cc, java, &root))))).map_err(|e| e.to_string())
}
