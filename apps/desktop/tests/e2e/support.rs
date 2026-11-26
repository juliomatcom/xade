#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Once;
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers, Vec2};
use xade_core::{AgentProvider, Role};

use crate::fake_provider::FakeProvider;
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use xade::App;

/// Keeps the app away from the machine it runs on: no real `claude` or `agy` is found (so no
/// provider starts and no history is imported), and nothing is read from or written to the real
/// home folder. Set once, before any app starts.
fn isolate() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let home = std::env::temp_dir().join(format!("xade-e2e-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("HOME", &home);
        std::env::set_var("PATH", "/usr/bin:/bin");
    });
}

/// A running Xade on a workspace folder holding some files.
pub struct Xade {
    pub ui: Harness<'static, App>,
    dir: PathBuf,
    // The session manager runs on this; it must outlive the window.
    _rt: tokio::runtime::Runtime,
}

impl Xade {
    /// Starts the app on a new workspace with `files` in it, in a window of `size`, over a
    /// mocked model with no history.
    pub fn start(files: &[(&str, &str)], size: impl Into<Vec2>) -> Self {
        Self::start_with(files, size, &[])
    }

    /// Like `start`, with the mocked model already holding `history` sessions (title, messages)
    /// in the workspace, to be imported on startup like a real provider's.
    pub fn start_with(
        files: &[(&str, &str)],
        size: impl Into<Vec2>,
        history: &[(&str, &[(Role, &str)])],
    ) -> Self {
        isolate();
        // Under the target dir, not the system temp dir: imports skip temp dirs.
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("xade-e2e-{}", uuid::Uuid::new_v4()));
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        // The folder as `xade <folder>` resolves it, so history lands in the same workspace.
        let workspace = std::fs::canonicalize(workspace).unwrap();
        for (name, text) in files {
            std::fs::write(workspace.join(name), text).unwrap();
        }
        let providers: Vec<Arc<dyn AgentProvider>> =
            vec![Arc::new(FakeProvider::new(&workspace, history))];
        let (rt, launch) =
            xade::backend_with(&dir.join("data"), providers).expect("backend starts");
        let ui = Harness::builder()
            .with_size(size)
            // Background tasks (git, tree, running polls) wake the UI via `Bridge::send`, so a
            // transition can take more than kittest's default 4 frames to settle.
            .with_max_steps(16)
            // The top bar's combo box has no accessible name yet; that is not what is tested.
            .with_accessibility_check(false)
            .build_eframe(|cc| launch.into_app(cc, Some(workspace)));
        let mut xade = Xade { ui, dir, _rt: rt };
        // The workspace is added by a background task; every flow starts once its session window
        // is up.
        xade.wait_for("the workspace's session window", |ui| {
            ui.query_by_label("🚀 Auto: OFF").is_some()
        });
        xade
    }

    /// Frames until nothing asks for another one right away.
    pub fn settle(&mut self) {
        self.ui.run();
    }

    /// Waits (up to five seconds) for something that arrives from a background task, such as
    /// the file list behind Cmd+P.
    pub fn wait_for(&mut self, what: &str, done: impl Fn(&Harness<'static, App>) -> bool) {
        let start = Instant::now();
        while !done(&self.ui) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(20));
            // Background work may keep requesting repaints while we wait; the loop's 5 s timeout
            // bounds it, so exceeding `max_steps` on one tick is not a failure.
            self.ui.run_ok();
        }
    }

    pub fn press(&mut self, modifiers: Modifiers, key: Key) {
        self.ui.key_press_modifiers(modifiers, key);
        self.settle();
    }

    /// Writes `text` in the chat box of the session window, as a person does: click it, type.
    pub fn write(&mut self, text: &str) {
        self.ui
            .get_by_role(egui::accesskit::Role::MultilineTextInput)
            .click();
        self.settle();
        self.type_text(text);
    }

    /// Like `write`, in the window that shows `label` (when several session windows are open).
    pub fn write_where(&mut self, label: &str, text: &str) {
        let x = self.ui.get_by_label(label).rect().center().x;
        let boxes = self
            .ui
            .get_all_by_role(egui::accesskit::Role::MultilineTextInput);
        boxes
            .into_iter()
            .find(|b| b.rect().x_range().contains(x))
            .expect("a chat box in that window")
            .click();
        self.settle();
        self.type_text(text);
    }

    pub fn type_text(&mut self, text: &str) {
        self.ui.event(Event::Text(text.to_string()));
        self.settle();
    }

    /// Writes a binary file (such as an image) into the workspace folder.
    pub fn write_file(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.dir.join("workspace").join(name), bytes).unwrap();
    }
}

impl Drop for Xade {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A table whose cells are long sentences, as a model writes them.
pub fn wide_table() -> String {
    let cell = "a sentence that is long enough to need several lines in a narrow window ";
    format!(
        "Results:\n\n| Use case | Status | Evidence |\n|---|---|---|\n| {} | PASS | {} |\n| short | NOT COMPLETED | {} |",
        cell.repeat(6),
        cell.repeat(6),
        cell.repeat(3),
    )
}
