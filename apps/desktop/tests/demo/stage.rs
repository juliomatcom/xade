//! The camera: steps the app one video frame at a time, plays the scheduled model events and
//! pipes each rendered frame to ffmpeg.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers};
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use xade::App;
use xade_core::{ProviderEvent, UsageTotals};

use crate::provider::Stage;

pub const FPS: u32 = 30;
/// Logical window size; frames are rendered at `SCALE` times this.
pub const SIZE: [f32; 2] = [1480.0, 810.0];
pub const SCALE: f32 = 2.0;

enum Cue {
    Model(&'static str, ProviderEvent),
    /// Writes a file into the workspace, so the turn's diff and git status are real.
    File(&'static str, &'static str),
}

pub struct Camera {
    pub ui: Harness<'static, App>,
    pub stage: Arc<Stage>,
    workspace: PathBuf,
    cues: BTreeMap<u32, Vec<Cue>>,
    frame: u32,
    encoder: Option<Child>,
    tools: u32,
}

impl Camera {
    pub fn new(ui: Harness<'static, App>, stage: Arc<Stage>, workspace: PathBuf) -> Self {
        Self {
            ui,
            stage,
            workspace,
            cues: BTreeMap::new(),
            frame: 0,
            encoder: None,
            tools: 0,
        }
    }

    /// Starts piping every later frame into a near-lossless master video.
    pub fn roll(&mut self, master: &Path) {
        let [w, h] = SIZE.map(|v| (v * SCALE) as u32);
        let child = Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-f", "rawvideo"])
            .args(["-pix_fmt", "rgba", "-s", &format!("{w}x{h}")])
            .args(["-r", &FPS.to_string(), "-i", "-"])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "8"])
            .args(["-pix_fmt", "yuv444p"])
            .arg(master)
            .stdin(Stdio::piped())
            .spawn()
            .expect("ffmpeg is installed");
        self.encoder = Some(child);
    }

    pub fn cut(&mut self) {
        if let Some(mut child) = self.encoder.take() {
            drop(child.stdin.take());
            assert!(child.wait().unwrap().success(), "ffmpeg failed");
        }
    }

    pub fn now(&self) -> u32 {
        self.frame
    }

    /// Plays `frames` video frames.
    pub fn run(&mut self, frames: u32) {
        for _ in 0..frames {
            for cue in self.cues.remove(&self.frame).unwrap_or_default() {
                match cue {
                    Cue::Model(model, event) => self.stage.emit(model, event),
                    Cue::File(name, text) => {
                        let path = self.workspace.join(name);
                        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                        std::fs::write(path, text).unwrap();
                    }
                }
            }
            // Gives the session actors time to carry the events to the UI's channel.
            std::thread::sleep(Duration::from_millis(12));
            self.ui.step();
            if let Some(child) = &mut self.encoder {
                let image = self.ui.render().expect("render a frame");
                child.stdin.as_mut().unwrap().write_all(&image).unwrap();
            }
            self.frame += 1;
        }
    }

    pub fn run_until(&mut self, frame: u32) {
        self.run(frame.saturating_sub(self.frame));
    }

    /// Plays frames until `done`; the story's next cues depend on what the app did.
    pub fn run_until_ui(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let start = Instant::now();
        while !done(self) {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "timed out waiting for {what}"
            );
            self.run(1);
        }
    }

    pub fn await_prompt(&mut self, model: &'static str, count: usize) {
        self.run_until_ui(model, |c| c.stage.prompts(model) >= count);
    }

    pub fn still(&mut self, path: &Path) {
        let image = self.ui.render().expect("render a still");
        image.save(path).expect("save the still");
    }

    fn cue(&mut self, at: u32, cue: Cue) {
        // A cue in the past would never play.
        self.cues.entry(at.max(self.frame)).or_default().push(cue);
    }

    // Model cues. Each takes its start frame and returns the frame it ends on.

    pub fn think(&mut self, at: u32, model: &'static str, frames: u32, tokens: u64) -> u32 {
        for i in (0..frames).step_by(3) {
            let estimated_tokens = tokens * u64::from(i + 3) / u64::from(frames + 3);
            self.cue(
                at + i,
                Cue::Model(model, ProviderEvent::ThinkingProgress { estimated_tokens }),
            );
        }
        at + frames
    }

    /// Streams `text` a few characters per frame, as a model does.
    pub fn stream(&mut self, at: u32, model: &'static str, text: &str, per_frame: usize) -> u32 {
        let chars: Vec<char> = text.chars().collect();
        let mut end = at;
        for (i, chunk) in chars.chunks(per_frame).enumerate() {
            end = at + i as u32;
            let delta = ProviderEvent::TextDelta(chunk.iter().collect());
            self.cue(end, Cue::Model(model, delta));
        }
        end + 1
    }

    /// Finalizes a block: `text`, plus a `<broadcast>` to the team when given.
    pub fn say(
        &mut self,
        at: u32,
        model: &'static str,
        text: &str,
        broadcast: Option<&str>,
    ) -> u32 {
        let mut text = text.to_string();
        if let Some(b) = broadcast {
            text.push_str(&format!("\n\n<broadcast>\n{b}\n</broadcast>"));
        }
        self.cue(at, Cue::Model(model, ProviderEvent::AssistantText { text }));
        at + 1
    }

    /// A tool call that runs for `frames` and then returns `output`.
    pub fn tool(
        &mut self,
        at: u32,
        model: &'static str,
        name: &str,
        input_json: String,
        output: &str,
        frames: u32,
    ) -> u32 {
        self.tools += 1;
        let id = format!("tool-{}", self.tools);
        self.cue(
            at,
            Cue::Model(
                model,
                ProviderEvent::ToolUse {
                    tool_use_id: id.clone(),
                    name: name.into(),
                    input_json,
                },
            ),
        );
        self.cue(
            at + frames,
            Cue::Model(
                model,
                ProviderEvent::ToolResult {
                    tool_use_id: id,
                    output: output.into(),
                    is_error: false,
                    structured_json: None,
                },
            ),
        );
        at + frames + 2
    }

    /// An `Edit` or `Write` call that really changes the file.
    pub fn edit(
        &mut self,
        at: u32,
        model: &'static str,
        file: &'static str,
        text: &'static str,
        frames: u32,
    ) -> u32 {
        self.cue(at + frames - 1, Cue::File(file, text));
        let input = serde_json::json!({ "file_path": file, "content": text }).to_string();
        self.tool(at, model, "Write", input, "ok", frames)
    }

    pub fn done(&mut self, at: u32, model: &'static str, context: u64) -> u32 {
        let usage = UsageTotals {
            context_tokens_used: context,
            context_window_max: 200_000,
            input_tokens: context * 6,
            output_tokens: context / 9,
            cache_read_tokens: context * 5,
            cost_usd: 0.0,
        };
        self.cue(
            at,
            Cue::Model(
                model,
                ProviderEvent::TurnComplete {
                    usage,
                    is_error: false,
                    interrupted: false,
                },
            ),
        );
        at + 1
    }

    // A person at the keyboard.

    pub fn press(&mut self, modifiers: Modifiers, key: Key) {
        self.ui.key_press_modifiers(modifiers, key);
        self.run(3);
    }

    pub fn click(&mut self, label: &str) {
        self.ui.get_by_label(label).click();
        self.run(3);
        self.ui.remove_cursor();
    }

    /// Clicks the multi-session tab `label`, found on the same row as the "Team" tab.
    pub fn tab(&mut self, label: &str) {
        let y = self.ui.get_by_label("Team").rect().center().y;
        self.ui
            .get_all_by_label(label)
            .find(|n| n.rect().y_range().contains(y))
            .expect("the tab")
            .click();
        self.run(3);
        self.ui.remove_cursor();
    }

    /// Clicks the chat box in the same column as `label`.
    pub fn focus_chat(&mut self, label: &str) {
        let x = self.ui.get_by_label(label).rect().center().x;
        self.ui
            .get_all_by_role(egui::accesskit::Role::MultilineTextInput)
            .find(|b| b.rect().x_range().contains(x))
            .expect("a chat box in that window")
            .click();
        self.run(3);
        self.ui.remove_cursor();
    }

    pub fn type_text(&mut self, text: &str, per_frame: usize) {
        let chars: Vec<char> = text.chars().collect();
        for chunk in chars.chunks(per_frame) {
            self.ui.event(Event::Text(chunk.iter().collect()));
            self.run(1);
        }
    }
}
