//! Bottom status bar: active providers and the git state of the active directory.

use std::time::Instant;

use egui::{Align, Layout, RichText};
use xade_runtime::GitState;

use crate::app::App;
use crate::keymap::{self, Action};
use crate::{icons, theme};

/// Draws the bar; returns whether the Settings button was clicked.
pub fn show(app: &App, ui: &mut egui::Ui) -> bool {
    let mut settings = false;
    let frame = egui::Frame::new()
        .fill(theme::BG_STATUS)
        .inner_margin(egui::Margin::symmetric(12, 4));
    egui::Panel::bottom("status_bar")
        .frame(frame)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                left(app, ui);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    settings = ui
                        .small_button(icons::SETTINGS)
                        .on_hover_text(format!(
                            "Settings ({})",
                            keymap::shortcut_text(Action::OpenSettings)
                        ))
                        .clicked();
                });
            });
        });
    settings
}

/// One dot and name per provider: green when preflight passed, red (error on hover) when not.
fn providers(app: &App, ui: &mut egui::Ui) {
    for p in &app.providers {
        let err = app.home.preflight_error.as_deref();
        let color = if err.is_none() {
            theme::OK
        } else {
            theme::DANGER
        };
        let text = match p.account_label() {
            Some(label) => format!("{} ({label})", p.display_name()),
            None => p.display_name().to_string(),
        };
        ui.label(RichText::new("\u{25CF}").small().color(color));
        ui.label(RichText::new(text).small().color(theme::TEXT_MUTED))
            .on_hover_text(err.unwrap_or("Logged in"));
    }
}

/// The providers, the directory name and its git state.
fn left(app: &App, ui: &mut egui::Ui) {
    let muted = |s: String| RichText::new(s).small().color(theme::TEXT_MUTED);
    providers(app, ui);
    if let Some(first) = app.chord.pending(Instant::now()) {
        let first = keymap::stroke_label(&first, cfg!(target_os = "macos"));
        ui.label(muted(format!("{first} \u{2026}")));
    }
    if let Some(warning) = app.warning_text() {
        ui.label(muted(warning.to_string()));
    }
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
            ..
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
}

/// A provider that reports a fixed account label, for tests.
#[cfg(test)]
pub(crate) struct LabelledProvider(pub &'static str);

#[cfg(test)]
impl xade_core::AgentProvider for LabelledProvider {
    fn id(&self) -> xade_core::ProviderId {
        xade_core::ProviderId::Claude
    }
    fn display_name(&self) -> &'static str {
        "Claude"
    }
    fn models(&self) -> Vec<xade_core::ModelOption> {
        Vec::new()
    }
    fn default_model(&self) -> String {
        String::new()
    }
    fn effort_levels(&self) -> Vec<xade_core::Effort> {
        Vec::new()
    }
    fn preflight(&self) -> Result<(), xade_core::ProviderError> {
        Ok(())
    }
    fn account_label(&self) -> Option<String> {
        Some(self.0.into())
    }
    fn spawn(
        &self,
        _cfg: xade_core::SpawnConfig,
    ) -> Result<xade_core::SessionHandle, xade_core::ProviderError> {
        Err(xade_core::ProviderError::NotInstalled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xade_runtime::git::parse_porcelain_v2;

    /// `(text, rect)` of everything the bar draws for `porcelain` in a directory named `name`.
    fn bar(porcelain: &str, name: &str, width: f32) -> Vec<(String, egui::Rect)> {
        bar_with(porcelain, name, width, None, false)
    }

    /// As `bar`, optionally with a warning and a provider labelled `a-long-account-label`.
    #[allow(clippy::unwrap_used)]
    fn bar_with(
        porcelain: &str,
        name: &str,
        width: f32,
        warning: Option<&str>,
        provider: bool,
    ) -> Vec<(String, egui::Rect)> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-bar-{}", uuid::Uuid::new_v4()));
        let root = dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        app.home.workspaces.push(xade_core::Workspace {
            id: xade_core::WorkspaceId::new(),
            root_path: root.clone(),
            display_name: name.into(),
            last_opened_at: time::OffsetDateTime::now_utc(),
            is_pinned: false,
        });
        app.git.insert(root, parse_porcelain_v2(porcelain));
        if provider {
            app.providers = vec![std::sync::Arc::new(LabelledProvider(
                "a-long-account-label",
            ))];
        }
        if let Some(w) = warning {
            app.warn(w);
        }
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 760.0),
            )),
            ..Default::default()
        };
        let run = || {
            let mut out = ctx.run_ui(input(), |ui| {
                show(&app, ui);
            });
            out.textures_delta.clear();
            out
        };
        run();
        let out = run();
        let _ = std::fs::remove_dir_all(&dir);
        fn walk(s: &egui::Shape, v: &mut Vec<(String, egui::Rect)>) {
            match s {
                egui::Shape::Text(t) => v.push((
                    t.galley.text().to_string(),
                    t.galley.rect.translate(t.pos.to_vec2()),
                )),
                egui::Shape::Vec(l) => l.iter().for_each(|s| walk(s, v)),
                _ => {}
            }
        }
        let mut v = Vec::new();
        out.shapes.iter().for_each(|c| walk(&c.shape, &mut v));
        v
    }

    /// The branch (or detached head) and the change counts both show, branch first, whatever
    /// the repo state and however long the directory name.
    #[test]
    fn branch_shows_next_to_the_change_counts() {
        const OID: &str = "# branch.oid 0123456789abcdef0123456789abcdef01234567\n";
        let cases = [
            ("# branch.oid (initial)\n# branch.head main\n? a\n? b\n? c\n", "git: main"),
            (
                &format!("{OID}# branch.head feat/x\n# branch.upstream origin/feat/x\n# branch.ab +1 -0\n? a\n? b\n? c\n"),
                "git: feat/x",
            ),
            (&format!("{OID}# branch.head (detached)\n? a\n? b\n? c\n"), "git: detached @ 0123456"),
        ];
        let long = "a-rather-long-workspace-directory-name-for-a-checkout";
        for (porcelain, head) in cases {
            for name in ["w", long] {
                let texts = bar(porcelain, name, crate::MIN_WINDOW_SIZE[0]);
                let at = |needle: &str| texts.iter().find(|(t, _)| t == needle).map(|(_, r)| *r);
                let (h, c) = (at(head), at("3 untracked"));
                assert!(
                    h.is_some() && c.is_some(),
                    "{head:?} / counts missing: {texts:?}"
                );
                assert!(h.unwrap().max.x <= c.unwrap().min.x, "branch after counts");
                assert!(
                    c.unwrap().max.x <= crate::MIN_WINDOW_SIZE[0],
                    "counts clipped"
                );
            }
        }
    }

    #[test]
    fn settings_button_sits_at_the_right_edge() {
        let w = crate::MIN_WINDOW_SIZE[0];
        let texts = bar("# branch.oid x\n# branch.head main\n", "w", w);
        let (_, gear) = texts
            .iter()
            .find(|(t, _)| t == icons::SETTINGS)
            .expect("gear drawn");
        assert!(gear.max.x > w - 40.0 && gear.max.x <= w, "{gear:?}");
    }

    #[test]
    fn provider_label_git_info_warning_and_gear_do_not_overlap() {
        let w = crate::MIN_WINDOW_SIZE[0];
        let long = "a-rather-long-workspace-directory-name-for-a-checkout";
        let porcelain = "# branch.oid x\n# branch.head feat/some-branch\n? a\n? b\n";
        let texts = bar_with(
            porcelain,
            long,
            w,
            Some("settings.json: invalid value on line 12"),
            true,
        );
        let provider = texts
            .iter()
            .find(|(t, _)| t == "Claude (a-long-account-label)")
            .expect("provider label drawn");
        let mut rects: Vec<_> = texts.iter().filter(|(t, _)| !t.trim().is_empty()).collect();
        assert!(rects.iter().any(|(t, _)| t.contains("settings.json")));
        assert!(rects.iter().any(|(t, _)| t.starts_with("git: ")));
        rects.sort_by(|a, b| a.1.min.x.total_cmp(&b.1.min.x));
        assert_eq!(rects[0].0, "\u{25CF}", "dot leads the bar");
        for pair in rects.windows(2) {
            assert!(
                pair[0].1.max.x <= pair[1].1.min.x + 0.5,
                "{:?} overlaps {:?}",
                pair[0],
                pair[1]
            );
        }
        let gear = rects.last().unwrap();
        assert_eq!(gear.0, icons::SETTINGS);
        assert!(gear.1.max.x <= w, "{gear:?}");
        assert!(provider.1.min.x < 40.0, "{provider:?}");
    }
}
