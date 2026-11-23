//! Sessions running outside Xade and prompt-cache freshness: the dot, the cache label, and
//! the confirm modal shown before opening a session that is running elsewhere.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use egui::RichText;
use time::OffsetDateTime;
use xade_core::{RunState, RunningSession, SessionId};

use crate::app::App;
use crate::{icons, theme};

/// How often `AgentProvider::running_sessions` is polled.
pub const POLL: Duration = Duration::from_secs(3);
/// Claude Code writes 1 h prompt-cache entries.
const CACHE_TTL_SECS: i64 = 3600;
/// Minimum width of the confirm modal's content.
pub const MODAL_MIN_WIDTH: f32 = 520.0;
const PULSE_FRAME: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    Fresh,
    Expired,
}

/// Prompt-cache freshness of a session last active at `updated_at`; a future timestamp counts as now.
pub fn cache_label(updated_at: OffsetDateTime, now: OffsetDateTime) -> (String, CacheKind) {
    let elapsed = (now - updated_at).whole_seconds().max(0);
    if elapsed >= CACHE_TTL_SECS {
        return ("Expired".into(), CacheKind::Expired);
    }
    let left = (CACHE_TTL_SECS - elapsed + 59) / 60;
    (format!("Cache \u{b7} {left}m left"), CacheKind::Fresh)
}

pub fn cache_color(kind: CacheKind) -> egui::Color32 {
    match kind {
        CacheKind::Fresh => theme::CACHE_FRESH,
        CacheKind::Expired => theme::TEXT_MUTED,
    }
}

/// Small colored cache label.
pub fn cache_text(updated_at: OffsetDateTime) -> RichText {
    let (text, kind) = cache_label(updated_at, OffsetDateTime::now_utc());
    RichText::new(text).small().color(cache_color(kind))
}

pub fn tooltip(r: &RunningSession) -> String {
    let state = match r.state {
        RunState::Busy => "busy",
        RunState::Idle => "idle",
    };
    format!("Running in {} \u{b7} {state}", r.location)
}

/// The running-elsewhere dot: steady yellow while idle, pulsing yellow (`t` in seconds) while busy.
pub fn dot_color(state: RunState, t: f32) -> egui::Color32 {
    match state {
        RunState::Idle => theme::WARN,
        RunState::Busy => theme::WARN.gamma_multiply(0.55 + 0.45 * (t * 4.0).sin()),
    }
}

/// A filled dot colored by `dot_color`.
pub fn paint_dot(ui: &egui::Ui, center: egui::Pos2, radius: f32, state: RunState) {
    let t = ui.input(|i| i.time) as f32;
    if state == RunState::Busy {
        ui.ctx().request_repaint_after(PULSE_FRAME);
    }
    ui.painter()
        .circle_filled(center, radius, dot_color(state, t));
}

/// Registry entries minus Xade's own children for the sessions `live` in Xade. Xade spawns
/// `claude` with entrypoint `sdk-cli`, so its children list as "SDK" like any other SDK
/// process; the process tree, not the location, tells them apart (`is_own` takes a pid).
pub fn exclude_own(
    list: Vec<RunningSession>,
    live: &HashSet<String>,
    is_own: impl Fn(u32) -> bool,
) -> Vec<RunningSession> {
    list.into_iter()
        .filter(|r| !(live.contains(&r.provider_session_id) && is_own(r.pid)))
        .collect()
}

fn parent_pid(pid: u32) -> Option<u32> {
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Whether `pid` descends from this process. A pid `ps` cannot resolve counts as ours, since
/// it already exited; a missing banner is better than a false one.
pub fn is_own_process(pid: u32) -> bool {
    let me = std::process::id();
    let mut cur = pid;
    for _ in 0..8 {
        match parent_pid(cur) {
            None => return true,
            Some(p) if p == me => return true,
            Some(p) if p <= 1 => return false,
            Some(p) => cur = p,
        }
    }
    false
}

/// The other process holding a session that is live in Xade, if any; `running` must already
/// exclude Xade's own children. This drives the "Also open in" banner.
pub fn elsewhere<'a>(
    provider_session_id: Option<&str>,
    live: bool,
    running: &'a [RunningSession],
) -> Option<&'a RunningSession> {
    let id = provider_session_id.filter(|_| live)?;
    running.iter().find(|r| r.provider_session_id == id)
}

/// Slack between Xade's own last write and a newer transcript before it counts as foreign.
const OWN_WRITE_SLACK: Duration = Duration::from_secs(2);

/// Whether another process appended to the transcript: it changed more than the slack after
/// Xade's last own write (session open or `TurnComplete`) while the session is idle.
pub fn updated_elsewhere(mtime: Option<SystemTime>, own_write: SystemTime, idle: bool) -> bool {
    idle && mtime.is_some_and(|m| m > own_write + OWN_WRITE_SLACK)
}

/// Fork state of one live session, refreshed by each running poll.
#[derive(Default)]
pub struct SessionSync {
    /// Location of another process holding the same session ("Also open in" banner).
    pub elsewhere: Option<String>,
    pub transcript: TranscriptWatch,
}

/// What a live session knows about its transcript file.
pub struct TranscriptWatch {
    pub path: Option<PathBuf>,
    pub mtime: Option<SystemTime>,
    /// When Xade last wrote it itself: at open, then at every turn end.
    pub own_write: SystemTime,
}

impl Default for TranscriptWatch {
    fn default() -> Self {
        TranscriptWatch {
            path: None,
            mtime: None,
            own_write: SystemTime::now(),
        }
    }
}

/// A session whose transcript the poll should stat.
pub struct TranscriptJob {
    pub id: SessionId,
    pub provider_session_id: String,
    pub path: Option<PathBuf>,
}

pub struct TranscriptStat {
    pub id: SessionId,
    pub path: Option<PathBuf>,
    pub mtime: Option<SystemTime>,
}

/// `<home>/.claude/projects/<dir>/<id>.jsonl`, found by scanning the project dirs.
fn find_transcript(home: &Path, provider_session_id: &str) -> Option<PathBuf> {
    let name = format!("{provider_session_id}.jsonl");
    std::fs::read_dir(home.join(".claude/projects"))
        .ok()?
        .flatten()
        .map(|e| e.path().join(&name))
        .find(|p| p.is_file())
}

/// Stats each transcript, scanning for the path once and then reusing it; blocking.
pub fn stat_transcripts(jobs: Vec<TranscriptJob>) -> Vec<TranscriptStat> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    jobs.into_iter()
        .map(|j| {
            let path = j
                .path
                .or_else(|| find_transcript(home.as_deref()?, &j.provider_session_id));
            let mtime = path
                .as_deref()
                .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
            TranscriptStat {
                id: j.id,
                path,
                mtime,
            }
        })
        .collect()
}

/// A session waiting for the user to confirm opening it although it runs elsewhere.
pub struct PendingOpen {
    pub id: SessionId,
    pub location: String,
    pub background: bool,
}

/// Draws the confirm modal when one is pending and applies the decision.
pub fn confirm_modal(app: &mut App, ctx: &egui::Context) {
    let Some(p) = app.pending_open.as_ref() else {
        return;
    };
    let location = p.location.clone();
    let (mut accept, mut cancel) = (false, false);
    let resp = egui::Modal::new(egui::Id::unique("confirm_open_running")).show(ctx, |ui| {
        ui.set_min_width(MODAL_MIN_WIDTH);
        ui.label(
            RichText::new(format!("{} Session open elsewhere", icons::WARNING))
                .strong()
                .size(18.0)
                .color(theme::WARN),
        );
        ui.add_space(6.0);
        ui.label(format!(
            "This session is running in {location}. If you send messages here and there, the \
             conversation forks: each side only sees its own replies until it reopens the session."
        ));
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            accept = ui
                .add(
                    egui::Button::new(RichText::new("Open anyway").color(theme::BG_APP))
                        .fill(theme::WARN),
                )
                .clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    if accept {
        if let Some(p) = app.pending_open.take() {
            app.open_confirmed(p.id, p.background);
        }
    } else if cancel || resp.should_close() {
        app.pending_open = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(elapsed_secs: i64) -> (String, CacheKind) {
        let now = OffsetDateTime::now_utc();
        cache_label(now - time::Duration::seconds(elapsed_secs), now)
    }

    #[test]
    fn cache_minutes_left_and_expiry() {
        assert_eq!(
            label(60),
            ("Cache \u{b7} 59m left".into(), CacheKind::Fresh)
        );
        assert_eq!(
            label(59 * 60),
            ("Cache \u{b7} 1m left".into(), CacheKind::Fresh)
        );
        assert_eq!(label(3600), ("Expired".into(), CacheKind::Expired));
        assert_eq!(label(86_400).1, CacheKind::Expired);
    }

    #[test]
    fn future_timestamps_clamp_to_a_full_cache() {
        assert_eq!(
            label(-600),
            ("Cache \u{b7} 60m left".into(), CacheKind::Fresh)
        );
    }

    #[test]
    fn dot_is_yellow_steady_when_idle_and_pulses_when_busy() {
        for t in [0.0, 0.3, 1.0, 7.5] {
            assert_eq!(dot_color(RunState::Idle, t), theme::WARN);
        }
        let busy: Vec<_> = [0.0, 0.4, 0.8, 1.2]
            .iter()
            .map(|t| dot_color(RunState::Busy, *t))
            .collect();
        assert!(busy.iter().any(|c| *c != busy[0]), "busy dot is steady");
        // Same hue as WARN, never brighter.
        assert!(busy.iter().all(|c| c.r() <= theme::WARN.r() && c.r() > 0));
    }

    fn entry(id: &str, pid: u32, location: &str) -> RunningSession {
        RunningSession {
            provider_session_id: id.into(),
            state: RunState::Idle,
            location: location.into(),
            pid,
        }
    }

    #[test]
    fn banner_needs_a_foreign_process_on_a_session_live_in_xade() {
        // Pid 10 is Xade's own child ("SDK"), 20 a terminal, 30 another SDK process.
        let all = vec![
            entry("a", 10, "SDK"),
            entry("a", 20, "terminal"),
            entry("b", 10, "SDK"),
            entry("c", 30, "SDK"),
        ];
        let live: HashSet<String> = ["a", "b"].map(String::from).into();
        let seen = exclude_own(all, &live, |pid| pid == 10);
        let loc =
            |id: Option<&str>, live: bool| elsewhere(id, live, &seen).map(|r| r.location.clone());
        assert_eq!(loc(Some("a"), true), Some("terminal".into()));
        // Only Xade's own child holds "b": no banner.
        assert_eq!(loc(Some("b"), true), None);
        // Not live in Xade, or no provider id yet: no banner.
        assert_eq!(loc(Some("a"), false), None);
        assert_eq!(loc(None, true), None);
        // A foreign "SDK" process on a live session is kept; the other pid is not "own".
        assert_eq!(
            exclude_own(
                vec![entry("c", 30, "SDK")],
                &["c".to_string()].into(),
                |p| p == 10
            )
            .len(),
            1
        );
        // Entries of sessions not live in Xade are never dropped.
        assert_eq!(
            exclude_own(vec![entry("z", 10, "SDK")], &HashSet::new(), |_| true).len(),
            1
        );
    }

    #[test]
    fn own_process_walks_the_process_tree() {
        assert!(!is_own_process(1));
        let mut child = Command::new("sleep").arg("5").spawn().expect("spawn");
        let own = is_own_process(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert!(own);
    }

    #[test]
    fn reload_is_offered_only_for_idle_sessions_changed_after_the_slack() {
        let own = SystemTime::now();
        let after = |s: u64| Some(own + Duration::from_secs(s));
        assert!(updated_elsewhere(after(3), own, true));
        // Within the slack of Xade's own last write, still working, or no file.
        assert!(!updated_elsewhere(after(2), own, true));
        assert!(!updated_elsewhere(after(60), own, false));
        assert!(!updated_elsewhere(
            Some(own - Duration::from_secs(60)),
            own,
            true
        ));
        assert!(!updated_elsewhere(None, own, true));
    }

    #[test]
    fn transcript_is_found_once_and_statted() {
        let home = std::env::temp_dir().join(format!("xade-home-{}", uuid::Uuid::new_v4()));
        let proj = home.join(".claude/projects/-tmp-x");
        std::fs::create_dir_all(&proj).expect("dir");
        std::fs::write(proj.join("sid.jsonl"), "{}").expect("write");
        assert_eq!(find_transcript(&home, "sid"), Some(proj.join("sid.jsonl")));
        assert_eq!(find_transcript(&home, "other"), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn modal_renders_and_escape_cancels() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-confirm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        app.pending_open = Some(PendingOpen {
            id: SessionId::new(),
            location: "terminal".into(),
            background: false,
        });
        let frame = |app: &mut App, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 700.0),
                )),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_pass(input, |ctx| confirm_modal(app, ctx));
            out.textures_delta.clear();
            out
        };
        frame(&mut app, vec![]);
        let out = frame(&mut app, vec![]);
        assert!(app.pending_open.is_some());
        // The dialog's frame is at least MODAL_MIN_WIDTH wide (the dimmed backdrop spans 1000)
        // and says what it warns about.
        fn walk(s: &egui::Shape, widths: &mut Vec<f32>, texts: &mut String) {
            match s {
                egui::Shape::Rect(r) => widths.push(r.rect.width()),
                egui::Shape::Text(t) => texts.push_str(t.galley.text()),
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, widths, texts)),
                _ => {}
            }
        }
        let (mut widths, mut texts) = (Vec::new(), String::new());
        out.shapes
            .iter()
            .for_each(|c| walk(&c.shape, &mut widths, &mut texts));
        assert!(
            widths.iter().any(|w| (MODAL_MIN_WIDTH..900.0).contains(w)),
            "{widths:?}"
        );
        assert!(texts.contains("Session open elsewhere") && texts.contains("Open anyway"));
        assert!(texts.contains("running in terminal"), "{texts}");
        let esc = egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        frame(&mut app, vec![esc]);
        assert!(app.pending_open.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
