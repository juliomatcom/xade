//! Bottom status bar: active providers and the git state of the active directory.

use std::time::{Duration, Instant};

use egui::{Align, Layout, RichText};
use xade_runtime::GitState;

use crate::app::App;
use crate::keymap;
use crate::theme;

/// Width reserved for the FPS text, wide enough for "~999+ fps" in any font.
const FPS_SLOT_WIDTH: f32 = 64.0;

/// A gap longer than this between frames means the UI was idle, not animating.
const IDLE_GAP: Duration = Duration::from_millis(250);

/// The shown number changes at most this often, so it can be read.
const FPS_REFRESH: Duration = Duration::from_millis(500);

/// Frame rate readout. Xade only repaints on demand, so an idle UI has no rate: while frames
/// follow each other it shows the measured rate, otherwise the rate it could sustain
/// (1 / the last frame's CPU time, prefixed "~").
pub struct FpsMeter {
    last_start: Option<Instant>,
    /// Gap between the last two frame starts.
    gap: Duration,
    /// CPU time of the last frame, smoothed.
    work: Duration,
    started: Option<Instant>,
    /// The text on screen and when it was set; see `FPS_REFRESH`.
    shown: String,
    shown_at: Option<Instant>,
}

impl FpsMeter {
    pub fn new() -> Self {
        FpsMeter {
            last_start: None,
            gap: IDLE_GAP,
            work: Duration::ZERO,
            started: None,
            shown: String::new(),
            shown_at: None,
        }
    }

    pub fn frame_start(&mut self, now: Instant) {
        if let Some(prev) = self.last_start {
            self.gap = now.saturating_duration_since(prev);
        }
        self.last_start = Some(now);
        self.started = Some(now);
    }

    pub fn frame_end(&mut self, now: Instant) {
        let Some(start) = self.started.take() else {
            return;
        };
        let took = now.saturating_duration_since(start);
        self.work = if self.work.is_zero() {
            took
        } else {
            self.work.mul_f32(0.8) + took.mul_f32(0.2)
        };
        if self
            .shown_at
            .is_none_or(|at| now.saturating_duration_since(at) >= FPS_REFRESH)
        {
            self.shown = self.current();
            self.shown_at = Some(now);
        }
    }

    /// "120 fps" while animating, "~900 fps" (capacity) when idle; empty before a frame ran.
    pub fn label(&self) -> String {
        self.shown.clone()
    }

    fn current(&self) -> String {
        if self.last_start.is_none() {
            return String::new();
        }
        if self.gap < IDLE_GAP {
            return format!("{} fps", fps_text(1.0 / self.gap.as_secs_f32().max(1e-4)));
        }
        if self.work.is_zero() {
            return String::new();
        }
        format!("~{} fps", fps_text(1.0 / self.work.as_secs_f32().max(1e-4)))
    }
}

/// At most three digits; anything above 999 reads "999+".
fn fps_text(fps: f32) -> String {
    let n = fps.round();
    if n > 999.0 {
        "999+".into()
    } else {
        format!("{n}")
    }
}

pub fn show(app: &App, ui: &mut egui::Ui) {
    let frame = egui::Frame::new()
        .fill(theme::bg_status())
        .inner_margin(egui::Margin::symmetric(12, 4));
    egui::Panel::bottom("status_bar")
        .frame(frame)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                left(app, ui);
                if app.config.show_fps {
                    // Rightmost, in a fixed-width slot so the number never shifts anything.
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(FPS_SLOT_WIDTH, ui.spacing().interact_size.y),
                            Layout::right_to_left(Align::Center),
                            |ui| {
                                ui.set_min_width(FPS_SLOT_WIDTH);
                                ui.label(
                                    RichText::new(app.fps.label())
                                        .small()
                                        .color(theme::text_muted()),
                                )
                                .on_hover_text(
                                    "Frames per second. Idle: the rate Xade could sustain, \
                                     from the last frame's CPU time.",
                                );
                            },
                        );
                    });
                }
            });
        });
}

/// One dot and name per installed provider: green when its preflight passed, red (error on
/// hover) when not. A provider whose CLI is not installed is hidden while another is shown.
fn providers(app: &App, ui: &mut egui::Ui) {
    for id in app.visible_providers() {
        let Some(p) = app.providers.iter().find(|p| p.id() == id) else {
            continue;
        };
        let err = app.preflight_error(id);
        let color = if err.is_none() {
            theme::ok()
        } else {
            theme::danger()
        };
        let text = match p.account_label() {
            Some(label) => format!("{} ({label})", p.display_name()),
            None => p.display_name().to_string(),
        };
        ui.label(RichText::new("\u{25CF}").small().color(color));
        ui.label(RichText::new(text).small().color(theme::text_muted()))
            .on_hover_text(err.unwrap_or_else(|| "Logged in".into()));
    }
}

/// The providers, the directory name and its git state.
fn left(app: &App, ui: &mut egui::Ui) {
    let muted = |s: String| RichText::new(s).small().color(theme::text_muted());
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
            is_open: true,
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
    fn fps_label_shows_measured_rate_when_animating_and_capacity_when_idle() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut m = FpsMeter::new();
        assert_eq!(m.label(), "");
        m.frame_start(t0);
        m.frame_end(t0 + ms(2));
        // First frame has no previous one: capacity from a 2 ms frame.
        assert_eq!(m.label(), "~500 fps");
        // Frames 10 ms apart: the number is held for 500 ms, then shows the measured 100 fps.
        let frame = |m: &mut FpsMeter, at: u64| {
            m.frame_start(t0 + ms(at));
            m.frame_end(t0 + ms(at + 2));
        };
        for at in (1000..=1100).step_by(10) {
            frame(&mut m, at);
        }
        assert_eq!(m.label(), "~500 fps", "held within the refresh interval");
        for at in (1110..=1600).step_by(10) {
            frame(&mut m, at);
        }
        assert_eq!(m.label(), "100 fps");
        // A long pause means idle again.
        frame(&mut m, 5000);
        assert!(m.label().starts_with('~'), "{}", m.label());
    }

    #[test]
    fn fps_text_caps_at_999_plus() {
        assert_eq!(fps_text(999.4), "999");
        assert_eq!(fps_text(1000.0), "999+");
        assert_eq!(fps_text(50_000.0), "999+");
    }

    #[test]
    fn widest_fps_label_fits_its_slot() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut w = 0.0;
        let out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let g = ui.painter().layout_no_wrap(
                "~999+ fps".into(),
                egui::TextStyle::Small.resolve(ui.style()),
                egui::Color32::WHITE,
            );
            w = g.size().x;
        });
        out.drop_without_applying_deltas();
        assert!(w <= FPS_SLOT_WIDTH, "{w} > {FPS_SLOT_WIDTH}");
    }

    #[test]
    fn the_provider_dot_leads_the_bar() {
        let w = crate::MIN_WINDOW_SIZE[0];
        let texts = bar_with("# branch.oid x\n# branch.head main\n", "w", w, None, true);
        let (_, dot) = texts
            .iter()
            .find(|(t, _)| t == "\u{25CF}")
            .expect("provider dot drawn");
        assert!(dot.min.x < 24.0, "{dot:?}");
    }

    /// The provider names and dot colours the bar draws, with `agy` as Antigravity's preflight.
    #[allow(clippy::unwrap_used)]
    fn provider_bar(
        agy: Option<Result<(), xade_core::ProviderError>>,
    ) -> (Vec<String>, Vec<egui::Color32>) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-bar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        app.providers
            .push(std::sync::Arc::new(crate::app::StubProvider::antigravity()));
        if let Some(r) = agy {
            app.home
                .preflight
                .insert(xade_core::ProviderId::Antigravity, r);
        }
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| providers(&app, ui));
        out.textures_delta.clear();
        let _ = std::fs::remove_dir_all(&dir);
        let (mut names, mut dots) = (Vec::new(), Vec::new());
        fn walk(s: &egui::Shape, n: &mut Vec<String>, d: &mut Vec<egui::Color32>) {
            match s {
                egui::Shape::Text(t) if t.galley.text() == "\u{25CF}" => {
                    d.extend(t.galley.job.sections.first().map(|s| s.format.color))
                }
                egui::Shape::Text(t) => n.push(t.galley.text().to_string()),
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, n, d)),
                _ => {}
            }
        }
        out.shapes
            .iter()
            .for_each(|c| walk(&c.shape, &mut names, &mut dots));
        (names, dots)
    }

    #[test]
    fn each_installed_provider_has_a_dot_in_its_own_state_and_a_missing_one_is_hidden() {
        use xade_core::ProviderError;
        let (names, dots) = provider_bar(None);
        assert_eq!(names, vec!["Claude", "Antigravity"]);
        assert_eq!(dots, vec![theme::ok(), theme::ok()]);
        let (names, dots) = provider_bar(Some(Err(ProviderError::NotLoggedIn)));
        assert_eq!(names, vec!["Claude", "Antigravity"]);
        assert_eq!(dots, vec![theme::ok(), theme::danger()], "its own result");
        let (names, dots) = provider_bar(Some(Err(ProviderError::NotInstalled)));
        assert_eq!(names, vec!["Claude"], "Claude-only users see today's bar");
        assert_eq!(dots, vec![theme::ok()]);
    }

    #[test]
    fn provider_label_git_info_and_warning_do_not_overlap() {
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
        let last = rects.last().unwrap();
        assert!(last.1.max.x <= w, "{last:?}");
        assert!(provider.1.min.x < 40.0, "{provider:?}");
    }
}
