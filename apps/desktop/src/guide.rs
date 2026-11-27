//! The "Get started" window, opened at startup when no agent CLI is usable and from the menu: how
//! to install and sign in to each one, then the way to a first session.

use egui::{Align, Layout, Margin, Rect, RichText};
use xade_core::{ProviderError, ProviderId};

use crate::app::App;
use crate::chrome;
use crate::conversation;
use crate::icons;
use crate::session_view::ViewOut;
use crate::theme;

const CONTENT_WIDTH: f32 = 620.0;
/// Width of a step's name, so the commands line up.
const STEP_WIDTH: f32 = 64.0;

/// The window's geometry; what it shows is read from the preflights every frame.
#[derive(Default)]
pub struct GuideVm {
    pub collapsed: bool,
    pub window_rect: Option<Rect>,
    pub apply_rect: Option<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuideAction {
    CheckAgain,
    OpenFolder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Checking,
    Ready,
    NotInstalled,
    NotSignedIn,
    Failed(String),
}

/// One CLI and where it stands.
pub struct Row {
    id: ProviderId,
    state: State,
}

struct Setup {
    name: &'static str,
    install: &'static str,
    sign_in: &'static str,
    docs: &'static str,
}

/// Each vendor's own install and sign-in commands (macOS and Linux).
fn setup(id: ProviderId) -> Setup {
    match id {
        ProviderId::Claude => Setup {
            name: "Claude Code",
            install: "curl -fsSL https://claude.ai/install.sh | bash",
            sign_in: "claude auth login",
            docs: "https://code.claude.com/docs/en/setup",
        },
        ProviderId::Codex => Setup {
            name: "Codex",
            install: "curl -fsSL https://chatgpt.com/codex/install.sh | sh",
            sign_in: "codex login",
            docs: "https://developers.openai.com/codex",
        },
        ProviderId::Antigravity => Setup {
            name: "Antigravity",
            install: "curl -fsSL https://antigravity.google/cli/install.sh | bash",
            sign_in: "agy",
            docs: "https://antigravity.google/docs/install?tab=cli",
        },
    }
}

/// Every registered provider, in the app's order.
pub fn rows(app: &App) -> Vec<Row> {
    app.providers
        .iter()
        .map(|p| {
            let id = p.id();
            let state = if app.home.pending.contains(&id) {
                State::Checking
            } else {
                match app.home.preflight.get(&id) {
                    None | Some(Ok(())) => State::Ready,
                    Some(Err(ProviderError::NotInstalled)) => State::NotInstalled,
                    Some(Err(ProviderError::NotLoggedIn)) => State::NotSignedIn,
                    Some(Err(e)) => State::Failed(e.to_string()),
                }
            };
            Row { id, state }
        })
        .collect()
}

pub fn show(
    ui: &mut egui::Ui,
    vm: &GuideVm,
    rows: &[Row],
    maximized: bool,
    focused: bool,
) -> ViewOut {
    let mut out = ViewOut::default();
    chrome::header(
        ui,
        chrome::HeaderState {
            maximized,
            collapsed: vm.collapsed,
            focused,
        },
        "Close guide",
        &mut out,
        &|ui| chrome::header_badge(ui, "Guide"),
        |ui| {
            ui.add(egui::Label::new(RichText::new("Get started").strong()).truncate());
        },
    );
    if vm.collapsed {
        return out;
    }
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Frame::new()
                .inner_margin(Margin::same(16))
                .show(ui, |ui| {
                    let width = ui.available_width().min(CONTENT_WIDTH);
                    let indent = (ui.available_width() - width) / 2.0;
                    ui.horizontal(|ui| {
                        ui.add_space(indent);
                        ui.vertical(|ui| {
                            ui.set_width(width);
                            out.guide = body(ui, rows);
                        });
                    });
                });
        });
    out
}

fn body(ui: &mut egui::Ui, rows: &[Row]) -> Option<GuideAction> {
    let mut action = None;
    let ready: Vec<_> = rows
        .iter()
        .filter(|r| r.state == State::Ready)
        .map(|r| setup(r.id).name)
        .collect();
    ui.label(RichText::new("Welcome to Xade").strong().size(20.0));
    ui.label(
        "Xade runs the coding agent CLIs installed on this computer, with your own login. \
         Set up any one of these in a terminal.",
    );
    ui.add_space(12.0);
    for row in rows {
        card(ui, row);
        ui.add_space(8.0);
    }
    ui.add_space(4.0);
    if !ready.is_empty() {
        ui.label(
            RichText::new(format!(
                "{} Ready: {}. Open a project folder to start a session.",
                icons::APPROVE,
                ready.join(", ")
            ))
            .color(theme::ok()),
        );
    }
    ui.horizontal(|ui| {
        if !ready.is_empty()
            && ui
                .button(icons::with(icons::ADD_DIR, "Open a folder"))
                .clicked()
        {
            action = Some(GuideAction::OpenFolder);
        }
        if ready.len() < rows.len() {
            let idle = rows.iter().all(|r| r.state != State::Checking);
            let button = egui::Button::new(icons::with(icons::RETRY, "Check again"));
            if ui.add_enabled(idle, button).clicked() {
                action = Some(GuideAction::CheckAgain);
            }
        }
    });
    if rows.iter().any(|r| r.state == State::NotInstalled) {
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Installed, but not found? Start Xade from your terminal with `xade .` so it \
                 sees your PATH.",
            )
            .small()
            .color(theme::text_muted()),
        );
    }
    action
}

fn card(ui: &mut egui::Ui, row: &Row) {
    let setup = setup(row.id);
    let (status, color) = match &row.state {
        State::Checking => ("Checking\u{2026}", theme::text_muted()),
        State::Ready => ("Ready", theme::ok()),
        State::NotInstalled => ("Not installed", theme::text_muted()),
        State::NotSignedIn => ("Not signed in", theme::warn()),
        State::Failed(_) => ("Not working", theme::danger()),
    };
    egui::Frame::new()
        .fill(theme::bg_raised())
        .stroke(egui::Stroke::new(1.0, theme::border()))
        .corner_radius(6)
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                if let Some(tex) = conversation::cli_symbol(ui.ctx(), row.id) {
                    ui.add(
                        egui::Image::new(&tex)
                            .tint(conversation::symbol_tint(row.id))
                            .fit_to_exact_size(egui::vec2(16.0, 16.0)),
                    );
                }
                ui.label(RichText::new(setup.name).strong());
                ui.label(RichText::new(status).small().color(color));
                ui.hyperlink_to(RichText::new("Docs").small(), setup.docs);
            });
            match &row.state {
                State::Checking | State::Ready => {}
                State::NotInstalled => {
                    command(ui, "Install", setup.install);
                    command(ui, "Sign in", setup.sign_in);
                }
                State::NotSignedIn => command(ui, "Sign in", setup.sign_in),
                State::Failed(error) => {
                    ui.label(RichText::new(error).small().color(theme::danger()));
                }
            }
        });
}

/// A step's terminal command with its Copy button.
fn command(ui: &mut egui::Ui, step: &str, text: &str) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(STEP_WIDTH, ui.spacing().interact_size.y),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.set_min_width(STEP_WIDTH);
                ui.label(RichText::new(step).small().color(theme::text_muted()));
            },
        );
        egui::Frame::new()
            .fill(theme::bg_terminal())
            .corner_radius(6)
            .inner_margin(Margin::symmetric(8, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button(icons::COPY).on_hover_text("Copy").clicked() {
                            ui.ctx().copy_text(text.to_owned());
                        }
                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                            ui.add(egui::Label::new(RichText::new(text).monospace()).wrap());
                        });
                    });
                });
            });
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Every text a headless frame of the guide paints.
    fn painted(rows: &[Row]) -> Vec<String> {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 900.0),
            )),
            ..Default::default()
        };
        let vm = GuideVm::default();
        let draw = |ui: &mut egui::Ui| {
            show(ui, &vm, rows, false, true);
        };
        ctx.run_ui(input(), draw).textures_delta.clear();
        let mut out = ctx.run_ui(input(), draw);
        out.textures_delta.clear();
        fn walk(s: &egui::Shape, t: &mut Vec<String>) {
            match s {
                egui::Shape::Text(x) => t.push(x.galley.text().to_string()),
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, t)),
                _ => {}
            }
        }
        let mut texts = Vec::new();
        out.shapes.iter().for_each(|c| walk(&c.shape, &mut texts));
        texts
    }

    fn row(id: ProviderId, state: State) -> Row {
        Row { id, state }
    }

    #[test]
    fn each_state_shows_only_the_steps_it_still_needs() {
        let claude = setup(ProviderId::Claude);
        let missing = painted(&[row(ProviderId::Claude, State::NotInstalled)]);
        assert!(missing.iter().any(|t| t == claude.install), "{missing:?}");
        assert!(missing.iter().any(|t| t == claude.sign_in));
        assert!(missing.iter().any(|t| t.ends_with("Check again")));

        let signed_out = painted(&[row(ProviderId::Claude, State::NotSignedIn)]);
        assert!(!signed_out.iter().any(|t| t == claude.install));
        assert!(signed_out.iter().any(|t| t == claude.sign_in));
        assert!(!signed_out.iter().any(|t| t.contains("not found?")));

        let ready = painted(&[
            row(ProviderId::Claude, State::Ready),
            row(ProviderId::Codex, State::NotInstalled),
        ]);
        assert!(ready.iter().any(|t| t.ends_with("Open a folder")));
        assert!(ready.iter().any(|t| t.ends_with("Check again")));
        assert!(!ready.iter().any(|t| t == claude.sign_in));

        let all_ready = painted(&[row(ProviderId::Claude, State::Ready)]);
        assert!(!all_ready.iter().any(|t| t.ends_with("Check again")));
    }
}
