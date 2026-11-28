//! `App`: all UI state, the event bridge, and the shared shell (top bar, status bar, sidebar).

use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_commonmark::CommonMarkCache;
use time::OffsetDateTime;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use xade_core::{
    AgentProvider, Delivery, Effort, Message, ModelOption, MultiSessionId, ProviderError,
    ProviderEvent, ProviderId, QuotaGroup, QuotaKind, QuotaWindow, Role, RunningSession, Session,
    SessionId, SessionStatus, Workspace, WorkspaceId,
};
use xade_runtime::git::{git_status, GitState};
use xade_runtime::{AppEvent, AppEventKind, ManagerCmd};
use xade_store::{Page, SessionCard, StoreReader, WriteOp, WriterHandle};

use crate::config::{self, Config};
use crate::diff::{is_code_change, DiffData};
use crate::editor::{self, CloseChoice, EditorId, Editors, SaveOutcome};
use crate::errors::{self, ErrorQueue, ErrorReport, ModalAction};
use crate::file_tree::{self, FileTree, Listing, TreeAction};
use crate::guide::GuideVm;
use crate::home::HomeState;
use crate::keymap::{self, Action, Chord};
use crate::multi_view::{MultiSessionVm, MultiTab};
use crate::running::{self, PendingOpen};
use crate::widgets::about_modal;
use crate::widgets::delete_modal::{self, PendingDelete, PendingDeleteTarget};
use crate::widgets::file_picker::{self, FilePicker};
use crate::widgets::keymaps_modal;
use crate::widgets::multi_modal::{self, MultiCreateState};
use crate::widgets::path_links::PathLinks;
use crate::widgets::quit_modal;
use crate::widgets::session_picker::{self, PickerState};
use crate::windows_state::Saved;
use crate::workspace::{Tiling, WindowKind};
use crate::{composer, home, logging, status_bar, theme, workspace};

/// The only provider that reports quota so far; the runtime's quota event carries no provider.
/// A second one adds its id to that event and replaces this constant where it is read.
pub(crate) const QUOTA_PROVIDER: ProviderId = ProviderId::Claude;
/// How often a provider is asked for its quota; a finished turn asks again sooner.
const QUOTA_POLL: Duration = Duration::from_secs(300);
const GIT_REFRESH: Duration = Duration::from_secs(5);
/// A running-set change re-imports at most this often.
const RUNNING_IMPORT_GAP: Duration = Duration::from_secs(30);
/// How long a warning stays in the status bar.
const WARNING_TTL: Duration = Duration::from_secs(10);
const LOG_TAIL_LINES: usize = 50;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Home,
    Workspace,
}

/// Everything background tasks send to `App::logic` through the std channel.
pub enum UiMsg {
    App(Box<AppEvent>),
    Cards {
        generation: u64,
        page: Page<SessionCard>,
    },
    Workspaces(Vec<Workspace>),
    /// Sent after `Workspaces` for a folder given on the command line.
    OpenWorkspace(WorkspaceId),
    /// One provider's preflight (startup, or its Retry).
    Preflight(ProviderId, Result<(), ProviderError>),
    /// Provider of the latest Xade session, the default for a new one.
    LastProvider(Option<ProviderId>),
    /// The fork prompts, read off the UI thread; `warning` says why a bundled one is used.
    ForkPrompt {
        session: SessionId,
        provider: ProviderId,
        prompt: String,
        resume: String,
        warning: Option<String>,
    },
    /// Quota rows saved by earlier runs.
    SavedQuota(Vec<QuotaWindow>),
    /// A provider's answer to `AgentProvider::quota`.
    ProviderQuota(ProviderId, Result<Vec<QuotaGroup>, ProviderError>),
    /// An error from a background task.
    Report(ErrorReport),
    PickerCards {
        generation: u64,
        page: Page<SessionCard>,
    },
    /// One `running_sessions` poll, minus Xade's own children, with the live sessions'
    /// transcript mtimes.
    Running {
        list: Vec<RunningSession>,
        transcripts: Vec<running::TranscriptStat>,
    },
    /// The working tree when a turn started (`None` outside a git repo).
    TurnBase {
        session: SessionId,
        base: Option<crate::turn_files::Snapshot>,
    },
    /// What a finished turn changed, for the card after message `anchor`.
    TurnFiles {
        session: SessionId,
        anchor: uuid::Uuid,
        changes: crate::turn_files::TurnChanges,
    },
    /// The files the quick-open listed, one list per root.
    PickerFiles {
        roots: Vec<PathBuf>,
        files: Vec<Vec<String>>,
    },
    /// A session folder's file list for `@` autocomplete.
    Files {
        session: SessionId,
        files: Vec<String>,
    },
    /// A pasted image could not be saved; shown above the session's input.
    AttachFailed {
        session: SessionId,
        message: String,
    },
    /// Files picked with a session's attach button, or a saved pasted image.
    Attached {
        session: SessionId,
        paths: Vec<PathBuf>,
    },
    /// A file picked with Open File, read (or decoded, for images) off the UI thread.
    FileOpened {
        path: PathBuf,
        content: Result<editor::FileContent, String>,
    },
    /// An open image re-decoded off the UI thread after its mtime changed on disk.
    ImageReloaded {
        id: EditorId,
        result: Result<egui::ColorImage, String>,
    },
    /// A folder read for the file tree finished.
    DirListed {
        path: PathBuf,
        result: Result<Listing, String>,
    },
    /// A save finished; `revision` is the text version that was written.
    Saved {
        id: EditorId,
        revision: u64,
        outcome: SaveOutcome,
    },
}

/// Sends to the UI thread and wakes it.
#[derive(Clone)]
pub struct Bridge {
    tx: Sender<UiMsg>,
    ctx: egui::Context,
}

impl Bridge {
    pub fn send(&self, msg: UiMsg) {
        let _ = self.tx.send(msg);
        self.ctx.request_repaint();
    }
}

/// Per-session UI state; lives in `App` so background sessions keep updating.
pub struct SessionVm {
    pub session: Session,
    pub cwd: PathBuf,
    pub status: SessionStatus,
    /// Messages held while a turn runs; the oldest goes out when the session turns idle.
    pub queued: std::collections::VecDeque<String>,
    pub messages: Vec<Message>,
    /// Streaming text of the block in flight; replaced by the finalized block.
    pub live_text: String,
    pub thinking_tokens: Option<u64>,
    pub pending_permission: Option<PendingPermission>,
    pub input: String,
    pub error: Option<String>,
    pub md_cache: CommonMarkCache,
    pub path_links: PathLinks,
    /// The diff modal, when open.
    pub diff: Option<DiffState>,
    /// Per-turn "files changed" summaries, computed once per finished turn.
    pub changes: crate::widgets::changes_summary::ChangesCache,
    /// A model or effort change waits for the next turn.
    pub settings_pending: bool,
    /// Working tree at the start of the running turn, to find what the turn changed.
    pub turn_base: Option<crate::turn_files::Snapshot>,
    /// A snapshot was requested for the running turn.
    pub turn_snapshotting: bool,
    /// Files each finished turn changed, keyed by the turn's last message; memory only.
    pub turn_files: std::collections::HashMap<uuid::Uuid, crate::turn_files::TurnChanges>,
    /// Model and thinking effort the last message went out with; a different choice now costs
    /// the next message its prompt cache.
    pub sent_model: String,
    pub sent_effort: xade_core::Effort,
    pub collapsed: bool,
    /// Last rect of the expanded window.
    pub window_rect: Option<egui::Rect>,
    /// One-frame layout override (tiling, collapse, expand).
    pub apply_rect: Option<egui::Rect>,
    /// Whether another process shares or has changed this live session's transcript.
    pub sync: running::SessionSync,
    /// `@` file autocomplete of the composer.
    pub mention: crate::mention::MentionState,
    /// Why an attachment was refused, shown above the input until it expires or a send.
    pub attach_note: Option<(String, std::time::Instant)>,
    /// The next composer frame focuses the text box with the cursor at the end (after a quote).
    pub focus_input: bool,
    /// What running subagents did this turn, in memory only (never stored); cleared when the turn ends.
    pub subagent_msgs: Vec<Message>,
    /// The chat area shows `subagent_msgs` instead of the main transcript.
    pub view_subagents: bool,
    /// A fork of this session is being prepared; its fork button stays off until it ends.
    pub fork_preparing: bool,
    /// The header title is being edited; holds the text so far.
    pub renaming: Option<String>,
    /// The user just sent a message: the conversation jumps to the bottom and sticks there.
    pub scroll_to_end: bool,
}

pub struct DiffState {
    /// Empty for a merged review of several calls, which is never decided.
    pub tool_use_id: String,
    pub data: DiffData,
}

pub struct PendingPermission {
    pub request_id: String,
    pub tool_use_id: String,
    pub tool_name: String,
    pub input_json: String,
    /// A code-change tool whose diff opened in the modal.
    pub reviewable: bool,
}

impl SessionVm {
    /// Opens the diff modal for a tool call, from its stored result when it has finished.
    pub fn open_diff(&mut self, tool_use_id: &str) {
        self.diff = self.diff_of(tool_use_id).map(|data| DiffState {
            tool_use_id: tool_use_id.to_string(),
            data,
        });
    }

    /// Opens one read-only modal over several finished calls to the same file.
    pub fn open_changes(&mut self, tool_use_ids: &[String]) {
        let parts = tool_use_ids.iter().filter_map(|id| self.diff_of(id));
        self.diff = DiffData::merge(parts.collect()).map(|data| DiffState {
            tool_use_id: String::new(),
            data,
        });
    }

    /// Opens one read-only modal for file `index` of the turn that ended with message `anchor`.
    pub fn open_turn_file(&mut self, anchor: uuid::Uuid, index: usize) {
        let Some(f) = self
            .turn_files
            .get(&anchor)
            .and_then(|c| c.files.get(index))
        else {
            return;
        };
        self.diff = Some(DiffState {
            tool_use_id: String::new(),
            data: DiffData::from_texts(&f.path, &f.old, &f.new),
        });
    }

    fn diff_of(&self, tool_use_id: &str) -> Option<DiffData> {
        let find = |role: Role| {
            self.messages
                .iter()
                .find(|m| m.role == role && m.tool_use_id.as_deref() == Some(tool_use_id))
        };
        let (Some(call), result) = (find(Role::ToolCall), find(Role::ToolResult)) else {
            return None;
        };
        DiffData::build(
            call.tool_name.as_deref().unwrap_or_default(),
            call.tool_payload_json.as_deref().unwrap_or_default(),
            result.and_then(|r| r.tool_payload_json.as_deref()),
            &self.cwd,
        )
    }
}

pub struct App {
    pub view: View,
    pub home: HomeState,
    pub sessions: HashMap<SessionId, SessionVm>,
    pub multi_sessions: HashMap<MultiSessionId, MultiSessionVm>,
    pub multi_create: Option<MultiCreateState>,
    pub open_windows: Vec<WindowKind>,
    /// Where the open windows are saved; `None` for a launch with a path, which leaves the plain window's list alone.
    windows_file: Option<PathBuf>,
    windows_saved: Vec<crate::windows_state::Saved>,
    /// Restored windows still opening; one that fails is dropped quietly instead of reported.
    restoring: HashSet<Saved>,
    /// The windows file whose restore waits for a provider still running its preflight.
    deferred_restore: Option<PathBuf>,
    pub focused: Option<WindowKind>,
    pub editors: Editors,
    pub tree: FileTree,
    /// The "Get started" window while it is open.
    pub guide: Option<GuideVm>,
    /// The startup checks have reported and the guide was opened if none passed.
    guide_offered: bool,
    /// Last quota window per provider and kind, in draw order (session before weekly).
    pub quota: Vec<(ProviderId, QuotaWindow)>,
    /// Quota providers report on request (`AgentProvider::quota`), by provider.
    pub provider_quota: HashMap<ProviderId, Vec<QuotaGroup>>,
    /// When each provider's quota was last asked for, and which asks are still out.
    quota_checked: HashMap<ProviderId, Instant>,
    quota_inflight: HashSet<ProviderId>,
    pub cmd_tx: UnboundedSender<ManagerCmd>,
    pub rt: Handle,
    pub git: HashMap<PathBuf, GitState>,
    /// Errors waiting for the modal; see `report`.
    pub errors: ErrorQueue,
    /// A non-fatal notice for the status bar, with when it appeared.
    warning: Option<(String, Instant)>,
    /// Where the log files live; `None` in headless tests.
    pub(crate) log_dir: Option<PathBuf>,
    pub providers: Vec<Arc<dyn AgentProvider>>,
    pub writer: WriterHandle,
    pub reader: StoreReader,
    pub tiling: Tiling,
    /// The session window filling the central area; the others are collapsed.
    pub maximized: Option<WindowKind>,
    /// Window to bring to the front on the next frame.
    pub raise: Option<WindowKind>,
    pub vscode: Option<PathBuf>,
    pub config: Config,
    /// Live-reload watcher of `settings.json`; absent in headless tests.
    config_watch: Option<config::Watch>,
    pub chord: Chord,
    pub fps: crate::status_bar::FpsMeter,
    pub sidebar_visible: bool,
    pub keymaps_open: bool,
    pub about_open: bool,
    pub status_open: bool,
    pub help_open: bool,
    /// The Cmd+F bar, when open.
    pub find: Option<crate::find::FindState>,
    /// The session picker, when open.
    pub picker: Option<PickerState>,
    picker_generation: u64,
    /// The file quick-open, when open.
    pub file_picker: Option<FilePicker>,
    /// Sessions running outside Xade, by provider session id.
    pub running: HashMap<String, RunningSession>,
    /// Awaiting confirmation to open a session that is running elsewhere.
    pub pending_open: Option<PendingOpen>,
    /// Awaiting confirmation to delete a session forever.
    pub pending_delete: Option<PendingDelete>,
    /// Awaiting confirmation to quit while sessions are mid-turn.
    pub pending_quit: bool,
    quit_confirmed: bool,
    /// Frames drawn with the "Saving workspaces…" overlay; the real close waits for it to paint.
    saving_frames: Option<u8>,
    /// Text selected in a chat and where the pointer left it: the Copy / Quote popup.
    pub selection_menu: Option<crate::widgets::selection_menu::Pending>,
    running_checked: Option<Instant>,
    tree_checked: Option<Instant>,
    running_inflight: bool,
    /// Provider session ids of fork snapshot turns running now; Xade's own, so they never count
    /// as sessions running elsewhere.
    throwaways: HashSet<String>,
    running_polled: bool,
    running_import_due: bool,
    running_import_at: Option<Instant>,
    /// Sessions closed for a reload; each reopens when the import that follows finishes.
    reloading: HashSet<SessionId>,
    /// Sessions being opened from Home with Cmd+click: they get a window but no view switch.
    background: HashSet<SessionId>,
    /// Multi-sessions opened from Home with Cmd+click (no view switch).
    background_multi: HashSet<MultiSessionId>,
    events_rx: Receiver<UiMsg>,
    bridge: Bridge,
    git_inflight: HashSet<PathBuf>,
    git_stale: HashSet<PathBuf>,
    git_dir: Option<PathBuf>,
    git_checked: Instant,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        rt: Handle,
        cmd_tx: UnboundedSender<ManagerCmd>,
        mut manager_events: UnboundedReceiver<AppEvent>,
        providers: Vec<Arc<dyn AgentProvider>>,
        writer: WriterHandle,
        reader: StoreReader,
        preflight: HashMap<ProviderId, Result<(), ProviderError>>,
        config_path: PathBuf,
        log_dir: PathBuf,
        initial_workspace: Option<PathBuf>,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        cc.egui_ctx
            .add_plugin(crate::widgets::selection_menu::SelectionWatcher);
        let (config, config_error) = config::load(&config_path);
        if let Some(dir) = config_path.parent().map(Path::to_path_buf) {
            rt.spawn_blocking(move || config::ensure_default_fork_prompt(&dir));
        }
        let (tx, events_rx) = std::sync::mpsc::channel();
        let bridge = Bridge {
            tx,
            ctx: cc.egui_ctx.clone(),
        };
        let fwd = bridge.clone();
        rt.spawn(async move {
            while let Some(ev) = manager_events.recv().await {
                fwd.send(UiMsg::App(Box::new(ev)));
            }
        });
        // A provider with no startup result is checked in the background: not offered, and not
        // an error, until it reports.
        let pending: HashSet<_> = providers
            .iter()
            .map(|p| p.id())
            .filter(|id| !preflight.contains_key(id))
            .collect();
        let mut app = App {
            view: View::Home,
            home: HomeState {
                preflight,
                pending: pending.clone(),
                ..HomeState::default()
            },
            sessions: HashMap::new(),
            multi_sessions: HashMap::new(),
            multi_create: None,
            open_windows: Vec::new(),
            windows_file: None,
            windows_saved: Vec::new(),
            restoring: HashSet::new(),
            deferred_restore: None,
            focused: None,
            editors: Editors::default(),
            tree: FileTree::default(),
            guide: None,
            guide_offered: false,
            quota: Vec::new(),
            provider_quota: HashMap::new(),
            quota_checked: HashMap::new(),
            quota_inflight: HashSet::new(),
            cmd_tx,
            rt,
            git: HashMap::new(),
            errors: ErrorQueue::default(),
            warning: None,
            log_dir: Some(log_dir),
            providers,
            writer,
            reader,
            tiling: Tiling::default(),
            maximized: None,
            raise: None,
            vscode: crate::widgets::diff_modal::find_code(),
            // No family yet, so the first `apply_config` always installs the configured font.
            config: Config {
                font_family: None,
                ..Config::default()
            },
            config_watch: Some(config::Watch::new(config_path.clone())),
            chord: Chord::default(),
            fps: crate::status_bar::FpsMeter::new(),
            sidebar_visible: true,
            keymaps_open: false,
            about_open: false,
            status_open: false,
            help_open: false,
            find: None,
            picker: None,
            picker_generation: 0,
            file_picker: None,
            running: HashMap::new(),
            pending_open: None,
            pending_delete: None,
            pending_quit: false,
            quit_confirmed: false,
            saving_frames: None,
            selection_menu: None,
            running_checked: None,
            tree_checked: None,
            running_inflight: false,
            throwaways: HashSet::new(),
            running_polled: false,
            running_import_due: false,
            running_import_at: None,
            reloading: HashSet::new(),
            background: HashSet::new(),
            background_multi: HashSet::new(),
            events_rx,
            bridge,
            git_inflight: HashSet::new(),
            git_stale: HashSet::new(),
            git_dir: None,
            git_checked: Instant::now(),
        };
        if let Some(e) = config_error {
            app.warn(e);
        }
        app.apply_config(&cc.egui_ctx, config);
        for id in pending {
            app.retry_preflight(id);
        }
        app.refresh_workspaces();
        app.refresh_cards();
        app.load_saved_quota();
        app.load_last_provider();
        editor::warm_syntax(&app.rt);
        if let Some(root) = initial_workspace {
            app.open_folder(root);
        } else {
            app.restore_windows(crate::windows_state::path(&config_path));
        }
        app.offer_guide();
        if app.any_available() {
            app.start_import();
        }
        app
    }

    /// A bare `App` over a store at `db`, for headless layout tests; starts nothing.
    #[cfg(test)]
    pub fn for_test(ctx: &egui::Context, rt: Handle, db: &std::path::Path) -> Self {
        let (tx, events_rx) = std::sync::mpsc::channel();
        let writer = xade_store::StoreWriter::spawn(db).expect("writer");
        let reader = xade_store::StoreReader::open(db).expect("reader");
        App {
            view: View::Home,
            home: HomeState::default(),
            sessions: HashMap::new(),
            multi_sessions: HashMap::new(),
            multi_create: None,
            open_windows: Vec::new(),
            windows_file: None,
            windows_saved: Vec::new(),
            restoring: HashSet::new(),
            deferred_restore: None,
            focused: None,
            editors: Editors::default(),
            tree: FileTree::default(),
            guide: None,
            guide_offered: false,
            quota: Vec::new(),
            provider_quota: HashMap::new(),
            quota_checked: HashMap::new(),
            quota_inflight: HashSet::new(),
            cmd_tx: tokio::sync::mpsc::unbounded_channel().0,
            rt,
            git: HashMap::new(),
            errors: ErrorQueue::default(),
            warning: None,
            log_dir: None,
            providers: vec![Arc::new(StubProvider::claude())],
            writer,
            reader,
            tiling: Tiling::default(),
            maximized: None,
            raise: None,
            vscode: None,
            config: Config::default(),
            config_watch: None,
            chord: Chord::default(),
            fps: crate::status_bar::FpsMeter::new(),
            sidebar_visible: true,
            keymaps_open: false,
            about_open: false,
            status_open: false,
            help_open: false,
            find: None,
            picker: None,
            picker_generation: 0,
            file_picker: None,
            running: HashMap::new(),
            pending_open: None,
            pending_delete: None,
            pending_quit: false,
            quit_confirmed: false,
            saving_frames: None,
            selection_menu: None,
            running_checked: None,
            tree_checked: None,
            running_inflight: false,
            throwaways: HashSet::new(),
            running_polled: false,
            running_import_due: false,
            running_import_at: None,
            reloading: HashSet::new(),
            background: HashSet::new(),
            background_multi: HashSet::new(),
            events_rx,
            bridge: Bridge {
                tx,
                ctx: ctx.clone(),
            },
            git_inflight: HashSet::new(),
            git_stale: HashSet::new(),
            git_dir: None,
            git_checked: Instant::now(),
        }
    }

    // ----- errors and warnings -----

    /// Queues an error for the modal (and logs it).
    pub fn report(&mut self, report: ErrorReport) {
        tracing::error!(title = %report.title, message = %report.message, "error reported");
        self.errors.push(report);
    }

    /// Logs a non-fatal problem and shows it in the status bar for `WARNING_TTL`. A warning
    /// that repeats while still shown is not logged again.
    pub fn warn(&mut self, message: impl Into<String>) {
        let message = message.into();
        if self.warning_text() != Some(message.as_str()) {
            tracing::warn!(%message, "warning shown");
        }
        self.warning = Some((message, Instant::now()));
    }

    /// The warning still on display.
    pub fn warning_text(&self) -> Option<&str> {
        self.warning
            .as_ref()
            .filter(|(_, at)| at.elapsed() < WARNING_TTL)
            .map(|(m, _)| m.as_str())
    }

    /// Runs one frame step so that a panic never closes the app: it is logged by the panic hook,
    /// recorded as an error report, and the rest of the step is skipped. Returns whether `f`
    /// finished.
    fn guarded(&mut self, what: &str, f: impl FnOnce(&mut Self)) -> bool {
        match catch_unwind(AssertUnwindSafe(|| f(self))) {
            Ok(()) => true,
            Err(payload) => {
                let msg = xade_core::panic_message(&*payload);
                let report = ErrorReport::new(
                    "Xade hit an internal error",
                    format!("A bug stopped part of the {what}: {msg}\nXade is still running."),
                )
                .with_details(logging::take_panic_details().unwrap_or_default());
                self.report(report);
                false
            }
        }
    }

    /// `guarded` for a drawing step: a panic leaves the frame half drawn, so an empty central
    /// panel finishes it. The error modal comes last either way.
    fn guarded_ui(&mut self, ui: &mut egui::Ui, draw: impl FnOnce(&mut Self, &mut egui::Ui)) {
        if !self.guarded("window", |app| draw(app, ui)) {
            egui::CentralPanel::default().show(ui, |_| {});
        }
        self.error_modal(&ui.ctx().clone());
    }

    /// Shows the first queued error and carries out the button pressed.
    fn error_modal(&mut self, ctx: &egui::Context) {
        let Some((report, count)) = self.errors.current().map(|(r, n)| (r.clone(), n)) else {
            return;
        };
        let action = catch_unwind(AssertUnwindSafe(|| errors::modal(ctx, &self.errors)))
            .unwrap_or_else(|_| {
                tracing::error!("the error modal panicked; dismissing the error");
                ModalAction::Dismiss
            });
        let tail = || {
            let dir = self.log_dir.as_deref();
            dir.map(|d| logging::tail(d, LOG_TAIL_LINES))
                .unwrap_or_default()
        };
        match action {
            ModalAction::None => {}
            ModalAction::Copy => ctx.copy_text(errors::report_text(&report, count, &tail())),
            ModalAction::OpenIssue => {
                let url = errors::issue_url(&report, count, &tail(), dirs::home_dir().as_deref());
                ctx.open_url(egui::OpenUrl::new_tab(url));
            }
            ModalAction::OpenLogs => {
                if let Some(dir) = &self.log_dir {
                    logging::open_dir(dir);
                }
            }
            ModalAction::Dismiss => self.errors.dismiss(),
        }
    }

    // ----- settings.json -----

    /// Applies `next`; fonts are rebuilt only when the family changed (or at startup with one).
    fn apply_config(&mut self, ctx: &egui::Context, next: Config) {
        theme::set_font_size(ctx, next.font_size);
        theme::set_palette(next.theme.clone());
        theme::refresh(ctx);
        keymap::set_current(next.shortcuts.clone());
        if next.font_family != self.config.font_family {
            let defs = match next.font_family.as_deref().map(config::font_definitions) {
                Some(Ok(defs)) => Some(defs),
                Some(Err(e)) => {
                    self.warn(format!("settings.json: {e}"));
                    None
                }
                None => Some(egui::FontDefinitions::default()),
            };
            if let Some(defs) = defs {
                ctx.set_fonts(defs);
            }
        }
        self.config = next;
    }

    /// Re-reads `settings.json` when its mtime changed (checked every `config::POLL`).
    fn reload_config(&mut self, ctx: &egui::Context) {
        let Some(watch) = self.config_watch.as_mut() else {
            return;
        };
        ctx.request_repaint_after(config::POLL);
        if !watch.changed(Instant::now()) {
            return;
        }
        let (next, error) = config::load(watch.path());
        tracing::info!("settings.json reloaded");
        if let Some(e) = error {
            self.warn(e);
        }
        self.apply_config(ctx, next);
    }

    /// Rewrites `settings.json` with the defaults; the watcher applies them within `POLL`.
    pub fn restore_default_settings(&mut self) {
        let Some(path) = self.config_watch.as_ref().map(|w| w.path().to_path_buf()) else {
            return;
        };
        match config::restore_defaults(&path) {
            Ok(()) => {
                self.warn("Settings restored to defaults (old file kept as settings.json.bak)")
            }
            Err(e) => self.warn(e),
        }
    }

    /// Opens `settings.json` in Xade's own editor, focusing the window when it is already open.
    pub fn open_settings(&mut self) {
        if let Some(path) = self.config_watch.as_ref().map(|w| w.path().to_path_buf()) {
            self.open_path(path);
        }
    }

    /// Opens a file in Xade's editor: focuses the window when it is already open, else reads it
    /// off the UI thread.
    pub fn open_path(&mut self, path: PathBuf) {
        if let Some(id) = self.editors.find_path(&path) {
            self.focus(WindowKind::Editor(id));
            return;
        }
        let bridge = self.bridge.clone();
        self.rt.spawn_blocking(move || {
            let content = editor::read_entry(&path);
            bridge.send(UiMsg::FileOpened { path, content });
        });
    }

    /// Expands a tree folder (reading it in the background) or collapses it.
    pub fn toggle_dir(&mut self, path: &std::path::Path) {
        if !self.tree.toggle(path) {
            return;
        }
        let (path, bridge) = (path.to_path_buf(), self.bridge.clone());
        self.rt.spawn_blocking(move || {
            let result = file_tree::read_listing(&path);
            bridge.send(UiMsg::DirListed { path, result });
        });
    }

    /// Re-reads every expanded folder (at most once per `running::POLL`) so files and folders
    /// created outside the tree show up; the old listing stays visible until the new one lands.
    fn refresh_tree(&mut self) {
        if self
            .tree_checked
            .is_some_and(|t| t.elapsed() < running::POLL)
        {
            return;
        }
        self.tree_checked = Some(Instant::now());
        for path in self.tree.expanded() {
            let bridge = self.bridge.clone();
            self.rt.spawn_blocking(move || {
                let result = file_tree::read_listing(&path);
                bridge.send(UiMsg::DirListed { path, result });
            });
        }
    }

    pub fn tree_action(&mut self, action: TreeAction) {
        match action {
            TreeAction::Toggle(path) => self.toggle_dir(&path),
            TreeAction::Open(path) => self.open_path(path),
            TreeAction::Reveal(dir) => logging::open_dir(&dir),
        }
    }

    // ----- background work -----

    pub fn refresh_workspaces(&self) {
        let (reader, bridge) = (self.reader.clone(), self.bridge.clone());
        self.rt.spawn(async move {
            match reader.list_workspaces().await {
                Ok(ws) => bridge.send(UiMsg::Workspaces(ws)),
                Err(e) => read_failed(&bridge, "workspaces", &e),
            }
        });
    }

    /// Lists or searches sessions for the current scope, query and page.
    pub fn refresh_cards(&mut self) {
        self.home.generation += 1;
        let generation = self.home.generation;
        let (scope, origin, query, page) = (
            self.home.scope,
            self.home.origin,
            self.home.search.trim().to_string(),
            self.home.page,
        );
        let (writer, reader, bridge) = (
            self.writer.clone(),
            self.reader.clone(),
            self.bridge.clone(),
        );
        self.rt.spawn(async move {
            xade_runtime::flush(&writer).await;
            let res = if query.is_empty() {
                reader
                    .list_sessions(scope, origin, page, home::PAGE_SIZE)
                    .await
            } else {
                reader
                    .search(scope, origin, query, page, home::PAGE_SIZE)
                    .await
            };
            match res {
                Ok(page) => bridge.send(UiMsg::Cards { generation, page }),
                Err(e) => read_failed(&bridge, "sessions", &e),
            }
        });
    }

    /// Lists or searches every session for the picker's query.
    pub fn refresh_picker(&mut self) {
        let Some(picker) = &self.picker else { return };
        self.picker_generation += 1;
        let generation = self.picker_generation;
        let query = picker.query.trim().to_string();
        let (reader, bridge) = (self.reader.clone(), self.bridge.clone());
        self.rt.spawn(async move {
            let size = session_picker::PAGE_SIZE;
            let res = if query.is_empty() {
                reader.list_sessions(None, None, 0, size).await
            } else {
                reader.search(None, None, query, 0, size).await
            };
            match res {
                Ok(page) => bridge.send(UiMsg::PickerCards { generation, page }),
                Err(e) => read_failed(&bridge, "sessions", &e),
            }
        });
    }

    fn load_saved_quota(&self) {
        let (reader, bridge) = (self.reader.clone(), self.bridge.clone());
        self.rt.spawn(async move {
            match reader.load_quota().await {
                Ok(q) => bridge.send(UiMsg::SavedQuota(q)),
                Err(e) => read_failed(&bridge, "usage limits", &e),
            }
        });
    }

    /// Imports every provider's history; one run at a time, finished by the first `ImportDone`.
    pub fn start_import(&mut self) {
        if self.home.importing {
            return;
        }
        self.home.importing = self.cmd_tx.send(ManagerCmd::Import).is_ok();
    }

    /// The Refresh button: imports every provider's history now and re-reads the lists, the
    /// running sessions included, without waiting for their timers.
    pub fn refresh_sessions(&mut self) {
        self.start_import();
        self.refresh_workspaces();
        self.refresh_cards();
        self.running_checked = None;
    }

    /// Asks each available provider for its quota every `QUOTA_POLL` and after its turns; one ask
    /// per provider in flight. Providers that report quota in events answer with nothing.
    fn poll_provider_quota(&mut self) {
        for p in self.providers.clone() {
            let id = p.id();
            let due = self
                .quota_checked
                .get(&id)
                .is_none_or(|t| t.elapsed() >= QUOTA_POLL);
            if !due || self.quota_inflight.contains(&id) || !self.available(id) {
                continue;
            }
            self.quota_checked.insert(id, Instant::now());
            self.quota_inflight.insert(id);
            let bridge = self.bridge.clone();
            self.rt
                .spawn_blocking(move || bridge.send(UiMsg::ProviderQuota(id, p.quota())));
        }
    }

    /// Polls running sessions every `running::POLL`; one poll in flight at a time.
    fn poll_running(&mut self) {
        let due = self
            .running_checked
            .is_none_or(|t| t.elapsed() >= running::POLL);
        if !due || self.running_inflight {
            return;
        }
        if self.providers.is_empty() {
            return;
        }
        let providers = self.providers.clone();
        self.running_checked = Some(Instant::now());
        self.running_inflight = true;
        let live = self.live_provider_ids();
        let jobs: Vec<running::TranscriptJob> = self
            .sessions
            .values()
            .filter(|vm| live_status(&vm.status))
            .filter_map(|vm| {
                Some(running::TranscriptJob {
                    id: vm.session.id,
                    provider_session_id: vm.session.provider_session_id.clone()?,
                    path: vm.sync.transcript.path.clone(),
                })
            })
            .collect();
        let bridge = self.bridge.clone();
        self.rt.spawn_blocking(move || {
            let found = providers
                .iter()
                .flat_map(|p| p.running_sessions())
                .collect();
            let list = running::exclude_own(found, &live, running::is_own_process);
            let transcripts = running::stat_transcripts(jobs);
            bridge.send(UiMsg::Running { list, transcripts });
        });
    }

    /// Provider session ids Xade runs itself: its live sessions and its fork snapshot turns.
    fn live_provider_ids(&self) -> HashSet<String> {
        self.sessions
            .values()
            .filter(|vm| live_status(&vm.status))
            .filter_map(|vm| vm.session.provider_session_id.clone())
            .chain(self.throwaways.iter().cloned())
            .collect()
    }

    /// Stores a poll: the fork state of Xade's live sessions, the other sessions as running
    /// elsewhere, and a queued re-import when that set changed.
    fn on_running(&mut self, list: Vec<RunningSession>, transcripts: Vec<running::TranscriptStat>) {
        self.running_inflight = false;
        let own = self.live_provider_ids();
        for vm in self.sessions.values_mut() {
            vm.sync.elsewhere = running::elsewhere(
                vm.session.provider_session_id.as_deref(),
                live_status(&vm.status),
                &list,
            )
            .map(|r| r.location.clone());
        }
        for t in transcripts {
            if let Some(vm) = self.sessions.get_mut(&t.id) {
                vm.sync.transcript.path = t.path;
                vm.sync.transcript.mtime = t.mtime;
            }
        }
        let next: HashMap<String, RunningSession> = list
            .into_iter()
            .filter(|r| !own.contains(&r.provider_session_id))
            .map(|r| (r.provider_session_id.clone(), r))
            .collect();
        let changed =
            next.len() != self.running.len() || next.keys().any(|k| !self.running.contains_key(k));
        // The first poll is the baseline; startup already imports.
        if changed {
            tracing::info!(running = next.len(), "running sessions changed");
        }
        if changed && self.running_polled {
            self.running_import_due = true;
        }
        self.running_polled = true;
        self.running = next;
    }

    /// Closes a session and reopens it once an import has brought in what other processes wrote.
    pub fn reload_session(&mut self, id: SessionId) {
        self.close_session(id);
        self.reloading.insert(id);
        self.start_import();
    }

    /// Runs the queued re-import once the 30 s gap has passed and no import is in flight.
    fn run_due_import(&mut self) {
        let gap_over = self
            .running_import_at
            .is_none_or(|t| t.elapsed() >= RUNNING_IMPORT_GAP);
        if self.running_import_due && !self.home.importing && gap_over {
            self.running_import_due = false;
            self.running_import_at = Some(Instant::now());
            self.start_import();
        }
    }

    /// The process running this session outside Xade, unless Xade has it live itself.
    pub fn running_for(&self, card: &SessionCard) -> Option<&RunningSession> {
        let id = card.session_id()?;
        if self.is_live(id) {
            return None;
        }
        self.running.get(card.provider_session_id.as_deref()?)
    }

    /// Opens a card's session or multi-session, asking first when a standalone session is running
    /// elsewhere.
    pub fn request_open(&mut self, card: &SessionCard, background: bool) {
        match card.target {
            xade_store::CardTarget::Session(id) => match self.running_for(card) {
                Some(r) => {
                    self.pending_open = Some(PendingOpen {
                        id,
                        location: r.location.clone(),
                        background,
                    });
                }
                None => self.open_confirmed(id, background),
            },
            xade_store::CardTarget::Multi(mid) => {
                if background {
                    self.open_multi_session_background(mid);
                } else {
                    self.open_multi_session(mid);
                }
            }
        }
    }

    /// Asks for confirmation before deleting a card's session or multi-session.
    pub fn request_delete(&mut self, card: &SessionCard) {
        let target = match card.target {
            xade_store::CardTarget::Session(id) => PendingDeleteTarget::Session(id),
            xade_store::CardTarget::Multi(id) => PendingDeleteTarget::Multi(id),
        };
        self.pending_delete = Some(PendingDelete {
            target,
            title: card.title.clone(),
            provider: card.provider_id,
        });
    }

    pub fn provider_models(&self, id: ProviderId) -> Vec<ModelOption> {
        self.provider(id).map_or_else(Vec::new, |p| p.models())
    }

    pub fn provider_default_model(&self, id: ProviderId) -> String {
        self.provider(id)
            .map_or_else(|| "default".into(), |p| p.default_model())
    }

    pub fn model_effort_levels(&self, id: ProviderId, model_id: &str) -> Vec<Effort> {
        let Some(p) = self.provider(id) else {
            return vec![Effort::Default];
        };
        let mut levels = vec![Effort::Default];
        if let Some(m) = p.models().into_iter().find(|m| m.id == model_id) {
            for e in m.effort_levels {
                if !levels.contains(&e) {
                    levels.push(e);
                }
            }
        } else {
            for e in p.effort_levels() {
                if !levels.contains(&e) {
                    levels.push(e);
                }
            }
        }
        levels
    }

    /// Opens the "New Multi-Session" dialog for the currently selected workspace.
    pub fn open_multi_create(&mut self) {
        let Some(ws_id) = self.home.scope else {
            return;
        };
        let Some(cwd) = self
            .home
            .workspaces
            .iter()
            .find(|w| w.id == ws_id && w.is_open)
            .map(|w| w.root_path.clone())
        else {
            return;
        };
        self.multi_create = MultiCreateState::default_for(self, ws_id, cwd);
    }

    /// Opens a stored multi-session and focuses its composite window.
    pub fn open_multi_session(&mut self, id: MultiSessionId) {
        self.background_multi.remove(&id);
        if !self.multi_sessions.contains_key(&id) {
            let _ = self.cmd_tx.send(ManagerCmd::OpenMulti { multi_id: id });
        }
        self.focus(WindowKind::MultiSession(id));
    }

    /// Opens a stored multi-session as a window without leaving Home.
    pub fn open_multi_session_background(&mut self, id: MultiSessionId) {
        if self.multi_sessions.contains_key(&id) {
            self.add_window(WindowKind::MultiSession(id));
        } else if self.background_multi.insert(id) {
            let _ = self.cmd_tx.send(ManagerCmd::OpenMulti { multi_id: id });
        }
    }

    pub fn close_multi_session(&mut self, id: MultiSessionId) {
        let _ = self.cmd_tx.send(ManagerCmd::CloseMulti { multi_id: id });
        self.drop_multi_session(id);
    }

    fn drop_multi_session(&mut self, id: MultiSessionId) {
        self.background_multi.remove(&id);
        self.restoring.remove(&Saved::MultiSession(id));
        if let Some(vm) = self.multi_sessions.remove(&id) {
            for sid in vm.member_ids {
                self.sessions.remove(&sid);
            }
        }
        self.drop_window(WindowKind::MultiSession(id));
    }

    pub fn rename_multi_session(&mut self, id: MultiSessionId, title: String) {
        let title = title.trim().to_string();
        let Some(vm) = self.multi_sessions.get_mut(&id) else {
            return;
        };
        if title.is_empty() || vm.multi.title == title {
            return;
        }
        vm.multi.title.clone_from(&title);
        for card in self
            .home
            .cards
            .iter_mut()
            .filter(|c| c.multi_id() == Some(id))
        {
            card.title.clone_from(&title);
        }
        let _ = self.cmd_tx.send(ManagerCmd::RenameMulti {
            multi_id: id,
            title,
        });
    }

    pub fn open_confirmed(&mut self, id: SessionId, background: bool) {
        if background {
            self.open_session_background(id);
        } else {
            self.open_session(id);
        }
    }

    /// Once per run, when every startup check has reported: opens the guide if none passed.
    fn offer_guide(&mut self) {
        if self.guide_offered || !self.home.pending.is_empty() {
            return;
        }
        self.guide_offered = true;
        if !self.any_available() {
            self.open_guide();
        }
    }

    pub fn open_guide(&mut self) {
        self.guide.get_or_insert_with(GuideVm::default);
        self.focus(WindowKind::Guide);
    }

    pub fn close_guide(&mut self) {
        self.guide = None;
        self.drop_window(WindowKind::Guide);
    }

    /// Re-runs every failed preflight, showing those providers as checking until they report.
    pub fn recheck_providers(&mut self) {
        let failed: Vec<_> = self
            .providers
            .iter()
            .map(|p| p.id())
            .filter(|id| !self.available(*id) && !self.home.pending.contains(id))
            .collect();
        for id in failed {
            self.home.pending.insert(id);
            self.retry_preflight(id);
        }
    }

    /// Re-runs only `id`'s preflight, off the UI thread.
    pub fn retry_preflight(&self, id: ProviderId) {
        let (provider, bridge) = (self.provider(id).cloned(), self.bridge.clone());
        self.rt.spawn_blocking(move || {
            if let Some(p) = provider {
                bridge.send(UiMsg::Preflight(id, p.preflight()));
            }
        });
    }

    fn provider(&self, id: ProviderId) -> Option<&Arc<dyn AgentProvider>> {
        self.providers.iter().find(|p| p.id() == id)
    }

    /// The provider's name for messages; the id has no name of its own.
    pub fn provider_name(&self, id: ProviderId) -> &'static str {
        self.provider(id).map_or("Provider", |p| p.display_name())
    }

    /// Whether `id` passed its preflight (not one still running in the background).
    pub fn available(&self, id: ProviderId) -> bool {
        !self.home.pending.contains(&id)
            && matches!(self.home.preflight.get(&id), None | Some(Ok(())))
    }

    pub fn any_available(&self) -> bool {
        self.providers.iter().any(|p| self.available(p.id()))
    }

    /// Why `id` failed its preflight, or `None` when it did not.
    pub fn preflight_error(&self, id: ProviderId) -> Option<String> {
        self.home
            .preflight
            .get(&id)?
            .as_ref()
            .err()
            .map(|e| e.to_string())
    }

    /// `preflight_error` naming the provider, or `None` when it is available.
    pub fn unavailable_text(&self, id: ProviderId) -> Option<String> {
        let err = self.preflight_error(id)?;
        Some(format!(
            "{} is not available: {err}",
            self.provider_name(id)
        ))
    }

    /// Providers the UI shows (status bar, banner): every one whose CLI is installed and whose
    /// preflight is done. When none is, only the first stays, so a machine without any CLI sees
    /// today's Claude banner.
    pub fn visible_providers(&self) -> Vec<ProviderId> {
        let installed = |p: &&Arc<dyn AgentProvider>| {
            !self.home.pending.contains(&p.id())
                && !matches!(
                    self.home.preflight.get(&p.id()),
                    Some(Err(ProviderError::NotInstalled))
                )
        };
        let shown: Vec<_> = self.providers.iter().filter(installed).collect();
        match shown.is_empty() {
            true => self.providers.iter().take(1).map(|p| p.id()).collect(),
            false => shown.iter().map(|p| p.id()).collect(),
        }
    }

    /// The providers a new session can use, in the app's provider order.
    pub fn selectable_providers(&self) -> Vec<ProviderId> {
        let mut ids: Vec<_> = self.providers.iter().map(|p| p.id()).collect();
        ids.retain(|p| self.available(*p));
        ids
    }

    /// The provider the next new session uses: the user's pick, else the latest Xade
    /// session's, else the first available one. `None` when none is available.
    pub fn new_provider(&self) -> Option<ProviderId> {
        let ids = self.selectable_providers();
        let usable = |p: &ProviderId| ids.contains(p);
        self.home
            .provider_pick
            .filter(usable)
            .or(self.home.last_provider.filter(usable))
            .or_else(|| ids.first().copied())
    }

    fn load_last_provider(&self) {
        let (reader, bridge) = (self.reader.clone(), self.bridge.clone());
        self.rt.spawn(async move {
            match reader.last_provider().await {
                Ok(p) => bridge.send(UiMsg::LastProvider(p)),
                Err(e) => read_failed(&bridge, "last provider", &e),
            }
        });
    }

    /// The provider of a session the UI already knows (open, or on the current Home page).
    fn provider_of(&self, id: SessionId) -> Option<ProviderId> {
        self.sessions
            .get(&id)
            .map(|vm| vm.session.provider_id)
            .or_else(|| {
                self.home
                    .cards
                    .iter()
                    .find(|c| c.session_id() == Some(id))
                    .map(|c| c.provider_id)
            })
    }

    /// Warns and returns true when session `id`'s provider is known and unavailable. A session
    /// whose provider is not known yet (a restored window) opens dormant and fails on Send.
    fn blocked_by_provider(&mut self, id: SessionId) -> bool {
        let Some(text) = self.provider_of(id).and_then(|p| self.unavailable_text(p)) else {
            return false;
        };
        self.warn(text);
        true
    }

    pub fn add_directory(&self) {
        let (writer, reader, bridge) = (
            self.writer.clone(),
            self.reader.clone(),
            self.bridge.clone(),
        );
        self.rt.spawn(async move {
            let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await else {
                return;
            };
            add_workspace(&writer, &reader, &bridge, folder.path().to_path_buf()).await;
        });
    }

    /// The guide's last step: pick a folder, then `open_folder` it.
    pub fn open_chosen_folder(&self) {
        let (writer, reader, bridge) = (
            self.writer.clone(),
            self.reader.clone(),
            self.bridge.clone(),
        );
        self.rt.spawn(async move {
            let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await else {
                return;
            };
            let root = folder.path().to_path_buf();
            if let Some(id) = add_workspace(&writer, &reader, &bridge, root).await {
                bridge.send(UiMsg::OpenWorkspace(id));
            }
        });
    }

    /// `xade <path>`: adds the folder like Add Directory, then selects it and starts a session.
    fn open_folder(&self, root_path: PathBuf) {
        let (writer, reader, bridge) = (
            self.writer.clone(),
            self.reader.clone(),
            self.bridge.clone(),
        );
        self.rt.spawn(async move {
            if let Some(id) = add_workspace(&writer, &reader, &bridge, root_path).await {
                bridge.send(UiMsg::OpenWorkspace(id));
            }
        });
    }

    /// Takes a workspace out of the sidebar; its sessions stay on Home.
    pub fn close_workspace(&mut self, id: WorkspaceId) {
        if self.home.scope == Some(id) {
            self.home.scope = None;
            self.home.page = 0;
            self.refresh_cards();
        }
        let (writer, reader, bridge) = (
            self.writer.clone(),
            self.reader.clone(),
            self.bridge.clone(),
        );
        self.rt.spawn(async move {
            if let Err(e) = writer.send(WriteOp::CloseWorkspace(id)) {
                let report = ErrorReport::new("Could not close the workspace", e.to_string());
                return bridge.send(UiMsg::Report(report));
            }
            xade_runtime::flush(&writer).await;
            match reader.list_workspaces().await {
                Ok(ws) => bridge.send(UiMsg::Workspaces(ws)),
                Err(e) => read_failed(&bridge, "workspaces", &e),
            }
        });
    }

    /// At most one run per directory; a request during a run triggers one more afterwards.
    fn refresh_git(&mut self, dir: PathBuf) {
        if !self.git_inflight.insert(dir.clone()) {
            self.git_stale.insert(dir);
            return;
        }
        let bridge = self.bridge.clone();
        self.rt.spawn(async move {
            let state = git_status(&dir).await;
            bridge.send(UiMsg::App(Box::new(AppEvent {
                session_id: None,
                kind: AppEventKind::Git { dir, state },
            })));
        });
    }

    // ----- actions -----

    pub fn new_session(&mut self) {
        let (Some(ws_id), Some(provider_id)) = (self.home.scope, self.new_provider()) else {
            return;
        };
        let Some(provider) = self.provider(provider_id) else {
            return;
        };
        let Some(ws) = self.home.workspaces.iter().find(|w| w.id == ws_id) else {
            return;
        };
        // New sessions start with the model, effort and Auto mode of the last one of the same
        // provider, across restarts.
        let (cmd_tx, reader) = (self.cmd_tx.clone(), self.reader.clone());
        let (cwd, default_model) = (ws.root_path.clone(), provider.default_model());
        let (models, levels) = (provider.models(), provider.effort_levels());
        self.rt.spawn(async move {
            let last = reader
                .last_model_choice(provider_id)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "could not read the last model choice");
                    None
                });
            let (model_id, effort, auto_mode) =
                last.unwrap_or((default_model, xade_core::Effort::Default, false));
            let supported = composer::effort_choices(&models, &model_id, &levels);
            let effort = if supported.contains(&effort) {
                effort
            } else {
                xade_core::Effort::Default
            };
            let _ = cmd_tx.send(ManagerCmd::Create {
                workspace_id: ws_id,
                cwd,
                provider: provider_id,
                model_id,
                effort,
                auto_mode,
            });
        });
    }

    /// Opens a stored session (spawning it when not live) and shows it.
    pub fn open_session(&mut self, id: SessionId) {
        if self.blocked_by_provider(id) {
            return;
        }
        self.background.remove(&id);
        if !self.is_live(id) {
            let _ = self.cmd_tx.send(ManagerCmd::Open { session_id: id });
        }
        self.focus(WindowKind::Session(id));
    }

    /// Opens a stored session as a window without leaving the current view (Cmd+click on Home).
    pub fn open_session_background(&mut self, id: SessionId) {
        if self.blocked_by_provider(id) {
            return;
        }
        if self.is_live(id) {
            self.add_window(WindowKind::Session(id));
        } else if self.background.insert(id) {
            let _ = self.cmd_tx.send(ManagerCmd::Open { session_id: id });
        }
    }

    fn is_live(&self, id: SessionId) -> bool {
        self.sessions
            .get(&id)
            .is_some_and(|vm| live_status(&vm.status))
    }

    pub fn window_exists(&self, kind: WindowKind) -> bool {
        match kind {
            WindowKind::Session(id) => self.sessions.contains_key(&id),
            WindowKind::MultiSession(id) => self.multi_sessions.contains_key(&id),
            WindowKind::Editor(id) => self.editors.map.contains_key(&id),
            WindowKind::Guide => self.guide.is_some(),
        }
    }

    fn add_window(&mut self, kind: WindowKind) {
        if !self.open_windows.contains(&kind) {
            // The guide has done its job once a chat opens.
            let chat = matches!(kind, WindowKind::Session(_) | WindowKind::MultiSession(_));
            if chat && self.guide.take().is_some() {
                self.open_windows.retain(|k| *k != WindowKind::Guide);
                if self.focused == Some(WindowKind::Guide) {
                    self.focused = Some(kind);
                }
            }
            self.open_windows.push(kind);
            self.maximized = None;
            self.tiling.retile();
            self.save_windows();
        }
    }

    /// Reopens the windows saved by the last plain launch (tiled, in order) and keeps saving.
    /// With no working provider nothing reopens and nothing is saved, so the list survives;
    /// with `restore_windows` off nothing reopens and the saved list is emptied on the next change.
    fn restore_windows(&mut self, file: PathBuf) {
        if !self.any_available() {
            // A provider still checking may make it possible once it reports.
            if !self.home.pending.is_empty() {
                self.deferred_restore = Some(file);
            }
            return;
        }
        let saved = crate::windows_state::load(&file);
        self.windows_file = Some(file);
        let reopen = self.config.restore_windows;
        for w in saved.iter().filter(|_| reopen) {
            self.restoring.insert(w.clone());
            match w {
                Saved::Session(id) => self.open_session(*id),
                Saved::MultiSession(id) => self.open_multi_session(*id),
                Saved::File(path) => self.open_path(path.clone()),
            }
        }
        self.windows_saved = saved;
    }

    /// Writes the open windows when they changed (unsaved new files are not kept).
    fn save_windows(&mut self) {
        let Some(file) = self.windows_file.clone() else {
            return;
        };
        let now: Vec<Saved> = self
            .open_windows
            .iter()
            .filter(|_| self.config.restore_windows)
            .filter_map(|k| match k {
                WindowKind::Session(id) => Some(Saved::Session(*id)),
                WindowKind::MultiSession(id) => Some(Saved::MultiSession(*id)),
                WindowKind::Editor(id) => self.editors.map.get(id)?.path.clone().map(Saved::File),
                WindowKind::Guide => None,
            })
            .collect();
        if now == self.windows_saved {
            return;
        }
        self.windows_saved = now.clone();
        crate::windows_state::save_later(&self.rt, file, now);
    }

    pub fn focus(&mut self, kind: WindowKind) {
        self.add_window(kind);
        self.focused = Some(kind);
        self.raise = Some(kind);
        self.view = View::Workspace;
    }

    /// The focused window when it is a live session.
    pub(crate) fn focused_session(&self) -> Option<SessionId> {
        self.focused
            .and_then(WindowKind::session)
            .filter(|id| self.sessions.contains_key(id))
    }

    /// The focused window when it is an open multi-session.
    pub(crate) fn focused_multi_session(&self) -> Option<MultiSessionId> {
        match self.focused {
            Some(WindowKind::MultiSession(id)) if self.multi_sessions.contains_key(&id) => Some(id),
            _ => None,
        }
    }

    pub fn close_session(&mut self, id: SessionId) {
        let _ = self.cmd_tx.send(ManagerCmd::Close { session_id: id });
        self.drop_session(id);
    }

    fn drop_session(&mut self, id: SessionId) {
        self.sessions.remove(&id);
        self.drop_window(WindowKind::Session(id));
    }

    fn drop_window(&mut self, kind: WindowKind) {
        self.open_windows.retain(|k| *k != kind);
        if self.maximized == Some(kind) {
            self.maximized = None;
        }
        self.tiling.retile();
        if self.focused == Some(kind) {
            self.focused = self.open_windows.last().copied();
        }
        if self.open_windows.is_empty() {
            self.view = View::Home;
        }
        self.save_windows();
    }

    // ----- editors -----

    /// Where file dialogs start: the selected workspace, else the focused session, else home.
    pub fn target_dir(&self) -> PathBuf {
        let selected = self
            .home
            .scope
            .and_then(|id| self.home.workspaces.iter().find(|w| w.id == id))
            .map(|w| w.root_path.clone());
        selected
            .or_else(|| {
                self.focused_session()
                    .and_then(|id| self.sessions.get(&id))
                    .map(|vm| vm.cwd.clone())
            })
            .or_else(|| {
                self.focused_multi_session()
                    .and_then(|id| self.multi_sessions.get(&id))
                    .map(|vm| vm.cwd.clone())
            })
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Lists the session folder's files in the background for `@` autocomplete.
    pub fn index_files(&mut self, session: SessionId) {
        let Some(vm) = self.sessions.get_mut(&session) else {
            return;
        };
        vm.mention.files = crate::mention::Files::Loading;
        let (root, bridge) = (vm.cwd.clone(), self.bridge.clone());
        self.rt.spawn(async move {
            let listed =
                tokio::task::spawn_blocking(move || crate::mention::list_files(&root)).await;
            match listed {
                Ok(files) => bridge.send(UiMsg::Files { session, files }),
                Err(e) => {
                    let report = ErrorReport::new("Could not list files", e.to_string());
                    bridge.send(UiMsg::Report(report));
                    bridge.send(UiMsg::Files {
                        session,
                        files: Vec::new(),
                    });
                }
            }
        });
    }

    /// Opens the file quick-open over the selected workspace, or every open one when none is
    /// selected, listing their files in the background.
    fn open_file_picker(&mut self) {
        let roots: Vec<(String, PathBuf)> = self
            .home
            .workspaces
            .iter()
            .filter(|w| w.is_open && self.home.scope.is_none_or(|id| id == w.id))
            .map(|w| (w.display_name.clone(), w.root_path.clone()))
            .collect();
        let paths: Vec<PathBuf> = roots.iter().map(|(_, p)| p.clone()).collect();
        self.file_picker = Some(FilePicker::new(roots, self.home.scope.is_some()));
        if paths.is_empty() {
            return;
        }
        let bridge = self.bridge.clone();
        self.rt.spawn(async move {
            let walk = paths.clone();
            let listed = tokio::task::spawn_blocking(move || {
                walk.iter()
                    .map(|p| crate::mention::list_files(p))
                    .collect::<Vec<_>>()
            })
            .await;
            match listed {
                Ok(files) => bridge.send(UiMsg::PickerFiles {
                    roots: paths,
                    files,
                }),
                Err(e) => {
                    let report = ErrorReport::new("Could not list files", e.to_string());
                    bridge.send(UiMsg::Report(report));
                    bridge.send(UiMsg::PickerFiles {
                        files: vec![Vec::new(); paths.len()],
                        roots: paths,
                    });
                }
            }
        });
    }

    /// Encodes and saves a pasted clipboard image off the UI thread; it comes back as an
    /// `Attached` mention.
    pub fn attach_image(&self, session: SessionId, image: std::sync::Arc<egui::ColorImage>) {
        let bridge = self.bridge.clone();
        self.rt.spawn(async move {
            let saved =
                tokio::task::spawn_blocking(move || crate::attach::save_pasted_image(&image)).await;
            match saved {
                Ok(Ok(path)) => bridge.send(UiMsg::Attached {
                    session,
                    paths: vec![path],
                }),
                Ok(Err(message)) => bridge.send(UiMsg::AttachFailed { session, message }),
                Err(e) => bridge.send(UiMsg::AttachFailed {
                    session,
                    message: format!("Could not save the image: {e}"),
                }),
            }
        });
    }

    /// A multi-file dialog starting in the session's folder; the picks become mentions.
    pub fn attach_dialog(&self, session: SessionId) {
        let Some(vm) = self.sessions.get(&session) else {
            return;
        };
        let (dir, bridge) = (vm.cwd.clone(), self.bridge.clone());
        self.rt.spawn(async move {
            let Some(files) = rfd::AsyncFileDialog::new()
                .set_directory(dir)
                .pick_files()
                .await
            else {
                return;
            };
            let paths = files.iter().map(|f| f.path().to_path_buf()).collect();
            bridge.send(UiMsg::Attached { session, paths });
        });
    }

    pub fn new_editor(&mut self) {
        let id = self.editors.create(None, String::new());
        self.focus(WindowKind::Editor(id));
    }

    /// Cmd+O: a file dialog, then the read, both off the UI thread.
    pub fn open_file_dialog(&self) {
        let (dir, bridge) = (self.target_dir(), self.bridge.clone());
        self.rt.spawn(async move {
            let Some(file) = rfd::AsyncFileDialog::new()
                .set_directory(dir)
                .pick_file()
                .await
            else {
                return;
            };
            let path = file.path().to_path_buf();
            let read = path.clone();
            let content = tokio::task::spawn_blocking(move || editor::read_entry(&read))
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            bridge.send(UiMsg::FileOpened { path, content });
        });
    }

    fn on_file_opened(&mut self, path: PathBuf, content: Result<editor::FileContent, String>) {
        let restoring = self.restoring.remove(&Saved::File(path.clone()));
        match content {
            Err(e) if restoring => {
                tracing::warn!(error = %e, "skipping a window that did not reopen")
            }
            Err(e) => self.report(ErrorReport::new("Could not open the file", e)),
            Ok(content) => {
                let id = match self.editors.find_path(&path) {
                    Some(id) => id,
                    None => match content {
                        editor::FileContent::Text(text) => self.editors.create(Some(path), text),
                        editor::FileContent::Image(pixels) => {
                            self.editors.create_image(path, pixels)
                        }
                    },
                };
                self.focus(WindowKind::Editor(id));
            }
        }
    }

    fn on_image_reloaded(&mut self, id: EditorId, result: Result<egui::ColorImage, String>) {
        let Some(vm) = self.editors.map.get_mut(&id) else {
            return;
        };
        match result {
            Ok(pixels) => vm.set_image(pixels),
            Err(e) => {
                if let Some(img) = vm.image.as_mut() {
                    img.reloading = false;
                }
                self.warn(e);
            }
        }
    }

    /// Writes the editor's text, asking for a path first when it has none or `as_new`.
    pub fn save_editor(&mut self, id: EditorId, as_new: bool, close_after: bool) {
        let dir = self.target_dir();
        let Some(vm) = self
            .editors
            .map
            .get_mut(&id)
            .filter(|vm| !vm.busy && !vm.is_image())
        else {
            return;
        };
        vm.busy = true;
        vm.close_after_save = close_after;
        let (text, revision, name) = (vm.text.clone(), vm.revision, vm.title());
        let known = vm.path.clone().filter(|_| !as_new);
        let start = vm
            .path
            .as_deref()
            .and_then(|p| p.parent())
            .map_or(dir, |p| p.to_path_buf());
        let bridge = self.bridge.clone();
        self.rt.spawn(async move {
            let path = match known {
                Some(p) => p,
                None => {
                    let picked = rfd::AsyncFileDialog::new()
                        .set_directory(start)
                        .set_file_name(name)
                        .save_file()
                        .await;
                    match picked {
                        Some(f) => f.path().to_path_buf(),
                        None => {
                            let outcome = SaveOutcome::Cancelled;
                            bridge.send(UiMsg::Saved {
                                id,
                                revision,
                                outcome,
                            });
                            return;
                        }
                    }
                }
            };
            let target = path.clone();
            let written =
                tokio::task::spawn_blocking(move || editor::write_atomic(&target, &text)).await;
            let outcome = match written {
                Ok(Ok(())) => SaveOutcome::Saved(path),
                Ok(Err(e)) => SaveOutcome::Failed(e.to_string()),
                Err(e) => SaveOutcome::Failed(e.to_string()),
            };
            bridge.send(UiMsg::Saved {
                id,
                revision,
                outcome,
            });
        });
    }

    fn on_saved(&mut self, id: EditorId, revision: u64, outcome: SaveOutcome) {
        let Some(vm) = self.editors.map.get_mut(&id) else {
            return;
        };
        vm.busy = false;
        let close = std::mem::take(&mut vm.close_after_save);
        match outcome {
            SaveOutcome::Saved(path) => {
                if vm.revision == revision {
                    vm.dirty = false;
                }
                vm.disk_mtime = editor::mtime(&path);
                vm.disk_changed = false;
                vm.path = Some(path);
                if close && !vm.dirty {
                    self.close_editor(id);
                }
            }
            SaveOutcome::Cancelled => {}
            SaveOutcome::Failed(e) => self.report(ErrorReport::new("Could not save the file", e)),
        }
    }

    /// Closes at once when clean; a dirty editor asks first.
    pub fn request_close_editor(&mut self, id: EditorId) {
        match self.editors.map.get(&id) {
            Some(vm) if vm.dirty => {
                self.editors.pending_close = Some(id);
                self.focus(WindowKind::Editor(id));
            }
            Some(_) => self.close_editor(id),
            None => {}
        }
    }

    fn close_editor(&mut self, id: EditorId) {
        self.editors.map.remove(&id);
        if self.editors.pending_close == Some(id) {
            self.editors.pending_close = None;
        }
        self.drop_window(WindowKind::Editor(id));
    }

    /// Draws the "Save changes?" modal while an editor waits to close.
    fn editor_modal(&mut self, ctx: &egui::Context) {
        let Some(id) = self.editors.pending_close else {
            return;
        };
        match self.editors.map.get(&id).map(|vm| vm.title()) {
            Some(name) => {
                if let Some(choice) = editor::close_modal(ctx, &name) {
                    self.resolve_close(id, choice);
                }
            }
            None => self.editors.pending_close = None,
        }
    }

    fn resolve_close(&mut self, id: EditorId, choice: CloseChoice) {
        self.editors.pending_close = None;
        match choice {
            CloseChoice::Save => self.save_editor(id, false, true),
            CloseChoice::DontSave => self.close_editor(id),
            CloseChoice::Cancel => {}
        }
    }

    /// Compares open files with disk every `DISK_POLL`, or at once when `force`.
    fn check_editors(&mut self, ctx: &egui::Context, force: bool) {
        if self.editors.map.is_empty() {
            return;
        }
        ctx.request_repaint_after(editor::DISK_POLL);
        let due = self
            .editors
            .checked
            .is_none_or(|t| t.elapsed() >= editor::DISK_POLL);
        if !(due || force) {
            return;
        }
        self.editors.checked = Some(Instant::now());
        let mut failures = Vec::new();
        let mut image_reloads = Vec::new();
        for (&id, vm) in &mut self.editors.map {
            if let Some(path) = vm.image_needs_reload() {
                image_reloads.push((id, path));
            } else if let Err(e) = vm.check_disk() {
                failures.push(e);
            }
        }
        for (id, path) in image_reloads {
            let bridge = self.bridge.clone();
            self.rt.spawn_blocking(move || {
                let result = editor::read_image(&path);
                bridge.send(UiMsg::ImageReloaded { id, result });
            });
        }
        for e in failures {
            self.warn(e);
        }
    }

    /// The directory the status bar describes.
    pub fn active_dir(&self) -> Option<PathBuf> {
        if self.view == View::Workspace {
            match self.focused {
                Some(WindowKind::Session(id)) => {
                    if let Some(vm) = self.sessions.get(&id) {
                        return Some(vm.cwd.clone());
                    }
                }
                Some(WindowKind::MultiSession(id)) => {
                    if let Some(vm) = self.multi_sessions.get(&id) {
                        return Some(vm.cwd.clone());
                    }
                }
                Some(WindowKind::Editor(id)) => {
                    if let Some(vm) = self.editors.map.get(&id) {
                        let parent = vm.path.as_deref().and_then(|p| p.parent());
                        return Some(parent.map_or_else(|| self.target_dir(), PathBuf::from));
                    }
                }
                Some(WindowKind::Guide) | None => {}
            }
        }
        let ws = &self.home.workspaces;
        match self.home.scope {
            Some(id) => ws.iter().find(|w| w.id == id),
            None => ws.iter().find(|w| w.is_open),
        }
        .map(|w| w.root_path.clone())
    }

    pub fn workspace_name(&self, id: WorkspaceId) -> String {
        self.home
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.display_name.clone())
            .unwrap_or_default()
    }

    /// Open sessions (standalone and multi-session) in the workspace.
    pub fn live_count(&self, id: WorkspaceId) -> usize {
        let standalone = self
            .sessions
            .values()
            .filter(|vm| vm.session.multi_session_id.is_none() && vm.session.workspace_id == id)
            .count();
        let multi = self
            .multi_sessions
            .values()
            .filter(|mvm| mvm.multi.workspace_id == id)
            .count();
        standalone + multi
    }

    /// Runs one shortcut. Actions that need a focused session do nothing without one.
    pub fn run_action(&mut self, action: Action) {
        let focused = self.focused_session();
        let focused_editor = match self.focused {
            Some(WindowKind::Editor(id)) if self.editors.map.contains_key(&id) => Some(id),
            _ => None,
        };
        let in_workspace = self.view == View::Workspace;
        match action {
            Action::NewSession => {
                if self.home.scope.is_none() {
                    self.warn("Select a workspace in the sidebar first.");
                } else {
                    self.new_session();
                }
            }
            Action::NewMultiSession => {
                if self.home.scope.is_none() {
                    self.warn("Select a workspace in the sidebar first.");
                } else if self.selectable_providers().is_empty() {
                    self.warn("No provider is available.");
                } else {
                    self.open_multi_create();
                }
            }
            Action::NewFile => self.new_editor(),
            Action::OpenFile => self.open_file_dialog(),
            Action::Save | Action::SaveAs => {
                if let Some(id) = focused_editor {
                    self.save_editor(id, action == Action::SaveAs, false);
                }
            }
            Action::OpenPicker => {
                if self.picker.take().is_none() {
                    self.picker = Some(PickerState::default());
                    self.refresh_picker();
                }
            }
            Action::SearchSessions => {
                self.view = View::Home;
                self.home.focus_search = true;
            }
            Action::Find => match (&mut self.find, self.focused) {
                (_, None) => self.warn("Focus a window to search in it."),
                (Some(f), _) => f.refocus(),
                (None, _) => self.find = Some(crate::find::FindState::new()),
            },
            Action::QuickOpen => {
                if self.file_picker.take().is_none() {
                    self.open_file_picker();
                }
            }
            Action::Stop => {
                if let Some(vm) = focused.and_then(|id| self.sessions.get(&id)) {
                    if vm.status == SessionStatus::Streaming {
                        let _ = self.cmd_tx.send(ManagerCmd::Interrupt {
                            session_id: vm.session.id,
                        });
                    }
                } else if let Some(mid) = self.focused_multi_session() {
                    let _ = self
                        .cmd_tx
                        .send(ManagerCmd::InterruptMultiAll { multi_id: mid });
                }
            }
            Action::CloseWindow => {
                if let (true, Some(id)) = (in_workspace, focused) {
                    self.close_session(id);
                } else if let (true, Some(mid)) = (in_workspace, self.focused_multi_session()) {
                    self.close_multi_session(mid);
                } else if let (true, Some(id)) = (in_workspace, focused_editor) {
                    self.request_close_editor(id);
                } else if in_workspace && self.focused == Some(WindowKind::Guide) {
                    self.close_guide();
                }
            }
            Action::FocusWindow(n) => {
                let kind = self
                    .open_windows
                    .get(usize::from(n).saturating_sub(1))
                    .copied();
                if let Some(kind) = kind.filter(|k| self.window_exists(*k)) {
                    self.focus(kind);
                }
            }
            Action::NextWindow | Action::PrevWindow => {
                let n = self.open_windows.len();
                let at = self
                    .focused
                    .and_then(|f| self.open_windows.iter().position(|id| *id == f));
                if let (true, Some(at)) = (n > 1, at) {
                    let next = if action == Action::NextWindow {
                        (at + 1) % n
                    } else {
                        (at + n - 1) % n
                    };
                    self.focus(self.open_windows[next]);
                }
            }
            Action::Maximize => {
                if let (true, Some(kind)) = (
                    in_workspace,
                    self.focused.filter(|k| self.window_exists(*k)),
                ) {
                    self.maximized = (self.maximized != Some(kind)).then_some(kind);
                    self.tiling.retile();
                    self.raise = Some(kind);
                }
            }
            Action::ToggleSidebar => self.sidebar_visible = !self.sidebar_visible,
            Action::Retile => self.tiling.retile(),
            Action::ToggleView => {
                if in_workspace {
                    self.view = View::Home;
                    self.refresh_cards();
                } else if !self.open_windows.is_empty() {
                    self.view = View::Workspace;
                }
            }
            Action::ToggleAuto => {
                let target_sid = focused.or_else(|| {
                    let mid = self.focused_multi_session()?;
                    match self.multi_sessions.get(&mid)?.active_tab {
                        MultiTab::Member(sid) => Some(sid),
                        MultiTab::Team => None,
                    }
                });
                if let Some(vm) = target_sid.and_then(|id| self.sessions.get_mut(&id)) {
                    vm.session.auto_mode = !vm.session.auto_mode;
                    let _ = self.cmd_tx.send(ManagerCmd::SetAutoMode {
                        session_id: vm.session.id,
                        on: vm.session.auto_mode,
                    });
                }
            }
            Action::Shortcuts => self.keymaps_open = !self.keymaps_open,
            #[cfg(debug_assertions)]
            Action::DebugPanic => panic!("forced panic (debug key)"),
            #[cfg(debug_assertions)]
            Action::DebugKillWriter => {
                let _ = self.writer.send(WriteOp::Poison);
            }
            Action::OpenSettings => self.open_settings(),
            // Handled by the composer and the modals themselves.
            Action::Send | Action::SendOther | Action::CloseModal => {}
        }
    }

    /// Reads this frame's shortcuts, before any widget can take the keys.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.chord.expire(now);
        let actions = ctx.input_mut(|i| keymap::poll(i, &mut self.chord, now));
        for action in actions {
            self.run_action(action);
        }
        if let Some(left) = self.chord.remaining(now) {
            ctx.request_repaint_after(left);
        }
        let typed_help = ctx.input(|i| {
            i.events
                .iter()
                .any(|e| matches!(e, egui::Event::Text(t) if t == "?"))
        });
        if typed_help && !ctx.egui_wants_keyboard_input() {
            self.keymaps_open = true;
        }
    }

    // ----- event folding -----

    pub(crate) fn on_msg(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::App(ev) => self.on_app_event(*ev),
            UiMsg::Cards { generation, page } => {
                if generation == self.home.generation {
                    self.home.cards = page.items;
                    self.home.total = page.total;
                }
            }
            UiMsg::PickerCards { generation, page } => {
                if let Some(p) = self
                    .picker
                    .as_mut()
                    .filter(|_| generation == self.picker_generation)
                {
                    p.results = page.items;
                    p.selected = p.selected.min(p.results.len().saturating_sub(1));
                }
            }
            UiMsg::PickerFiles { roots, files } => {
                if let Some(p) = self.file_picker.as_mut().filter(|p| p.has_roots(&roots)) {
                    p.set_files(files);
                }
            }
            UiMsg::Running { list, transcripts } => self.on_running(list, transcripts),
            UiMsg::TurnBase { session, base } => {
                if let Some(vm) = self.sessions.get_mut(&session) {
                    // A turn too short to outlive its snapshot has no use for it.
                    if vm.turn_snapshotting {
                        vm.turn_base = base;
                    }
                }
            }
            UiMsg::TurnFiles {
                session,
                anchor,
                changes,
            } => {
                if let Some(vm) = self.sessions.get_mut(&session) {
                    if !changes.files.is_empty() {
                        vm.turn_files.insert(anchor, changes);
                    }
                }
            }
            UiMsg::Files { session, files } => {
                if let Some(vm) = self.sessions.get_mut(&session) {
                    vm.mention.set_files(files);
                }
            }
            UiMsg::AttachFailed { session, message } => {
                if let Some(vm) = self.sessions.get_mut(&session) {
                    composer::set_attach_note(vm, message);
                }
            }
            UiMsg::Attached { session, paths } => {
                if let Some(vm) = self.sessions.get_mut(&session) {
                    composer::insert_mentions(vm, &paths);
                }
            }
            UiMsg::FileOpened { path, content } => self.on_file_opened(path, content),
            UiMsg::ImageReloaded { id, result } => self.on_image_reloaded(id, result),
            UiMsg::DirListed { path, result } => self.tree.loaded(&path, result),
            UiMsg::Saved {
                id,
                revision,
                outcome,
            } => self.on_saved(id, revision, outcome),
            UiMsg::Workspaces(ws) => {
                if self
                    .home
                    .scope
                    .is_some_and(|s| !ws.iter().any(|w| w.id == s && w.is_open))
                {
                    self.home.scope = None;
                }
                self.home.workspaces = ws;
            }
            UiMsg::OpenWorkspace(id) => {
                self.home.scope = Some(id);
                self.home.page = 0;
                self.refresh_cards();
                self.new_session();
            }
            UiMsg::Preflight(id, result) => {
                let recovered = result.is_ok() && !self.available(id);
                self.home.pending.remove(&id);
                self.home.preflight.insert(id, result);
                if recovered {
                    self.start_import();
                    if let Some(file) = self.deferred_restore.take() {
                        self.restore_windows(file);
                    }
                }
                self.offer_guide();
            }
            UiMsg::ProviderQuota(id, result) => {
                self.quota_inflight.remove(&id);
                match result {
                    Ok(groups) => {
                        self.provider_quota.insert(id, groups);
                    }
                    // Keep what was shown; the next poll tries again.
                    Err(e) => tracing::warn!(provider = ?id, error = %e, "quota unavailable"),
                }
            }
            UiMsg::LastProvider(p) => self.home.last_provider = p,
            UiMsg::Report(report) => self.report(report),
            UiMsg::ForkPrompt {
                session,
                provider,
                prompt,
                resume,
                warning,
            } => {
                if let Some(w) = warning {
                    self.warn(w);
                }
                let sent = self.cmd_tx.send(ManagerCmd::Fork {
                    session_id: session,
                    provider,
                    handoff_prompt: prompt,
                    resume_prompt: resume,
                });
                if sent.is_err() {
                    if let Some(vm) = self.sessions.get_mut(&session) {
                        vm.fork_preparing = false;
                    }
                }
            }
            UiMsg::SavedQuota(saved) => {
                // A live report may have arrived first; it wins.
                for q in saved {
                    if !self.quota.iter().any(|(_, x)| x.kind == q.kind) {
                        self.quota.push((QUOTA_PROVIDER, q));
                    }
                }
                self.quota
                    .sort_by_key(|(p, q)| (*p as u8, q.kind == QuotaKind::SevenDay));
            }
        }
    }

    fn build_session_vm(
        &self,
        session: Session,
        messages: Vec<Message>,
        status: SessionStatus,
    ) -> SessionVm {
        let cwd = self
            .home
            .workspaces
            .iter()
            .find(|w| w.id == session.workspace_id)
            .map(|w| w.root_path.clone())
            .unwrap_or_default();
        let sent_model = if session.sent_model_id.is_empty() {
            session.model_id.clone()
        } else {
            session.sent_model_id.clone()
        };
        let sent_effort = if session.sent_model_id.is_empty() {
            session.effort
        } else {
            session.sent_effort
        };
        SessionVm {
            cwd,
            session,
            status,
            queued: Default::default(),
            messages,
            live_text: String::new(),
            thinking_tokens: None,
            pending_permission: None,
            input: String::new(),
            error: None,
            md_cache: crate::widgets::markdown::new_cache(),
            path_links: PathLinks::default(),
            diff: None,
            changes: Default::default(),
            settings_pending: false,
            turn_base: None,
            turn_snapshotting: false,
            turn_files: Default::default(),
            sent_model,
            sent_effort,
            collapsed: false,
            window_rect: None,
            apply_rect: None,
            sync: Default::default(),
            mention: Default::default(),
            attach_note: None,
            focus_input: false,
            subagent_msgs: Vec::new(),
            view_subagents: false,
            fork_preparing: false,
            renaming: None,
            scroll_to_end: false,
        }
    }

    fn insert_multi_vm(
        &mut self,
        multi: xade_core::MultiSession,
        members: Vec<(Session, Vec<Message>)>,
        team_messages: Vec<xade_core::TeamMessage>,
    ) {
        let mid = multi.id;
        let cwd = self
            .home
            .workspaces
            .iter()
            .find(|w| w.id == multi.workspace_id)
            .map(|w| w.root_path.clone())
            .unwrap_or_default();
        let mut member_ids = Vec::with_capacity(members.len());
        for (session, messages) in members {
            let sid = session.id;
            member_ids.push(sid);
            let vm = self.build_session_vm(session, messages, SessionStatus::Idle);
            self.sessions.insert(sid, vm);
        }
        let mvm = MultiSessionVm::new(multi, cwd, member_ids, team_messages);
        self.multi_sessions.insert(mid, mvm);
        if self.background_multi.remove(&mid) {
            self.add_window(WindowKind::MultiSession(mid));
        } else {
            self.focus(WindowKind::MultiSession(mid));
        }
    }

    fn insert_vm(&mut self, session: Session, messages: Vec<Message>) {
        let id = session.id;
        let vm = self.build_session_vm(session, messages, SessionStatus::Starting);
        self.sessions.insert(id, vm);
        if self.background.remove(&id) {
            self.add_window(WindowKind::Session(id));
        } else {
            self.focus(WindowKind::Session(id));
        }
    }

    fn on_app_event(&mut self, ev: AppEvent) {
        let AppEvent { session_id, kind } = ev;
        // A fork ends with one of these two for its source; unrelated errors leave it alone.
        match &kind {
            AppEventKind::ForkPreparing { from } => self.set_fork_preparing(*from, true),
            AppEventKind::SessionForked { from, .. } | AppEventKind::ForkFailed { from, .. } => {
                self.set_fork_preparing(*from, false)
            }
            _ => {}
        }
        match (session_id, kind) {
            (
                _,
                AppEventKind::Throwaway {
                    provider_session_id,
                    active,
                },
            ) => {
                if active {
                    self.throwaways.insert(provider_session_id);
                } else {
                    self.throwaways.remove(&provider_session_id);
                }
            }
            (_, AppEventKind::ForkFailed { message, .. }) => {
                self.report(ErrorReport::new("Something went wrong", message))
            }
            (_, AppEventKind::Quota(q)) => {
                self.quota.retain(|(_, x)| x.kind != q.kind);
                self.quota.push((QUOTA_PROVIDER, q));
                self.quota
                    .sort_by_key(|(p, q)| (*p as u8, q.kind == QuotaKind::SevenDay));
            }
            (_, AppEventKind::ImportDone { imported, updated }) => {
                self.home.importing = false;
                tracing::info!(imported, updated, "import done");
                self.refresh_workspaces();
                self.refresh_cards();
                for id in std::mem::take(&mut self.reloading) {
                    self.open_session(id);
                }
            }
            (_, AppEventKind::Git { dir, state }) => {
                self.git.insert(dir.clone(), state);
                self.git_inflight.remove(&dir);
                if self.git_stale.remove(&dir) {
                    self.refresh_git(dir);
                }
            }
            (Some(id), AppEventKind::Error(msg)) if self.restoring.remove(&Saved::Session(id)) => {
                tracing::warn!(error = %msg, "skipping a window that did not reopen");
                self.drop_session(id);
            }
            (_, AppEventKind::Error(msg)) => {
                self.report(ErrorReport::new("Something went wrong", msg))
            }
            (Some(id), AppEventKind::SessionDeleted { .. }) => {
                self.drop_session(id);
                self.background.remove(&id);
                self.reloading.remove(&id);
                if self.pending_open.as_ref().is_some_and(|p| p.id == id) {
                    self.pending_open = None;
                }
                self.refresh_workspaces();
                self.refresh_cards();
            }
            (Some(id), AppEventKind::SessionForked { .. }) => {
                self.refresh_workspaces();
                self.refresh_cards();
                self.open_session(id);
            }
            (Some(_), AppEventKind::SessionCreated(s)) => {
                self.insert_vm(s, Vec::new());
                self.refresh_workspaces();
                self.refresh_cards();
            }
            (Some(_), AppEventKind::SessionStored) => self.refresh_cards(),
            (Some(_), AppEventKind::SessionOpened { session, messages }) => {
                self.restoring.remove(&Saved::Session(session.id));
                self.insert_vm(session, messages);
                self.refresh_workspaces();
            }
            (Some(id), AppEventKind::Status(status)) => {
                if status == SessionStatus::Closed {
                    if self
                        .sessions
                        .get(&id)
                        .is_some_and(|vm| vm.session.multi_session_id.is_some())
                    {
                        if let Some(vm) = self.sessions.get_mut(&id) {
                            vm.status = SessionStatus::Closed;
                        }
                        return;
                    }
                    return self.drop_session(id);
                }
                if let Some(vm) = self.sessions.get_mut(&id) {
                    if status != SessionStatus::NeedsApproval {
                        vm.pending_permission = None;
                    }
                    vm.status = status;
                    if matches!(vm.status, SessionStatus::Error(_)) {
                        vm.turn_snapshotting = false;
                        vm.turn_base = None;
                    }
                    if vm.status == SessionStatus::Streaming && !vm.turn_snapshotting {
                        vm.turn_snapshotting = true;
                        let (dir, bridge) = (vm.cwd.clone(), self.bridge.clone());
                        self.rt.spawn(async move {
                            let base = crate::turn_files::snapshot(&dir).await;
                            bridge.send(UiMsg::TurnBase { session: id, base });
                        });
                    }
                    if vm.status == SessionStatus::Idle {
                        if let Some(text) = vm.queued.pop_front() {
                            let delivery = Some(Delivery::Queued);
                            push_user_message(vm, text.clone(), delivery);
                            vm.error = None;
                            let _ = self.cmd_tx.send(ManagerCmd::Send {
                                session_id: id,
                                text,
                                delivery,
                                context: self.editors.ide_context(),
                            });
                        }
                    }
                }
            }
            (Some(id), AppEventKind::Provider(p)) => self.on_provider_event(id, p),
            (Some(id), AppEventKind::MemberMessage(msg)) => {
                if let Some(vm) = self.sessions.get_mut(&id) {
                    // Only a new prompt jumps to the end; replies follow `stick_to_bottom`.
                    vm.scroll_to_end |= msg.role == Role::User;
                    if msg.role == Role::User {
                        mark_sent(vm);
                    }
                    vm.messages.push(msg);
                    vm.error = None;
                }
            }
            (_, AppEventKind::MultiCreated { multi, members }) => {
                let pairs = members.into_iter().map(|s| (s, Vec::new())).collect();
                self.insert_multi_vm(multi, pairs, Vec::new());
                self.refresh_workspaces();
                self.refresh_cards();
            }
            (
                _,
                AppEventKind::MultiOpened {
                    multi,
                    members,
                    team_messages,
                },
            ) => {
                self.restoring.remove(&Saved::MultiSession(multi.id));
                self.insert_multi_vm(multi, members, team_messages);
                self.refresh_workspaces();
            }
            (_, AppEventKind::MultiOpenFailed { multi_id, message }) => {
                let was_restoring = self.restoring.remove(&Saved::MultiSession(multi_id));
                self.drop_multi_session(multi_id);
                if !was_restoring {
                    self.report(ErrorReport::new(
                        "Could not open the multi-session",
                        message,
                    ));
                }
            }
            (_, AppEventKind::MultiTeamMessage { multi_id, message }) => {
                if let Some(mvm) = self.multi_sessions.get_mut(&multi_id) {
                    mvm.team_messages.push(message);
                }
                self.refresh_cards();
            }
            (_, AppEventKind::MultiRenamed { multi_id, title }) => {
                if let Some(mvm) = self.multi_sessions.get_mut(&multi_id) {
                    mvm.multi.title.clone_from(&title);
                }
                for card in self
                    .home
                    .cards
                    .iter_mut()
                    .filter(|c| c.multi_id() == Some(multi_id))
                {
                    card.title.clone_from(&title);
                }
            }
            (
                _,
                AppEventKind::MultiBroadcastPaused {
                    multi_id,
                    paused,
                    hops,
                },
            ) => {
                if let Some(mvm) = self.multi_sessions.get_mut(&multi_id) {
                    mvm.broadcasts_paused = paused;
                    mvm.paused_hops = hops;
                }
            }
            (_, AppEventKind::MultiClosed { multi_id }) => self.drop_multi_session(multi_id),
            (_, AppEventKind::MultiDeleted { multi_id }) => {
                self.drop_multi_session(multi_id);
                self.refresh_cards();
                self.refresh_workspaces();
            }
            _ => {}
        }
    }

    /// Renames a session: the title shows at once, the store and the runtime follow.
    pub fn rename_session(&mut self, id: SessionId, title: String) {
        let title = title.trim().to_string();
        let Some(vm) = self.sessions.get_mut(&id) else {
            return;
        };
        if title.is_empty() || vm.session.title == title {
            return;
        }
        vm.session.title.clone_from(&title);
        // The card is patched in place: a re-read could land before the store write does.
        for card in self
            .home
            .cards
            .iter_mut()
            .filter(|c| c.session_id() == Some(id))
        {
            card.title.clone_from(&title);
        }
        let _ = self.cmd_tx.send(ManagerCmd::Rename {
            session_id: id,
            title,
        });
    }

    fn set_fork_preparing(&mut self, id: SessionId, on: bool) {
        if let Some(vm) = self.sessions.get_mut(&id) {
            vm.fork_preparing = on;
        }
    }

    /// Forks a session to `provider`: reads the hand-off prompt off the UI thread, then sends
    /// `ManagerCmd::Fork`. The session's fork button is off from now until the fork ends.
    pub fn fork_session(&mut self, id: SessionId, provider: ProviderId) {
        self.set_fork_preparing(id, true);
        let dir = self
            .config_watch
            .as_ref()
            .and_then(|w| w.path().parent().map(Path::to_path_buf));
        let path = dir
            .as_ref()
            .map(|dir| dir.join(&self.config.fork_handoff_prompt));
        let resume_path = dir
            .as_ref()
            .map(|dir| dir.join(&self.config.fork_resume_prompt));
        let bridge = self.bridge.clone();
        self.rt.spawn_blocking(move || {
            let (prompt, warning) = config::read_fork_prompt(path.as_deref());
            let (resume, resume_warning) = config::read_fork_resume_prompt(resume_path.as_deref());
            let warning = match (warning, resume_warning) {
                (Some(a), Some(b)) => Some(format!("{a}; {b}")),
                (a, b) => a.or(b),
            };
            bridge.send(UiMsg::ForkPrompt {
                session: id,
                provider,
                prompt,
                resume,
                warning,
            });
        });
    }

    /// Sessions mid-turn or waiting on an approval.
    pub fn busy_sessions(&self) -> usize {
        self.sessions
            .values()
            .filter(|vm| {
                matches!(
                    vm.status,
                    SessionStatus::Streaming | SessionStatus::NeedsApproval
                )
            })
            .count()
    }

    /// Cancels a window close while sessions are busy and asks first.
    fn guard_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.quit_confirmed {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        if self.busy_sessions() > 0 {
            self.pending_quit = true;
        } else {
            self.begin_exit(ctx);
        }
    }

    /// Quit confirmed: shows the saving overlay first, because shutting down blocks the UI.
    pub fn confirm_quit(&mut self, ctx: &egui::Context) {
        self.pending_quit = false;
        self.begin_exit(ctx);
    }

    fn begin_exit(&mut self, ctx: &egui::Context) {
        self.quit_confirmed = true;
        self.saving_frames = Some(0);
        ctx.request_repaint();
    }

    /// Paints the overlay for two frames, then lets the close through (`on_exit` blocks).
    fn saving_overlay(&mut self, ctx: &egui::Context) {
        let Some(frames) = self.saving_frames else {
            return;
        };
        crate::widgets::modal(ctx, egui::Id::unique("saving_workspaces")).show(ctx, |ui| {
            ui.label(
                egui::RichText::new("Saving workspaces…")
                    .strong()
                    .size(18.0),
            );
        });
        if frames >= 2 {
            self.saving_frames = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            self.saving_frames = Some(frames + 1);
            ctx.request_repaint();
        }
    }

    fn on_provider_event(&mut self, id: SessionId, ev: ProviderEvent) {
        let Some(vm) = self.sessions.get_mut(&id) else {
            return;
        };
        let dir = vm.cwd.clone();
        let mut refresh_git = false;
        let mut refresh_cards = false;
        let provider = vm.session.provider_id;
        let mut quota_stale = false;
        match ev {
            ProviderEvent::TextDelta(t) => {
                vm.thinking_tokens = None;
                vm.live_text.push_str(&t);
            }
            ProviderEvent::ThinkingProgress { estimated_tokens } => {
                vm.thinking_tokens = Some(estimated_tokens);
            }
            ProviderEvent::AssistantText { text } => {
                vm.live_text.clear();
                vm.thinking_tokens = None;
                let m = new_message(vm, Role::Assistant, text, None);
                vm.messages.push(m);
            }
            ProviderEvent::ToolUse {
                tool_use_id,
                name,
                input_json,
            } => {
                vm.thinking_tokens = None;
                let mut m = new_message(vm, Role::ToolCall, String::new(), Some(tool_use_id));
                m.tool_name = Some(name);
                m.tool_payload_json = Some(input_json);
                vm.messages.push(m);
            }
            ProviderEvent::ToolResult {
                tool_use_id,
                output,
                is_error,
                structured_json,
            } => {
                let mut m = new_message(vm, Role::ToolResult, output, Some(tool_use_id));
                m.tool_payload_json = structured_json;
                m.is_error = is_error;
                vm.messages.push(m);
                refresh_git = true;
                // An agent edit may have changed an open file.
                self.editors.checked = None;
            }
            ProviderEvent::Subagent { event, .. } => match *event {
                ProviderEvent::AssistantText { text } => {
                    let m = new_message(vm, Role::Assistant, text, None);
                    vm.subagent_msgs.push(m);
                }
                ProviderEvent::ToolUse {
                    tool_use_id,
                    name,
                    input_json,
                } => {
                    let mut m = new_message(vm, Role::ToolCall, String::new(), Some(tool_use_id));
                    m.tool_name = Some(name);
                    m.tool_payload_json = Some(input_json);
                    vm.subagent_msgs.push(m);
                }
                ProviderEvent::ToolResult {
                    tool_use_id,
                    output,
                    is_error,
                    structured_json,
                } => {
                    let mut m = new_message(vm, Role::ToolResult, output, Some(tool_use_id));
                    m.tool_payload_json = structured_json;
                    m.is_error = is_error;
                    vm.subagent_msgs.push(m);
                }
                _ => {}
            },
            ProviderEvent::PermissionRequest {
                request_id,
                tool_use_id,
                tool_name,
                input_json,
            } => {
                let diff = is_code_change(&tool_name)
                    .then(|| DiffData::build(&tool_name, &input_json, None, &vm.cwd))
                    .flatten();
                vm.pending_permission = Some(PendingPermission {
                    request_id,
                    tool_use_id: tool_use_id.clone(),
                    tool_name,
                    input_json,
                    reviewable: diff.is_some(),
                });
                let is_active_tab = vm.session.multi_session_id.is_none_or(|mid| {
                    self.focused == Some(WindowKind::MultiSession(mid))
                        && self
                            .multi_sessions
                            .get(&mid)
                            .is_some_and(|m| m.active_tab == MultiTab::Member(id))
                });
                if is_active_tab {
                    if let Some(data) = diff {
                        vm.diff = Some(DiffState { tool_use_id, data });
                    }
                }
            }
            ProviderEvent::TurnComplete { usage, .. } => {
                vm.turn_snapshotting = false;
                quota_stale = true;
                if let (Some(base), Some(anchor)) =
                    (vm.turn_base.take(), vm.messages.last().map(|m| m.id))
                {
                    let (dir, bridge) = (vm.cwd.clone(), self.bridge.clone());
                    self.rt.spawn(async move {
                        let files = crate::turn_files::since(&base, &dir).await;
                        bridge.send(UiMsg::TurnFiles {
                            session: id,
                            anchor,
                            changes: crate::turn_files::TurnChanges { files },
                        });
                    });
                }
                vm.subagent_msgs.clear();
                vm.view_subagents = false;
                vm.sync.transcript.own_write = std::time::SystemTime::now();
                vm.live_text.clear();
                vm.thinking_tokens = None;
                vm.settings_pending = false;
                vm.session.usage.accumulate(&usage);
                refresh_git = true;
                refresh_cards = true;
            }
            ProviderEvent::Error(e) => vm.error = Some(e),
            ProviderEvent::Started { .. }
            | ProviderEvent::Quota(_)
            | ProviderEvent::Exited { .. } => {}
        }
        if refresh_git {
            self.refresh_git(dir);
        }
        if refresh_cards {
            self.refresh_cards();
        }
        if quota_stale {
            // The turn used some: ask again at the next frame, unless an ask is out.
            self.quota_checked.remove(&provider);
        }
    }
}

/// A store read that failed in a background task: the user sees it, with the cause.
fn read_failed(bridge: &Bridge, what: &str, e: &xade_store::StoreError) {
    let report = ErrorReport::new(format!("Could not read {what}"), e.to_string());
    bridge.send(UiMsg::Report(report));
}

/// Lists `root_path` in the sidebar and refreshes the list; returns its workspace id.
async fn add_workspace(
    writer: &WriterHandle,
    reader: &StoreReader,
    bridge: &Bridge,
    root_path: PathBuf,
) -> Option<WorkspaceId> {
    let display_name = root_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root_path.display().to_string());
    let added = writer.send(WriteOp::UpsertWorkspace(Workspace {
        id: WorkspaceId::new(),
        root_path: root_path.clone(),
        display_name,
        last_opened_at: OffsetDateTime::now_utc(),
        is_open: true,
    }));
    if let Err(e) = added {
        let report = ErrorReport::new("Could not add the folder", e.to_string());
        bridge.send(UiMsg::Report(report));
        return None;
    }
    xade_runtime::flush(writer).await;
    match reader.list_workspaces().await {
        Ok(ws) => {
            let id = ws.iter().find(|w| w.root_path == root_path).map(|w| w.id);
            bridge.send(UiMsg::Workspaces(ws));
            id
        }
        Err(e) => {
            read_failed(bridge, "workspaces", &e);
            None
        }
    }
}

/// A session whose child is alive (or starting); an errored one has none.
fn live_status(status: &SessionStatus) -> bool {
    !matches!(status, SessionStatus::Error(_))
}

/// Builds a message for local display (not persisted here; the session actor owns writes).
pub(crate) fn new_message(
    vm: &SessionVm,
    role: Role,
    text: String,
    tool_use_id: Option<String>,
) -> Message {
    Message {
        id: uuid::Uuid::new_v4(),
        session_id: vm.session.id,
        seq: vm.messages.last().map_or(1, |m| m.seq + 1),
        role,
        content_text: text,
        thinking_text: None,
        tool_name: None,
        tool_use_id,
        tool_payload_json: None,
        is_error: false,
        created_at: OffsetDateTime::now_utc(),
        provider_id: None,
        sender_handle: None,
    }
}

/// A prompt went out with the current model and effort, so changing them is no longer pending.
fn mark_sent(vm: &mut SessionVm) {
    vm.sent_model.clone_from(&vm.session.model_id);
    vm.sent_effort = vm.session.effort;
}

/// Appends the user's prompt to the local transcript.
pub fn push_user_message(vm: &mut SessionVm, text: String, delivery: Option<Delivery>) {
    // The runtime titles the stored row the same way; this keeps the window in step.
    if vm.session.title.is_empty() {
        vm.session.title = xade_core::Session::derive_title(&text);
    }
    mark_sent(vm);
    let mut m = new_message(vm, Role::User, text, None);
    m.tool_name = delivery.map(|d| d.tag().to_string());
    vm.messages.push(m);
}

impl eframe::App for App {
    /// Drains the bridge and runs timers; no painting here.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.fps.frame_start(Instant::now());
        self.guarded("frame update", |app| app.update(ctx));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.guarded_ui(ui, Self::draw);
        self.fps.frame_end(Instant::now());
    }

    /// eframe calls this on the window close button and on Cmd+Q (via `exiting`),
    /// possibly twice; a repeat `ShutdownAll` returns at once.
    fn on_exit(&mut self) {
        let dirty = self.editors.map.values().filter(|vm| vm.dirty).count();
        if dirty > 0 {
            tracing::warn!(dirty, "exiting with unsaved editors");
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.cmd_tx.send(ManagerCmd::ShutdownAll(tx)).is_ok() {
            // The timer must be built inside the runtime; on this thread there is none.
            let waited = self
                .rt
                .block_on(async { tokio::time::timeout(Duration::from_secs(5), rx).await });
            if waited.is_err() {
                tracing::warn!("sessions did not shut down within 5 s");
            }
        }
    }
}

/// The window under `pointer`, else `fallback`.
fn drop_target<T: Copy>(
    pointer: Option<egui::Pos2>,
    rects: &[(T, egui::Rect)],
    fallback: Option<T>,
) -> Option<T> {
    pointer
        .and_then(|p| rects.iter().find(|(_, r)| r.contains(p)).map(|(id, _)| *id))
        .or(fallback)
}

impl App {
    /// Drains the bridge and runs timers; no painting here.
    fn update(&mut self, ctx: &egui::Context) {
        if let Some(reason) = self.writer.take_failure() {
            self.report(ErrorReport::new(
                "Saving is disabled",
                format!(
                    "{reason}. New messages and sessions are no longer saved; restart Xade to \
                     recover."
                ),
            ));
        }
        if let Some((_, at)) = &self.warning {
            ctx.request_repaint_after(WARNING_TTL.saturating_sub(at.elapsed()));
        }
        while let Ok(msg) = self.events_rx.try_recv() {
            self.on_msg(msg);
        }
        self.guard_close(ctx);
        self.handle_shortcuts(ctx);
        self.handle_drops(ctx);
        self.reload_config(ctx);
        self.check_editors(ctx, false);
        self.poll_running();
        self.poll_provider_quota();
        self.refresh_tree();
        self.run_due_import();
        ctx.request_repaint_after(running::POLL);
        if let Some(changed) = self.home.search_changed_at {
            let wait = home::SEARCH_DEBOUNCE.saturating_sub(changed.elapsed());
            if wait.is_zero() {
                self.home.search_changed_at = None;
                self.home.page = 0;
                self.refresh_cards();
            } else {
                ctx.request_repaint_after(wait);
            }
        }
        if let Some(d) = self.active_dir() {
            let focused = ctx.input(|i| i.focused);
            let changed = self.git_dir.as_ref() != Some(&d);
            if changed || (focused && self.git_checked.elapsed() >= GIT_REFRESH) {
                self.git_dir = Some(d.clone());
                self.git_checked = Instant::now();
                self.refresh_git(d);
            }
            ctx.request_repaint_after(GIT_REFRESH);
        }
    }

    /// Files dropped on the window become mentions in a session's input. macOS reports no
    /// pointer position while a file is dragged in, so the target falls back to the focused
    /// (or only) session.
    fn handle_drops(&mut self, ctx: &egui::Context) {
        let (dropped, pointer): (Vec<PathBuf>, _) = ctx.input(|i| {
            let files = i.raw.dropped_files.iter();
            (
                files.map(|f| f.path().to_path_buf()).collect(),
                i.pointer.latest_pos(),
            )
        });
        if dropped.is_empty() {
            return;
        }
        let target = if self.view == View::Workspace {
            // Build z-ordered rects across all open Session and MultiSession windows so a drop over
            // a MultiSession is never hijacked by a focused standalone session.
            let rects: Vec<(WindowKind, egui::Rect)> = self
                .open_windows
                .iter()
                .rev()
                .filter_map(|&k| match k {
                    WindowKind::Session(id) => Some((k, self.sessions.get(&id)?.window_rect?)),
                    WindowKind::MultiSession(mid) => {
                        Some((k, self.multi_sessions.get(&mid)?.window_rect?))
                    }
                    WindowKind::Editor(_) | WindowKind::Guide => None,
                })
                .collect();
            let chat_windows: Vec<WindowKind> = self
                .open_windows
                .iter()
                .copied()
                .filter(|k| matches!(k, WindowKind::Session(_) | WindowKind::MultiSession(_)))
                .collect();
            let only = (chat_windows.len() == 1).then(|| chat_windows[0]);
            let fallback = self
                .focused_session()
                .map(WindowKind::Session)
                .or_else(|| self.focused_multi_session().map(WindowKind::MultiSession))
                .or(only);
            drop_target(pointer, &rects, fallback)
        } else {
            None
        };
        match target {
            Some(WindowKind::Session(id)) => {
                if let Some(vm) = self.sessions.get_mut(&id) {
                    composer::insert_mentions(vm, &dropped);
                    return;
                }
            }
            Some(WindowKind::MultiSession(mid)) => {
                if let Some(mvm) = self.multi_sessions.get_mut(&mid) {
                    let before = mvm.input.len();
                    let rejected =
                        crate::composer::append_mentions(&mut mvm.input, &mvm.cwd, &dropped);
                    if mvm.input.len() > before {
                        mvm.focus_input = true;
                    }
                    if let Some(msg) = rejected {
                        self.warn(msg);
                    }
                    return;
                }
            }
            _ => {}
        }
        self.warn("Drop files on a session window to mention them.");
    }

    /// Draws the whole window except the error modal.
    fn draw(&mut self, ui: &mut egui::Ui) {
        crate::find::update(self, ui.ctx());
        home::top_bar(self, ui);
        status_bar::show(self, ui);
        if self.sidebar_visible {
            home::sidebar(self, ui);
        }
        let frame = egui::Frame::new()
            .fill(theme::bg_panel())
            .inner_margin(egui::Margin::same(16));
        egui::CentralPanel::default()
            .frame(frame)
            .show(ui, |ui| match self.view {
                View::Home => home::grid(self, ui),
                View::Workspace => workspace::show(self, ui),
            });
        let ctx = ui.ctx().clone();
        session_picker::show(self, &ctx);
        file_picker::show(self, &ctx);
        running::confirm_modal(self, &ctx);
        delete_modal::show(self, &ctx);
        multi_modal::show(self, &ctx);
        quit_modal::show(self, &ctx);
        crate::widgets::selection_menu::show(self, &ctx);
        self.saving_overlay(&ctx);
        about_modal::show(self, &ctx);
        crate::widgets::status_modal::show(self, &ctx);
        crate::widgets::help_modal::show(self, &ctx);
        crate::find::show(self, &ctx);
        self.editor_modal(&ctx);
        if self.keymaps_open && keymaps_modal::show(&ctx) {
            self.keymaps_open = false;
        }
    }
}

/// A provider with fixed models and a preflight that always passes, for tests.
#[cfg(test)]
pub(crate) struct StubProvider {
    id: ProviderId,
    name: &'static str,
    models: Vec<xade_core::ModelOption>,
    /// How long `preflight` takes and what it returns.
    preflight: (Duration, Result<(), ProviderError>),
}

#[cfg(test)]
impl StubProvider {
    /// A preflight that takes `delay` and returns `result`.
    pub(crate) fn preflighting(
        mut self,
        delay: Duration,
        result: Result<(), ProviderError>,
    ) -> Self {
        self.preflight = (delay, result);
        self
    }

    pub(crate) fn claude() -> Self {
        Self::new(ProviderId::Claude, "Claude", &["opus"])
    }

    pub(crate) fn antigravity() -> Self {
        Self::new(ProviderId::Antigravity, "Antigravity", &["gemini-4-neon"])
    }

    pub(crate) fn codex() -> Self {
        Self::new(ProviderId::Codex, "Codex", &["gpt-6-luna"])
    }

    fn new(id: ProviderId, name: &'static str, models: &[&str]) -> Self {
        let models = models
            .iter()
            .map(|m| xade_core::ModelOption {
                id: (*m).into(),
                label: (*m).into(),
                description: String::new(),
                effort_levels: vec![xade_core::Effort::Low, xade_core::Effort::High],
            })
            .collect();
        Self {
            id,
            name,
            models,
            preflight: (Duration::ZERO, Ok(())),
        }
    }
}

#[cfg(test)]
impl AgentProvider for StubProvider {
    fn id(&self) -> ProviderId {
        self.id
    }
    fn display_name(&self) -> &'static str {
        self.name
    }
    fn models(&self) -> Vec<xade_core::ModelOption> {
        self.models.clone()
    }
    fn default_model(&self) -> String {
        self.models[0].id.clone()
    }
    fn effort_levels(&self) -> Vec<xade_core::Effort> {
        vec![xade_core::Effort::Low, xade_core::Effort::High]
    }
    fn preflight(&self) -> Result<(), ProviderError> {
        std::thread::sleep(self.preflight.0);
        self.preflight.1.clone()
    }
    fn spawn(
        &self,
        _cfg: xade_core::SpawnConfig,
    ) -> Result<xade_core::SessionHandle, ProviderError> {
        Err(ProviderError::NotInstalled)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        }
    }

    /// Settings opens in Xade's editor (once), and saving there reaches the mtime watch.
    #[test]
    fn settings_open_in_the_editor_and_saving_triggers_reload() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-app-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        let file = config::path(&dir);
        config::load(&file);
        let mut watch = config::Watch::new(file.clone());
        app.config_watch = Some(config::Watch::new(file.clone()));
        let text = std::fs::read_to_string(&file).unwrap();
        // What `open_settings` sends once the file is read.
        app.on_msg(UiMsg::FileOpened {
            path: file.clone(),
            content: Ok(editor::FileContent::Text(text)),
        });
        let id = app.editors.find_path(&file).expect("editor opened");
        assert_eq!(app.focused, Some(WindowKind::Editor(id)));
        app.view = View::Home;
        app.open_settings();
        assert_eq!(app.editors.map.len(), 1);
        assert!(app.view == View::Workspace);
        assert_eq!(app.focused, Some(WindowKind::Editor(id)));
        // The editor saves with `write_atomic`; the watch sees the new mtime.
        std::thread::sleep(Duration::from_millis(20));
        editor::write_atomic(&file, "{\"font_size\": 20}\n").unwrap();
        assert!(watch.changed(Instant::now() + config::POLL));
        assert_eq!(config::load(&file).0.font_size, 20.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chord_and_plain_shortcuts_drive_the_app() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-app-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        let frame = |app: &mut App, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                events,
                ..Default::default()
            };
            ctx.run_pass(input, |ctx| app.handle_shortcuts(ctx))
                .drop_without_applying_deltas();
        };
        frame(&mut app, vec![key(egui::Key::B)]);
        assert!(!app.sidebar_visible);
        frame(&mut app, vec![key(egui::Key::K)]);
        assert!(app.chord.armed(Instant::now()) && !app.keymaps_open);
        frame(&mut app, vec![key(egui::Key::S)]);
        assert!(app.keymaps_open);
        frame(&mut app, vec![key(egui::Key::P)]);
        assert!(app.file_picker.is_some() && app.picker.is_none());
        app.file_picker = None;
        // Cmd+P must be released first, or egui reads the next P press as a key repeat.
        let p = |pressed, modifiers| egui::Event::Key {
            key: egui::Key::P,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers,
        };
        frame(&mut app, vec![p(false, egui::Modifiers::COMMAND)]);
        let cmd_shift = egui::Modifiers::COMMAND | egui::Modifiers::SHIFT;
        frame(&mut app, vec![p(true, cmd_shift)]);
        assert!(app.picker.is_some() && app.file_picker.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_forces_the_next_running_poll_and_a_new_list_read() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-app-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        app.running_checked = Some(Instant::now());
        let before = app.home.generation;
        app.refresh_sessions();
        assert!(app.running_checked.is_none());
        assert!(app.home.generation > before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_windows_are_saved_and_restored_in_order() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-app-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        let file = crate::windows_state::path(&dir.join("settings.json"));
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        app.windows_file = Some(file.clone());
        let (a, b) = (SessionId::new(), SessionId::new());
        app.add_window(WindowKind::Session(a));
        app.add_window(WindowKind::Session(b));
        app.drop_window(WindowKind::Session(a));
        app.add_window(WindowKind::Session(a));
        let want = vec![Saved::Session(b), Saved::Session(a)];
        // The write runs on the blocking pool.
        let deadline = Instant::now() + Duration::from_secs(5);
        while crate::windows_state::load(&file) != want && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(crate::windows_state::load(&file), want);
        let mut again = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace2.db"));
        again.restore_windows(file);
        assert_eq!(
            again.open_windows,
            vec![WindowKind::Session(b), WindowKind::Session(a)]
        );
        assert!(again.view == View::Workspace);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_windows_off_reopens_nothing_and_clears_the_list() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-app-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        let file = crate::windows_state::path(&dir.join("settings.json"));
        crate::windows_state::save(&file, &[Saved::Session(SessionId::new())]).unwrap();
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        app.config.restore_windows = false;
        app.restore_windows(file.clone());
        assert!(app.open_windows.is_empty() && app.restoring.is_empty());
        app.add_window(WindowKind::Session(SessionId::new()));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !crate::windows_state::load(&file).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(crate::windows_state::load(&file).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_window_that_fails_to_reopen_is_skipped_quietly() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-app-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        let file = crate::windows_state::path(&dir.join("settings.json"));
        let (gone, good) = (SessionId::new(), SessionId::new());
        let (missing, present) = (dir.join("missing.rs"), dir.join("present.rs"));
        crate::windows_state::save(
            &file,
            &[
                Saved::Session(gone),
                Saved::File(missing.clone()),
                Saved::Session(good),
                Saved::File(present.clone()),
            ],
        )
        .unwrap();
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        app.restore_windows(file);
        let event = |id, kind| {
            UiMsg::App(Box::new(AppEvent {
                session_id: Some(id),
                kind,
            }))
        };
        app.on_msg(event(gone, AppEventKind::Error("not found".into())));
        app.on_msg(UiMsg::FileOpened {
            path: missing,
            content: Err("No such file".into()),
        });
        app.on_msg(UiMsg::FileOpened {
            path: present.clone(),
            content: Ok(editor::FileContent::Text("fn main() {}".into())),
        });
        // Only the failed ones went, with no error shown; the rest kept their place.
        assert!(app.errors.current().is_none());
        let editor = app.editors.find_path(&present).expect("editor opened");
        assert_eq!(
            app.open_windows,
            vec![WindowKind::Session(good), WindowKind::Editor(editor)]
        );
        // An error for a session that was not being restored is reported as usual.
        app.on_msg(event(SessionId::new(), AppEventKind::Error("other".into())));
        assert!(app.errors.current().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_or_closing_a_window_retiles_and_clears_maximize() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("xade-app-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        let (a, b, c) = (
            WindowKind::Session(SessionId::new()),
            WindowKind::Session(SessionId::new()),
            WindowKind::Session(SessionId::new()),
        );
        app.add_window(a);
        app.maximized = Some(a);
        app.tiling.auto = false;
        app.add_window(b);
        assert!(app.maximized.is_none() && app.tiling.auto);
        app.tiling.auto = false;
        app.add_window(b);
        assert!(!app.tiling.auto, "an already open window changes nothing");
        app.add_window(c);
        assert!(app.tiling.auto);
        app.tiling.auto = false;
        app.drop_window(b);
        assert!(app.tiling.auto && app.open_windows == vec![a, c]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[derive(Debug)]
    struct Dropped(PathBuf);

    impl egui::DroppedFile for Dropped {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
        fn bytes(&self) -> Result<Vec<u8>, String> {
            Ok(Vec::new())
        }
    }

    fn new_session_row() -> Session {
        let now = OffsetDateTime::now_utc();
        Session {
            origin: xade_core::SessionOrigin::default(),
            id: SessionId::new(),
            workspace_id: WorkspaceId::new(),
            provider_id: xade_core::ProviderId::Claude,
            provider_session_id: None,
            title: "t".into(),
            preview_snippet: String::new(),
            model_id: "opus".into(),
            effort: xade_core::Effort::Default,
            sent_model_id: String::new(),
            sent_effort: xade_core::Effort::Default,
            auto_mode: false,
            usage: xade_core::UsageTotals::default(),
            created_at: now,
            updated_at: now,
            multi_session_id: None,
            agent_handle: None,
        }
    }

    #[test]
    fn a_snapshot_turn_is_xades_own_until_it_is_deleted() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = test_app(&rt);
        let throwaway = |active| AppEvent {
            session_id: None,
            kind: AppEventKind::Throwaway {
                provider_session_id: "psid-1".into(),
                active,
            },
        };
        assert!(!app.live_provider_ids().contains("psid-1"));
        app.on_app_event(throwaway(true));
        assert!(app.live_provider_ids().contains("psid-1"));
        // A poll that lists it (a child of Xade) is no change in what runs elsewhere.
        let entry = RunningSession {
            provider_session_id: "psid-1".into(),
            state: xade_core::RunState::Busy,
            location: "SDK".into(),
            pid: 1,
        };
        app.on_running(Vec::new(), Vec::new());
        app.on_running(vec![entry], Vec::new());
        assert!(app.running.is_empty() && !app.running_import_due);
        app.on_app_event(throwaway(false));
        assert!(!app.live_provider_ids().contains("psid-1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn renaming_updates_the_window_and_card_and_sends_one_command() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = test_app(&rt);
        let (tx, mut cmds) = tokio::sync::mpsc::unbounded_channel();
        app.cmd_tx = tx;
        let mut session = new_session_row();
        session.title = "old".into();
        let id = session.id;
        app.insert_vm(session, Vec::new());

        app.rename_session(id, "  new name ".into());
        assert_eq!(app.sessions[&id].session.title, "new name");
        match cmds.try_recv() {
            Ok(ManagerCmd::Rename { session_id, title }) => {
                assert_eq!((session_id, title.as_str()), (id, "new name"));
            }
            _ => panic!("expected Rename"),
        }
        // Unchanged and empty names send nothing.
        app.rename_session(id, "new name".into());
        app.rename_session(id, "   ".into());
        assert!(cmds.try_recv().is_err());
        assert_eq!(app.sessions[&id].session.title, "new name");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_fork_reads_the_prompt_file_off_thread_and_stays_off_until_it_ends() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = test_app(&rt);
        let (tx, mut cmds) = tokio::sync::mpsc::unbounded_channel();
        app.cmd_tx = tx;
        app.config_watch = Some(config::Watch::new(dir.join("settings.json")));
        std::fs::create_dir_all(dir.join("prompts")).unwrap();
        std::fs::write(dir.join("prompts/mine.md"), "my prompt").unwrap();
        app.config.fork_handoff_prompt = PathBuf::from("prompts/mine.md");
        std::fs::write(dir.join("prompts/resume.md"), "my resume").unwrap();
        app.config.fork_resume_prompt = PathBuf::from("prompts/resume.md");
        let session = new_session_row();
        let id = session.id;
        app.insert_vm(session, Vec::new());

        app.fork_session(id, ProviderId::Claude);
        assert!(
            app.sessions[&id].fork_preparing,
            "off at once, before the read"
        );
        assert!(
            cmds.try_recv().is_err(),
            "nothing is sent before the file is read"
        );
        pump(&mut app);
        match cmds.try_recv() {
            Ok(ManagerCmd::Fork {
                session_id,
                provider,
                handoff_prompt,
                resume_prompt,
            }) => assert_eq!(
                (session_id, provider, handoff_prompt.as_str(), resume_prompt),
                (id, ProviderId::Claude, "my prompt", "my resume".to_string())
            ),
            _ => panic!("no Fork sent"),
        }
        assert!(app.warning_text().is_none());

        // The manager's events keep it off, then clear it, for the source session.
        let event = |kind| AppEvent {
            session_id: Some(id),
            kind,
        };
        app.on_app_event(event(AppEventKind::ForkPreparing { from: id }));
        assert!(app.sessions[&id].fork_preparing);
        // An unrelated error for the source does not end the fork.
        app.on_app_event(event(AppEventKind::Error("nope".into())));
        assert!(app.sessions[&id].fork_preparing);
        // A failed fork ends it, and shows its message like any error.
        assert_eq!(app.errors.pending(), 1, "the unrelated error");
        app.on_app_event(event(AppEventKind::ForkFailed {
            from: id,
            message: "Could not fork the session: x".into(),
        }));
        assert!(!app.sessions[&id].fork_preparing);
        assert_eq!(app.errors.pending(), 2);
        app.errors.dismiss();
        let (report, _) = app.errors.current().unwrap();
        assert!(report.message.contains("Could not fork"));
        app.on_app_event(event(AppEventKind::ForkPreparing { from: id }));
        app.on_app_event(AppEvent {
            session_id: Some(SessionId::new()),
            kind: AppEventKind::SessionForked {
                from: id,
                session_id: SessionId::new(),
            },
        });
        assert!(!app.sessions[&id].fork_preparing);

        // A missing file sends the bundled prompt and warns.
        while cmds.try_recv().is_ok() {}
        app.config.fork_handoff_prompt = PathBuf::from("prompts/gone.md");
        app.config.fork_resume_prompt = PathBuf::from("prompts/gone-resume.md");
        app.fork_session(id, ProviderId::Claude);
        pump_until_fork(&mut app, &mut cmds);
        match cmds.try_recv() {
            Ok(ManagerCmd::Fork {
                handoff_prompt,
                resume_prompt,
                ..
            }) => {
                assert_eq!(handoff_prompt, config::BUNDLED_FORK_PROMPT);
                assert_eq!(resume_prompt, config::BUNDLED_FORK_RESUME_PROMPT);
            }
            _ => panic!("no Fork sent"),
        }
        assert!(app
            .warning_text()
            .is_some_and(|w| w.contains("gone.md") && w.contains("gone-resume.md")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dropped_files_become_mentions_without_a_pointer_position() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = test_app(&rt);
        let session = new_session_row();
        let id = session.id;
        app.insert_vm(session, Vec::new());
        app.sessions.get_mut(&id).unwrap().cwd = dir.clone();
        app.add_window(WindowKind::Session(id));
        app.focus(WindowKind::Session(id));
        app.view = View::Workspace;
        std::fs::write(dir.join("a.rs"), "x").unwrap();
        std::fs::write(dir.join("b.rs"), "x").unwrap();

        // A drag from Finder on macOS carries no pointer position.
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            dropped_files: vec![std::sync::Arc::new(Dropped(dir.join("a.rs")))],
            ..Default::default()
        };
        ctx.run_pass(raw, |ctx| app.handle_drops(ctx))
            .drop_without_applying_deltas();
        assert_eq!(app.sessions[&id].input, "@a.rs ");
        assert!(app.warning_text().is_none());

        // On Home there is no session window to receive it.
        app.view = View::Home;
        let raw = egui::RawInput {
            dropped_files: vec![std::sync::Arc::new(Dropped(dir.join("b.rs")))],
            ..Default::default()
        };
        ctx.run_pass(raw, |ctx| app.handle_drops(ctx))
            .drop_without_applying_deltas();
        assert_eq!(app.sessions[&id].input, "@a.rs ");
        assert!(app.warning_text().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drop_target_prefers_the_window_under_the_pointer() {
        let (a, b) = (SessionId::new(), SessionId::new());
        let rect = |x: f32| egui::Rect::from_min_size(egui::pos2(x, 0.0), egui::vec2(100.0, 100.0));
        let rects = [(a, rect(0.0)), (b, rect(100.0))];
        assert_eq!(
            drop_target(Some(egui::pos2(150.0, 5.0)), &rects, Some(a)),
            Some(b)
        );
        assert_eq!(
            drop_target(Some(egui::pos2(500.0, 5.0)), &rects, Some(a)),
            Some(a)
        );
        assert_eq!(drop_target(None, &rects, Some(b)), Some(b));
        assert_eq!(drop_target(None, &rects, None), None);
    }

    fn test_app(rt: &tokio::runtime::Runtime) -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("xade-app-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        let app = App::for_test(&ctx, rt.handle().clone(), &dir.join("workspace.db"));
        (app, dir)
    }

    #[test]
    fn closing_the_window_waits_for_shutdown_without_panicking() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = test_app(&rt);
        // `for_test` drops the manager's end, which would skip the wait entirely.
        let (tx, mut cmds) = tokio::sync::mpsc::unbounded_channel();
        app.cmd_tx = tx;
        rt.spawn(async move {
            if let Some(ManagerCmd::ShutdownAll(done)) = cmds.recv().await {
                let _ = done.send(());
            }
        });
        let started = Instant::now();
        eframe::App::on_exit(&mut app);
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "did not wait for the ack"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Folds in bridge messages until one sends a `Fork` (other tasks report on the same channel).
    fn pump_until_fork(app: &mut App, cmds: &mut tokio::sync::mpsc::UnboundedReceiver<ManagerCmd>) {
        for _ in 0..20 {
            if !cmds.is_empty() {
                return;
            }
            pump(app);
        }
    }

    /// Waits for the next bridge message and folds it in.
    fn pump(app: &mut App) {
        let msg = app
            .events_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("bridge message");
        app.on_msg(msg);
    }

    #[test]
    fn save_writes_atomically_and_clears_dirty() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = test_app(&rt);
        let path = dir.join("note.md");
        let id = app.editors.create(Some(path.clone()), String::new());
        {
            let vm = app.editors.map.get_mut(&id).unwrap();
            vm.text = "hello".into();
            vm.dirty = true;
            vm.revision = 1;
        }
        app.save_editor(id, false, false);
        pump(&mut app);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(!app.editors.map[&id].dirty && !app.editors.map[&id].busy);
        assert!(app.errors.current().is_none());
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
        // An edit made while a save is in flight keeps the editor dirty.
        {
            let vm = app.editors.map.get_mut(&id).unwrap();
            vm.text = "hello!".into();
            vm.dirty = true;
            vm.revision = 2;
        }
        app.save_editor(id, false, false);
        app.editors.map.get_mut(&id).unwrap().revision = 3;
        pump(&mut app);
        assert!(app.editors.map[&id].dirty);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dirty_close_asks_and_each_choice_acts() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = test_app(&rt);
        let path = dir.join("a.md");
        let make = |app: &mut App, dirty: bool| {
            let id = app.editors.create(Some(path.clone()), "x".into());
            app.editors.map.get_mut(&id).unwrap().dirty = dirty;
            app.focus(WindowKind::Editor(id));
            id
        };
        // Clean: closes at once.
        let id = make(&mut app, false);
        app.request_close_editor(id);
        assert!(app.editors.map.is_empty() && app.open_windows.is_empty());
        // Dirty: asks; Cancel keeps it.
        let id = make(&mut app, true);
        app.request_close_editor(id);
        assert_eq!(app.editors.pending_close, Some(id));
        app.resolve_close(id, CloseChoice::Cancel);
        assert!(app.editors.pending_close.is_none() && app.editors.map.contains_key(&id));
        // Don't Save: closes without writing.
        app.request_close_editor(id);
        app.resolve_close(id, CloseChoice::DontSave);
        assert!(app.editors.map.is_empty() && !path.exists());
        // Save: writes, then closes.
        let id = make(&mut app, true);
        app.request_close_editor(id);
        app.resolve_close(id, CloseChoice::Save);
        pump(&mut app);
        assert!(app.editors.map.is_empty() && app.open_windows.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dirty_close_modal_draws_for_the_pending_editor() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (mut app, dir) = test_app(&rt);
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let id = app.editors.create(None, "x".into());
        app.editors.map.get_mut(&id).unwrap().dirty = true;
        app.focus(WindowKind::Editor(id));
        app.request_close_editor(id);
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 800.0),
                )),
                ..Default::default()
            };
            ctx.run_pass(input, |ctx| app.editor_modal(ctx))
                .drop_without_applying_deltas();
        }
        assert_eq!(app.editors.pending_close, Some(id));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_editor_ignores_image_windows() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = test_app(&rt);
        let path = dir.join("shot.png");
        let original = b"original-bytes";
        std::fs::write(&path, original).unwrap();
        let pixels = egui::ColorImage::filled([8, 8], egui::Color32::RED);
        let id = app.editors.create_image(path.clone(), pixels);
        app.focus(WindowKind::Editor(id));
        app.save_editor(id, false, false);
        assert!(!app.editors.map[&id].busy);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_panic_in_a_step_is_reported_and_does_not_propagate() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (mut app, dir) = test_app(&rt);
        assert!(app.guarded("test step", |_| {}));
        assert!(app.errors.current().is_none());
        let finished = app.guarded("test step", |_| panic!("boom in a handler"));
        assert!(!finished);
        let (report, count) = app.errors.current().unwrap();
        assert_eq!(count, 1);
        assert!(
            report.message.contains("boom in a handler") && report.message.contains("test step")
        );
        // The same panic again collapses into one entry.
        app.guarded("test step", |_| panic!("boom in a handler"));
        assert_eq!(app.errors.current().unwrap().1, 2);
        assert_eq!(app.errors.pending(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A panic while drawing still ends the frame: the empty panel and the error modal are
    /// drawn, and the next frame draws normally.
    #[test]
    fn a_panic_while_drawing_finishes_the_frame_with_the_modal() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (mut app, dir) = test_app(&rt);
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0));
        let frame = |app: &mut App, panics: bool| {
            let input = egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                app.guarded_ui(ui, |app, ui| {
                    if panics {
                        panic!("draw failed");
                    }
                    app.draw(ui);
                });
            });
            out.textures_delta.clear();
            out.shapes
        };
        let has_text = |shapes: &[egui::epaint::ClippedShape], needle: &str| {
            fn walk(s: &egui::Shape, needle: &str) -> bool {
                match s {
                    egui::Shape::Text(t) => t.galley.text().contains(needle),
                    egui::Shape::Vec(l) => l.iter().any(|s| walk(s, needle)),
                    _ => false,
                }
            }
            shapes.iter().any(|c| walk(&c.shape, needle))
        };
        frame(&mut app, true);
        let shapes = frame(&mut app, false);
        assert!(
            has_text(&shapes, "Xade hit an internal error"),
            "modal shown"
        );
        assert!(has_text(&shapes, "Dismiss"));
        assert_eq!(app.errors.current().map(|(_, n)| n), Some(1));
        app.errors.dismiss();
        let shapes = frame(&mut app, false);
        assert!(!has_text(&shapes, "Dismiss"), "modal gone once dismissed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dead_writer_shows_saving_is_disabled_once() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (mut app, dir) = test_app(&rt);
        let ctx = egui::Context::default();
        app.update(&ctx);
        assert!(app.errors.current().is_none());
        app.writer.send(WriteOp::Poison).unwrap();
        for _ in 0..200 {
            app.update(&ctx);
            if app.errors.current().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let (report, _) = app.errors.current().expect("writer death reported");
        assert_eq!(report.title, "Saving is disabled");
        app.update(&ctx);
        assert_eq!(app.errors.current().unwrap().1, 1, "reported only once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn warnings_expire_and_repeats_are_not_stacked() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (mut app, dir) = test_app(&rt);
        assert!(app.warning_text().is_none());
        app.warn("settings.json: bad");
        assert_eq!(app.warning_text(), Some("settings.json: bad"));
        assert!(app.errors.current().is_none(), "warnings get no modal");
        app.warning = app
            .warning
            .take()
            .map(|(m, at)| (m, at - WARNING_TTL - Duration::from_secs(1)));
        assert!(app.warning_text().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn target_folder_prefers_workspace_then_session_then_home() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (mut app, dir) = test_app(&rt);
        assert_eq!(app.target_dir(), dirs::home_dir().unwrap());
        let ws = Workspace {
            id: WorkspaceId::new(),
            root_path: dir.clone(),
            display_name: "w".into(),
            last_opened_at: OffsetDateTime::now_utc(),
            is_open: true,
        };
        app.home.scope = Some(ws.id);
        app.home.workspaces = vec![ws];
        assert_eq!(app.target_dir(), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn two_provider_app(rt: &tokio::runtime::Runtime) -> (App, std::path::PathBuf) {
        let (mut app, dir) = test_app(rt);
        app.providers = vec![
            Arc::new(StubProvider::claude()),
            Arc::new(StubProvider::antigravity()),
        ];
        (app, dir)
    }

    fn fail(app: &mut App, id: ProviderId, e: ProviderError) {
        app.home.preflight.insert(id, Err(e));
    }

    #[test]
    fn a_not_installed_provider_is_hidden_while_another_is_shown() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = two_provider_app(&rt);
        let (claude, agy) = (ProviderId::Claude, ProviderId::Antigravity);
        assert_eq!(app.visible_providers(), vec![claude, agy]);
        assert_eq!(app.selectable_providers(), vec![claude, agy]);

        // Antigravity missing: Claude-only users see today's UI.
        fail(&mut app, agy, ProviderError::NotInstalled);
        assert_eq!(app.visible_providers(), vec![claude]);
        assert_eq!(app.selectable_providers(), vec![claude]);
        assert!(app.available(claude) && !app.available(agy));

        // Installed but signed out: shown, with its error, but not selectable.
        fail(&mut app, agy, ProviderError::NotLoggedIn);
        assert_eq!(app.visible_providers(), vec![claude, agy]);
        assert_eq!(app.selectable_providers(), vec![claude]);
        let text = app.unavailable_text(agy).unwrap();
        assert!(text.starts_with("Antigravity is not available: "), "{text}");

        // Claude missing, Antigravity fine: only Antigravity shows.
        app.home.preflight.remove(&agy);
        fail(&mut app, claude, ProviderError::NotInstalled);
        assert_eq!(app.visible_providers(), vec![agy]);
        assert_eq!(app.new_provider(), Some(agy));

        // Neither installed: the first provider stays, for today's banner.
        fail(&mut app, agy, ProviderError::NotInstalled);
        assert_eq!(app.visible_providers(), vec![claude]);
        assert!(!app.any_available() && app.new_provider().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_new_session_provider_is_the_pick_then_the_last_then_the_first_available() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = two_provider_app(&rt);
        let (claude, agy) = (ProviderId::Claude, ProviderId::Antigravity);
        assert_eq!(app.new_provider(), Some(claude));
        app.on_msg(UiMsg::LastProvider(Some(agy)));
        assert_eq!(app.new_provider(), Some(agy));
        app.home.provider_pick = Some(claude);
        assert_eq!(app.new_provider(), Some(claude));
        // An unavailable choice is skipped.
        fail(&mut app, claude, ProviderError::NotLoggedIn);
        assert_eq!(app.new_provider(), Some(agy));
        app.home.provider_pick = Some(agy);
        fail(&mut app, agy, ProviderError::NotLoggedIn);
        assert_eq!(app.new_provider(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs `new_session` and returns the `Create` it sent.
    fn created(
        rt: &tokio::runtime::Runtime,
        app: &mut App,
    ) -> (ProviderId, String, xade_core::Effort, bool) {
        let (tx, mut cmds) = tokio::sync::mpsc::unbounded_channel();
        app.cmd_tx = tx;
        app.new_session();
        let cmd = rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), cmds.recv()).await })
            .unwrap()
            .unwrap();
        let ManagerCmd::Create {
            provider,
            model_id,
            effort,
            auto_mode,
            ..
        } = cmd
        else {
            panic!("not a Create");
        };
        (provider, model_id, effort, auto_mode)
    }

    fn stored(
        app: &App,
        ws: WorkspaceId,
        provider: ProviderId,
        model: &str,
        effort: xade_core::Effort,
    ) {
        let mut row = new_session_row();
        row.workspace_id = ws;
        row.provider_id = provider;
        row.model_id = model.into();
        row.effort = effort;
        row.origin = xade_core::SessionOrigin::Xade;
        app.writer.send(WriteOp::InsertSession(row)).unwrap();
        app.writer.flush().unwrap();
    }

    #[test]
    fn new_session_uses_the_picked_provider_and_only_its_own_last_choice() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = two_provider_app(&rt);
        let (claude, agy) = (ProviderId::Claude, ProviderId::Antigravity);
        let ws = WorkspaceId::new();
        app.home.workspaces = vec![Workspace {
            id: ws,
            root_path: dir.clone(),
            display_name: "w".into(),
            last_opened_at: OffsetDateTime::now_utc(),
            is_open: true,
        }];
        app.home.scope = Some(ws);
        app.writer
            .send(WriteOp::UpsertWorkspace(app.home.workspaces[0].clone()))
            .unwrap();
        // Only Claude has a stored session, with a Claude-only model and effort.
        stored(&app, ws, claude, "opus", xade_core::Effort::High);

        app.home.provider_pick = Some(agy);
        let (p, model, effort, _) = created(&rt, &mut app);
        assert_eq!(p, agy);
        assert_eq!(
            model, "gemini-4-neon",
            "the provider default, not a Claude id"
        );
        assert_eq!(effort, xade_core::Effort::Default);

        app.home.provider_pick = Some(claude);
        let (p, model, effort, _) = created(&rt, &mut app);
        assert_eq!(
            (p, model.as_str(), effort),
            (claude, "opus", xade_core::Effort::High)
        );

        // An effort the model does not offer falls back to Default.
        stored(&app, ws, agy, "gemini-4-neon", xade_core::Effort::Max);
        app.home.provider_pick = Some(agy);
        let (p, model, effort, _) = created(&rt, &mut app);
        assert_eq!((p, model.as_str()), (agy, "gemini-4-neon"));
        assert_eq!(effort, xade_core::Effort::Default);

        // With the pick unavailable, nothing is created for it.
        fail(&mut app, agy, ProviderError::NotLoggedIn);
        fail(&mut app, claude, ProviderError::NotLoggedIn);
        let (tx, mut cmds) = tokio::sync::mpsc::unbounded_channel();
        app.cmd_tx = tx;
        app.new_session();
        assert!(cmds.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_is_blocked_only_for_the_provider_that_is_unavailable() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = two_provider_app(&rt);
        let (tx, mut cmds) = tokio::sync::mpsc::unbounded_channel();
        app.cmd_tx = tx;
        let mut claude_row = new_session_row();
        claude_row.provider_id = ProviderId::Claude;
        let mut agy_row = new_session_row();
        agy_row.provider_id = ProviderId::Antigravity;
        let (claude_id, agy_id) = (claude_row.id, agy_row.id);
        app.insert_vm(claude_row, Vec::new());
        app.insert_vm(agy_row, Vec::new());
        // An errored session is not live, so `open_session` sends `Open` for it.
        for vm in app.sessions.values_mut() {
            vm.status = SessionStatus::Error("x".into());
        }
        fail(
            &mut app,
            ProviderId::Antigravity,
            ProviderError::NotLoggedIn,
        );

        app.open_session(agy_id);
        assert!(cmds.try_recv().is_err(), "Antigravity is blocked");
        let text = app.warning_text().unwrap();
        assert!(text.starts_with("Antigravity is not available"), "{text}");
        app.open_session_background(agy_id);
        assert!(cmds.try_recv().is_err());

        app.open_session(claude_id);
        assert!(
            matches!(cmds.try_recv(), Ok(ManagerCmd::Open { session_id }) if session_id == claude_id)
        );
        assert!(app.open_windows.contains(&WindowKind::Session(claude_id)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retry_reruns_one_provider_and_a_recovery_starts_the_import() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = two_provider_app(&rt);
        let (tx, mut cmds) = tokio::sync::mpsc::unbounded_channel();
        app.cmd_tx = tx;
        let agy = ProviderId::Antigravity;
        fail(&mut app, agy, ProviderError::NotLoggedIn);
        app.retry_preflight(agy);
        let msg = app.events_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let UiMsg::Preflight(id, result) = msg else {
            panic!("not a Preflight");
        };
        assert_eq!(id, agy, "only the retried provider reports");
        assert!(app.events_rx.try_recv().is_err());
        app.on_msg(UiMsg::Preflight(id, result));
        assert!(app.available(agy));
        assert!(matches!(cmds.try_recv(), Ok(ManagerCmd::Import)));
        // A provider that was already fine does not re-import.
        app.on_msg(UiMsg::Preflight(agy, Ok(())));
        assert!(cmds.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_provider_still_checking_is_neither_offered_nor_an_error_until_it_reports() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = two_provider_app(&rt);
        let (tx, mut cmds) = tokio::sync::mpsc::unbounded_channel();
        app.cmd_tx = tx;
        let (claude, agy) = (ProviderId::Claude, ProviderId::Antigravity);
        app.home.pending.insert(agy);
        assert!(app.available(claude) && !app.available(agy));
        assert_eq!(app.selectable_providers(), vec![claude]);
        assert_eq!(app.visible_providers(), vec![claude]);
        assert_eq!(app.preflight_error(agy), None);
        assert_eq!(app.unavailable_text(agy), None);

        // Its report ends the wait and offers it.
        app.on_msg(UiMsg::Preflight(agy, Ok(())));
        assert!(app.home.pending.is_empty());
        assert_eq!(app.selectable_providers(), vec![claude, agy]);
        assert_eq!(app.visible_providers(), vec![claude, agy]);
        assert!(matches!(cmds.try_recv(), Ok(ManagerCmd::Import)));

        // A failure shows as today's error.
        app.home.pending.insert(agy);
        app.on_msg(UiMsg::Preflight(agy, Err(ProviderError::NotLoggedIn)));
        assert_eq!(app.selectable_providers(), vec![claude]);
        assert!(app.unavailable_text(agy).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_window_restore_waits_for_a_pending_provider_when_none_is_usable_yet() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = two_provider_app(&rt);
        let (tx, mut cmds) = tokio::sync::mpsc::unbounded_channel();
        app.cmd_tx = tx;
        let (claude, agy) = (ProviderId::Claude, ProviderId::Antigravity);
        fail(&mut app, claude, ProviderError::NotInstalled);
        app.home.pending.insert(agy);
        let file = dir.join("windows.json");
        app.restore_windows(file.clone());
        assert_eq!(app.deferred_restore, Some(file.clone()));
        assert!(app.windows_file.is_none());
        app.on_msg(UiMsg::Preflight(agy, Ok(())));
        assert!(app.deferred_restore.is_none());
        assert_eq!(app.windows_file, Some(file));
        assert!(matches!(cmds.try_recv(), Ok(ManagerCmd::Import)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_guide_opens_once_when_no_provider_passes_and_goes_when_a_chat_opens() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = two_provider_app(&rt);
        let (claude, agy) = (ProviderId::Claude, ProviderId::Antigravity);
        let missing = || Err(ProviderError::NotInstalled);
        fail(&mut app, claude, ProviderError::NotInstalled);
        app.home.pending.insert(agy);
        app.offer_guide();
        assert!(app.guide.is_none(), "waits for every check");
        app.on_msg(UiMsg::Preflight(agy, missing()));
        assert_eq!(app.open_windows, vec![WindowKind::Guide]);
        assert!(app.view == View::Workspace);

        app.close_guide();
        assert!(app.view == View::Home && app.open_windows.is_empty());
        app.recheck_providers();
        assert_eq!(app.home.pending.len(), 2, "both show as checking");
        app.on_msg(UiMsg::Preflight(claude, missing()));
        app.on_msg(UiMsg::Preflight(agy, missing()));
        assert!(app.guide.is_none(), "a closed guide stays closed");

        app.open_guide();
        let id = SessionId::new();
        app.focus(WindowKind::Session(id));
        assert!(app.guide.is_none());
        assert_eq!(app.open_windows, vec![WindowKind::Session(id)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn three_providers_filter_and_select_codex_independently() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (mut app, dir) = two_provider_app(&rt);
        app.providers.push(Arc::new(StubProvider::codex()));
        let (claude, agy, codex) = (
            ProviderId::Claude,
            ProviderId::Antigravity,
            ProviderId::Codex,
        );
        assert_eq!(app.visible_providers(), vec![claude, agy, codex]);
        assert_eq!(app.selectable_providers(), vec![claude, agy, codex]);

        // Missing Antigravity still leaves Claude + Codex selectable.
        fail(&mut app, agy, ProviderError::NotInstalled);
        assert_eq!(app.visible_providers(), vec![claude, codex]);
        assert_eq!(app.selectable_providers(), vec![claude, codex]);

        // Picking Codex creates a Codex session with its own default model.
        let ws = WorkspaceId::new();
        app.home.workspaces = vec![Workspace {
            id: ws,
            root_path: dir.clone(),
            display_name: "w".into(),
            last_opened_at: OffsetDateTime::now_utc(),
            is_open: true,
        }];
        app.home.scope = Some(ws);
        app.writer
            .send(WriteOp::UpsertWorkspace(app.home.workspaces[0].clone()))
            .unwrap();
        app.home.provider_pick = Some(codex);
        let (p, model, _, _) = created(&rt, &mut app);
        assert_eq!(p, codex);
        assert_eq!(model, "gpt-6-luna");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
