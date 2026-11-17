//! FilmCraft hosted inside Workbench's side panel: the same app as the desktop build, drawn into a view
//! Workbench owns, with Workbench's look. A video editor always shows its full tools (panels, timeline,
//! monitors), so there is no Hands off. Workbench (and Claude) drive it through its control channel. See
//! `craft-embed`.
//!
//! The protocol maps onto FilmCraft's control channel (`filmcraft_ui_egui::control`):
//! `engine.execute {command, params}`, `engine.commands` and `ui.inspect` run at once; `document.inspect`
//! is the engine's `project.inspect` with the active sequence's `sequence.inspect`; `file.open {path}`,
//! `file.new`, `file.saveAs {path}` and `file.import {path}` (also as `engine.execute`) work on real paths. Requests FilmCraft answers only after a laid-out
//! frame (`ui.click`, `ui.elements`...) reply `{"pending": true}` and settle over the next frames.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use craft_embed::{EmbedApp, egui, egui_wgpu};
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::{self, ControlRequest, Outcome};
use serde_json::{Value, json};

struct Film {
    app: FilmcraftApp,
    /// eframe's `App` methods take a frame; FilmCraft's only sets the UI language and reads the GPU on it.
    frame: eframe::Frame,
    /// The app's own control queue, for the requests it answers after a frame (it drains it each frame).
    queue: Sender<ControlRequest>,
    /// The reply still to come for the last such request.
    waiting: Option<Receiver<Value>>,
}

impl EmbedApp for Film {
    fn logic(&mut self, ctx: &egui::Context) {
        eframe::App::logic(&mut self.app, ctx, &mut self.frame);
    }
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        eframe::App::raw_input_hook(&mut self.app, ctx, raw);
    }
    fn ui(&mut self, ui: &mut egui::Ui) {
        eframe::App::ui(&mut self.app, ui, &mut self.frame);
    }
    fn control(&mut self, ctx: &egui::Context, request: &str) -> String {
        let req: Value = serde_json::from_str(request).unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(json!({}));
        if hands_off(method, &params) {
            return json!({"ok": true, "result": {"on": false}}).to_string();
        }
        self.request(ctx, method, params).to_string()
    }
    fn settle(&mut self) -> Option<String> {
        let rx = self.waiting.as_ref()?;
        let reply = match rx.try_recv() {
            Ok(v) => v,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => json!({"ok": false, "error": "the app dropped the request"}),
        };
        self.waiting = None;
        Some(reply.to_string())
    }
    fn gpu(&mut self, rs: &egui_wgpu::RenderState) {
        // What the desktop's app creator does with `cc.wgpu_render_state`: the GPU compositor for the monitors.
        self.frame.wgpu_render_state = Some(rs.clone());
        self.app.set_wgpu(rs.clone());
    }
    fn exit(&mut self) {
        eframe::App::on_exit(&mut self.app);
    }
}

impl Film {
    /// One request of Workbench's protocol to its `{ok, result}` / `{ok: false, error}` reply.
    fn request(&mut self, ctx: &egui::Context, method: &str, params: Value) -> Value {
        let path = |p: &Value| p.get("path").and_then(Value::as_str).map(str::to_owned);
        match method {
            "file.new" => self.new_document(),
            "file.open" => match path(&params) {
                Some(p) => self.engine("file.open", json!({"path": p})),
                None => json!({"ok": false, "error": "file.open needs `path`"}),
            },
            "file.saveAs" => match path(&params) {
                Some(p) => self.engine("file.saveAs", json!({"path": p})),
                None => json!({"ok": false, "error": "file.saveAs needs `path`"}),
            },
            "document.inspect" => self.inspect(),
            "file.import" => self.import(ctx, &params),
            // a UI command sent as the method (`view.workbench {on, accent}`)
            m if m.starts_with("view.") => match filmcraft_ui_egui::menus::invoke_command(&mut self.app, ctx, m, params) {
                Ok(v) => json!({"ok": true, "result": v}),
                Err(e) => json!({"ok": false, "error": e}),
            },
            // the same by their engine names
            "engine.execute" if params.get("command").and_then(Value::as_str) == Some("file.new") => self.new_document(),
            "engine.execute" if params.get("command").and_then(Value::as_str) == Some("file.import") => {
                let inner = params.get("params").cloned().unwrap_or(json!({}));
                self.import(ctx, &inner)
            }
            // the UI's own: a screenshot is Workbench's to take, of the whole panel
            "ui.screenshot" => json!({"ok": false, "error": "Workbench takes the screenshot of the panel itself"}),
            // these need a laid-out frame (synthetic input, widget rects): the app answers them over the next frames
            m if m.starts_with("ui.") && m != "ui.inspect" => self.queue(method, params),
            _ => {
                let (req, _reply) = ControlRequest::new(method, params.clone());
                match control::handle(&mut self.app, ctx, &req) {
                    Outcome::Done(v) => v,
                    // not ready yet (an element not on screen): the app tries again on later frames
                    Outcome::Retry(_) => self.queue(method, params),
                    Outcome::AfterInput => json!({"ok": true, "result": null}),
                    Outcome::Screenshot { .. } => json!({"ok": false, "error": "Workbench takes the screenshot of the panel itself"}),
                }
            }
        }
    }

    /// Hand a request to the app's control queue; the host runs frames and asks [`EmbedApp::settle`] for the reply.
    fn queue(&mut self, method: &str, params: Value) -> Value {
        let (req, rx) = ControlRequest::new(method, params);
        if self.queue.send(req).is_err() {
            return json!({"ok": false, "error": "the app's control queue is closed"});
        }
        self.waiting = Some(rx);
        json!({"ok": true, "result": {"pending": true}})
    }

    /// An engine command straight on the session (files by real path, no dialogs).
    fn engine(&mut self, command: &str, params: Value) -> Value {
        match self.app.session.execute(command, params) {
            Ok(v) => json!({"ok": true, "result": v}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        }
    }

    /// Media files (video, audio, stills) from disk into the open project's shown bin, as dropping them on
    /// the Project panel does: `{path}` or `{paths: [...]}`. FilmCraft always has a project open (a new
    /// session starts with an empty one), so there is always one to import into.
    fn import(&mut self, ctx: &egui::Context, params: &Value) -> Value {
        let mut paths: Vec<Value> = params.get("paths").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(p) = params.get("path").filter(|p| p.is_string()) {
            paths.push(p.clone());
        }
        if paths.is_empty() {
            return json!({"ok": false, "error": "file.import needs `path` (or `paths`)"});
        }
        let mut args = json!({"paths": paths});
        if let Some(bin) = params.get("bin") {
            args["bin"] = bin.clone();
        }
        match filmcraft_ui_egui::menus::invoke_command(&mut self.app, ctx, "file.import", args) {
            Ok(v) => json!({"ok": true, "result": v}),
            Err(e) => json!({"ok": false, "error": e}),
        }
    }

    /// A new, empty project with one open sequence to edit in.
    fn new_document(&mut self) -> Value {
        let project = self.engine("file.newProject", json!({"name": "Untitled"}));
        if project["ok"] != json!(true) {
            return project;
        }
        let sequence = self.engine("file.newSequence", json!({"name": "Sequence 01"}));
        if sequence["ok"] != json!(true) {
            return sequence;
        }
        json!({"ok": true, "result": {"project": project["result"], "sequence": sequence["result"]}})
    }

    /// The project, and the sequence open in the timeline.
    fn inspect(&mut self) -> Value {
        let project = self.engine("project.inspect", json!({}));
        if project["ok"] != json!(true) {
            return project;
        }
        let sequence = match self.app.session.active_sequence() {
            Some(_) => self.engine("sequence.inspect", json!({}))["result"].clone(),
            None => Value::Null,
        };
        json!({"ok": true, "result": {"project": project["result"], "sequence": sequence}})
    }
}

// the desktop app's sound output (playback and scrubbing through the default device)
#[path = "../../filmcraft/src/audio.rs"]
#[allow(dead_code)]
mod audio;

fn make(ctx: &egui::Context) -> Box<dyn EmbedApp> {
    filmcraft_ui_egui::install_ime_reset_callbacks(ctx);
    // OS hardware video decoders (VideoToolbox) in front of FilmCraft's own, as on the desktop
    let _ = filmcraft_platform::register();
    // No auto-save, recovery journal or prefs file: Workbench owns the files.
    let (queue, rx) = std::sync::mpsc::channel();
    let mut app = FilmcraftApp::new(Session::default()).with_control(rx);
    // Workbench's look from the first frame; Workbench sets the mode's accent right after
    let _ = filmcraft_ui_egui::menus::invoke_command(&mut app, ctx, "view.workbench", json!({"on": true}));
    // sound, as on the desktop (CRAFT_NO_AUDIO=1: none, for test runs)
    if std::env::var_os("CRAFT_NO_AUDIO").is_none() {
        app.audio = Some(Box::new(audio::CpalOut::new()));
        let rate = app.session.active_sequence().map(|q| q.settings.sample_rate);
        app.scrub_out = Some(Box::new(audio::CpalScrubOut::new(&app.session.prefs.audio_hardware, rate)));
    }
    Box::new(Film { app, frame: eframe::Frame::_new_kittest(), queue, waiting: None })
}

/// Whether a request is `view.handsOff`: a video editor has no Hands off (E's rule), so the panel takes the call
/// and nothing changes.
fn hands_off(method: &str, params: &Value) -> bool {
    let command = params.get("command").or_else(|| params.get("id")).and_then(Value::as_str);
    method == "view.handsOff" || (matches!(method, "engine.execute" | "command" | "ui.command") && command == Some("view.handsOff"))
}

craft_embed::export_embed!(make);
