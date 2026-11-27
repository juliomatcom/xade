//! Help dialog: answers to the questions the UI does not answer by itself, in two columns, each
//! with the button that does what it describes.

use std::path::PathBuf;

use egui::RichText;

use crate::app::App;
use crate::keymap::{self, Action};
use crate::{icons, logging, theme};

const ISSUES_URL: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/issues/new");
/// Width of one column; the dialog is two of them plus the gap.
const COLUMN_WIDTH: f32 = 340.0;
const COLUMN_GAP: f32 = 20.0;

/// What a button in the dialog asks for; applied once the dialog has drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpAction {
    ReportBug,
    OpenLogs,
    OpenDataFolder,
    OpenSettings,
    RestoreSettings,
    Shortcuts,
}

impl HelpAction {
    fn label(self) -> String {
        match self {
            Self::ReportBug => icons::with(icons::BUG, "Report a bug"),
            Self::OpenLogs => icons::with(icons::FOLDER_OPEN, "Open logs folder"),
            Self::OpenDataFolder => icons::with(icons::FOLDER_OPEN, "Open data folder"),
            Self::OpenSettings => icons::with(icons::SETTINGS, "Open settings"),
            Self::RestoreSettings => icons::with(icons::RETRY, "Restore defaults"),
            Self::Shortcuts => icons::with(icons::SHORTCUTS, "Show shortcuts"),
        }
    }
}

struct Entry {
    question: &'static str,
    answer: String,
    actions: &'static [HelpAction],
}

/// `text` followed by ` (<shortcut>)` when `action` has one.
fn with_key(text: &str, action: Action) -> String {
    let key = keymap::shortcut_text(action);
    if key.is_empty() {
        text.to_string()
    } else {
        format!("{text} ({key})")
    }
}

/// The entries, left column first; `data` is the data folder, shown where it helps.
fn entries(data: &str) -> (Vec<Entry>, Vec<Entry>) {
    let left = vec![
        Entry {
            question: "How do I report a bug?",
            answer: "Open an issue on GitHub with what you did, what you expected and the last \
                     lines of the log. When Xade shows an error, its Open GitHub issue button \
                     fills the issue in for you, with your home folder and emails masked."
                .into(),
            actions: &[HelpAction::ReportBug, HelpAction::OpenLogs],
        },
        Entry {
            question: "Where are the logs?",
            answer: format!(
                "In {data}/logs, one file per day, the last 7 kept. A crash also writes a crash-*.txt file there."
            ),
            actions: &[HelpAction::OpenLogs],
        },
        Entry {
            question: "Where does Xade keep my data?",
            answer: format!(
                "Everything lives in {data}: workspace.db (sessions and messages), \
                 settings.json, windows.txt (windows to reopen) and the fork prompts. Set \
                 XADE_DATA_DIR to use another folder."
            ),
            actions: &[HelpAction::OpenDataFolder],
        },
        Entry {
            question: "How do I delete a session?",
            answer: "Use the trash button at the bottom right of its card on Home. It deletes \
                     the session and the CLI's own copy of it, and can't be undone. Only \
                     sessions started in Xade have one; sessions imported from Claude Code or \
                     Codex stay as they are in their CLI."
                .into(),
            actions: &[],
        },
        Entry {
            question: "How do I remove a folder from the sidebar?",
            answer: "Right-click it and choose Close workspace. Its sessions are kept and it \
                     comes back when you open one of them."
                .into(),
            actions: &[],
        },
        Entry {
            question: "How do I reset my settings?",
            answer: format!(
                "Restore defaults rewrites settings.json and keeps the old file as \
                 settings.json.bak; right-clicking the gear at the top right does the same. \
                 {}: changes apply as soon as you save.",
                with_key("Open settings", Action::OpenSettings)
            ),
            actions: &[HelpAction::OpenSettings, HelpAction::RestoreSettings],
        },
    ];
    let right = vec![
        Entry {
            question: "What does \u{201c}Cache expired\u{201d} mean?",
            answer: "A session idle for over an hour has lost its prompt cache, so the next \
                     message resends the whole conversation at full price. Changing the model \
                     or thinking effort does the same."
                .into(),
            actions: &[],
        },
        Entry {
            question: "Why does a session have a yellow dot?",
            answer: "It is open in another process, such as the CLI in a terminal; the dot \
                     pulses while that one is working. Xade asks before opening it, because \
                     sending from both places mixes up the conversation."
                .into(),
            actions: &[],
        },
        Entry {
            question: "My terminal sessions don't show up.",
            answer: "Xade imports Claude Code, Codex and Antigravity sessions at startup. Click \
                     Refresh sessions at the bottom of the sidebar to import new ones now."
                .into(),
            actions: &[],
        },
        Entry {
            question: "Can I send while the agent is working?",
            answer: format!(
                "Yes. By default the message waits in a queue until the turn ends; \
                 send_while_running in settings makes it steer the running turn instead. {} \
                 does the other one.",
                with_key("Sending the other way", Action::SendOther)
            ),
            actions: &[],
        },
        Entry {
            question: "What does Auto do?",
            answer: format!(
                "It approves the session's tool requests, code changes included, without \
                 asking. {} turns it off again.",
                with_key("The Auto button", Action::ToggleAuto)
            ),
            actions: &[],
        },
        Entry {
            question: "Where are the keyboard shortcuts?",
            answer: "Press ? anywhere outside a text box. Every shortcut can be changed in the \
                     shortcuts block of settings.json."
                .into(),
            actions: &[HelpAction::Shortcuts],
        },
    ];
    (left, right)
}

/// One question with its answer and buttons; returns the button clicked.
fn entry(ui: &mut egui::Ui, e: &Entry) -> Option<HelpAction> {
    let mut clicked = None;
    ui.label(RichText::new(e.question).strong());
    ui.add_space(2.0);
    ui.label(RichText::new(&e.answer).color(theme::text_muted()));
    if !e.actions.is_empty() {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            for a in e.actions {
                if ui.small_button(a.label()).clicked() {
                    clicked = Some(*a);
                }
            }
        });
    }
    ui.add_space(14.0);
    clicked
}

/// The data folder: the parent of the logs folder.
fn data_dir(app: &App) -> Option<PathBuf> {
    app.log_dir.as_deref()?.parent().map(PathBuf::from)
}

/// Draws the dialog while `app.help_open`; Close, Esc or a click outside closes it.
pub fn show(app: &mut App, ctx: &egui::Context) {
    if !app.help_open {
        return;
    }
    let data = data_dir(app).map_or_else(
        || "the data folder".to_string(),
        |d| crate::errors::redact(&d.display().to_string(), dirs::home_dir().as_deref()),
    );
    let (left, right) = entries(&data);
    let mut close = false;
    let mut action = None;
    let resp = crate::widgets::modal(ctx, egui::Id::unique("help")).show(ctx, |ui| {
        let width = COLUMN_WIDTH * 2.0 + COLUMN_GAP;
        ui.set_width(width);
        ui.label(
            RichText::new(icons::with(icons::HELP, "Help"))
                .strong()
                .size(20.0),
        );
        ui.add_space(10.0);
        let max_height = (ctx.content_rect().height() - 160.0).max(200.0);
        egui::ScrollArea::vertical()
            .max_height(max_height)
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    for (i, column) in [&left, &right].into_iter().enumerate() {
                        if i > 0 {
                            ui.add_space(COLUMN_GAP);
                        }
                        ui.vertical(|ui| {
                            ui.set_width(COLUMN_WIDTH);
                            for e in column {
                                if let Some(a) = entry(ui, e) {
                                    action = Some(a);
                                }
                            }
                        });
                    }
                });
            });
        ui.add_space(4.0);
        close = ui.button("Close").clicked();
    });
    if let Some(a) = action {
        apply(app, ctx, a);
    }
    if close || resp.should_close() {
        app.help_open = false;
    }
}

fn apply(app: &mut App, ctx: &egui::Context, action: HelpAction) {
    match action {
        HelpAction::ReportBug => ctx.open_url(egui::OpenUrl::new_tab(ISSUES_URL)),
        HelpAction::OpenLogs => {
            if let Some(dir) = &app.log_dir {
                logging::open_dir(dir);
            }
        }
        HelpAction::OpenDataFolder => {
            if let Some(dir) = data_dir(app) {
                logging::open_dir(&dir);
            }
        }
        HelpAction::OpenSettings => {
            app.help_open = false;
            app.open_settings();
        }
        HelpAction::RestoreSettings => app.restore_default_settings(),
        HelpAction::Shortcuts => {
            app.help_open = false;
            app.keymaps_open = true;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_issue_link_points_at_the_repository() {
        assert!(ISSUES_URL.starts_with("https://github.com/"));
        assert!(ISSUES_URL.ends_with("/issues/new"));
    }

    #[test]
    fn answers_name_the_data_folder_and_both_columns_are_filled() {
        let (left, right) = entries("~/.local/share/xade");
        assert!(!left.is_empty() && !right.is_empty());
        let logs = left.iter().find(|e| e.question.contains("logs")).unwrap();
        assert!(logs.answer.contains("~/.local/share/xade/logs"));
    }

    #[test]
    fn draws_two_columns_and_closes_on_escape() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-help-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        app.help_open = true;
        let frame = |app: &mut App, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                events,
                ..Default::default()
            };
            ctx.run_pass(input, |ctx| show(app, ctx))
                .drop_without_applying_deltas();
        };
        frame(&mut app, vec![]);
        frame(&mut app, vec![]);
        assert!(app.help_open);
        let esc = egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        frame(&mut app, vec![esc]);
        frame(&mut app, vec![]);
        assert!(!app.help_open);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
