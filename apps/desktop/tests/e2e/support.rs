#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Once;
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers, Vec2};
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
    /// Starts the app on a new workspace with `files` in it, in a window of `size`.
    pub fn start(files: &[(&str, &str)], size: impl Into<Vec2>) -> Self {
        isolate();
        let dir = std::env::temp_dir().join(format!("xade-e2e-{}", uuid::Uuid::new_v4()));
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        for (name, text) in files {
            std::fs::write(workspace.join(name), text).unwrap();
        }
        let (rt, launch) = xade::backend(&dir.join("data")).expect("backend starts");
        let ui = Harness::builder()
            .with_size(size)
            // The top bar's combo box has no accessible name yet; that is not what is tested.
            .with_accessibility_check(false)
            .build_eframe(|cc| launch.into_app(cc, Some(workspace)));
        let mut xade = Xade { ui, dir, _rt: rt };
        xade.settle();
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
            self.ui.run();
        }
    }

    pub fn press(&mut self, modifiers: Modifiers, key: Key) {
        self.ui.key_press_modifiers(modifiers, key);
        self.settle();
    }

    pub fn type_text(&mut self, text: &str) {
        self.ui.event(Event::Text(text.to_string()));
        self.settle();
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
