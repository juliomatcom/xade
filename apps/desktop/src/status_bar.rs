//! Bottom status bar: git state of the active directory.

use egui::RichText;
use xade_runtime::GitState;

use crate::app::App;
use crate::theme;

pub fn show(app: &App, ui: &mut egui::Ui) {
    let frame = egui::Frame::new()
        .fill(theme::BG_APP)
        .inner_margin(egui::Margin::symmetric(12, 4));
    egui::Panel::bottom("status_bar")
        .frame(frame)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let muted = |s: String| RichText::new(s).small().color(theme::TEXT_MUTED);
                let Some(dir) = app.active_dir() else {
                    ui.label(muted("No directory".into()));
                    return;
                };
                let name = dir.file_name().map_or_else(
                    || dir.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                ui.label(muted(name));
                match app.git.get(&dir) {
                    None => {}
                    Some(GitState::NotARepo) => {
                        ui.label(muted("Not a git repository".into()));
                    }
                    Some(GitState::GitMissing) => {
                        ui.label(muted("git not found".into()));
                    }
                    Some(GitState::Repo {
                        branch,
                        detached_sha,
                        has_upstream,
                        ahead,
                        behind,
                        staged,
                        modified,
                        untracked,
                    }) => {
                        let head = match (branch, detached_sha) {
                            (Some(b), _) => b.clone(),
                            (None, Some(sha)) => format!("detached @ {sha}"),
                            (None, None) => "unknown".into(),
                        };
                        ui.label(muted(format!("git: {head}")));
                        if *has_upstream {
                            ui.label(muted(format!("\u{2191}{ahead} \u{2193}{behind}")));
                        }
                        let changes = [
                            (*staged, "staged"),
                            (*modified, "modified"),
                            (*untracked, "untracked"),
                        ]
                        .iter()
                        .filter(|(n, _)| *n > 0)
                        .map(|(n, l)| format!("{n} {l}"))
                        .collect::<Vec<_>>();
                        if changes.is_empty() {
                            ui.label(muted("clean".into()));
                        } else {
                            ui.label(muted(changes.join("  ")));
                        }
                    }
                }
            });
        });
}
