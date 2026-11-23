//! `App`: all UI state, the event bridge, and the shared shell (top bar, status bar, sidebar).

use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_commonmark::CommonMarkCache;
use time::OffsetDateTime;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use xade_core::{
    AgentProvider, Delivery, Message, ProviderEvent, QuotaKind, QuotaWindow, Role, RunningSession,
    Session, SessionId, SessionStatus, Workspace, WorkspaceId,
};
use xade_runtime::git::{git_status, GitState};
use xade_runtime::{AppEvent, AppEventKind, ManagerCmd};
use xade_store::{Page, SessionCard, StoreReader, WriteOp, WriterHandle};

use crate::config::{self, Config};
use crate::diff::{is_code_change, DiffData};
use crate::editor::{self, CloseChoice, EditorId, Editors, SaveOutcome};
use crate::errors::{self, ErrorQueue, ErrorReport, ModalAction};
use crate::file_tree::{self, FileTree, Listing, TreeAction};
use crate::home::HomeState;
use crate::keymap::{self, Action, Chord};
use crate::running::{self, PendingOpen};
use crate::widgets::delete_modal::{self, PendingDelete};
use crate::widgets::keymaps_modal;
use crate::widgets::path_links::PathLinks;
use crate::widgets::session_picker::{self, PickerState};
use crate::workspace::{Tiling, WindowKind};
use crate::{composer, home, logging, status_bar, theme, workspace};

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
    Preflight(Option<String>),
    /// Quota rows saved by earlier runs.
    SavedQuota(Vec<QuotaWindow>),
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
    /// A file picked with Open File, read off the UI thread.
    FileOpened {
        path: PathBuf,
        text: Result<String, String>,
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
    /// A model or effort change waits for the next turn.
    pub settings_pending: bool,
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
}

pub struct DiffState {
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
        let find = |role: Role| {
            self.messages
                .iter()
                .find(|m| m.role == role && m.tool_use_id.as_deref() == Some(tool_use_id))
        };
        let (Some(call), result) = (find(Role::ToolCall), find(Role::ToolResult)) else {
            return;
        };
        let data = DiffData::build(
            call.tool_name.as_deref().unwrap_or_default(),
            call.tool_payload_json.as_deref().unwrap_or_default(),
            result.and_then(|r| r.tool_payload_json.as_deref()),
            &self.cwd,
        );
        self.diff = data.map(|data| DiffState {
            tool_use_id: tool_use_id.to_string(),
            data,
        });
    }
}

pub struct App {
    pub view: View,
    pub home: HomeState,
    pub sessions: HashMap<SessionId, SessionVm>,
    pub open_windows: Vec<WindowKind>,
    pub focused: Option<WindowKind>,
    pub editors: Editors,
    pub tree: FileTree,
    pub quota: Vec<QuotaWindow>,
    pub cmd_tx: UnboundedSender<ManagerCmd>,
    pub rt: Handle,
    pub git: HashMap<PathBuf, GitState>,
    /// Errors waiting for the modal; see `report`.
    pub errors: ErrorQueue,
    /// A non-fatal notice for the status bar, with when it appeared.
    warning: Option<(String, Instant)>,
    /// Where the log files live; `None` in headless tests.
    log_dir: Option<PathBuf>,
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
    pub sidebar_visible: bool,
    pub keymaps_open: bool,
    /// The session picker, when open.
    pub picker: Option<PickerState>,
    picker_generation: u64,
    /// Sessions running outside Xade, by provider session id.
    pub running: HashMap<String, RunningSession>,
    /// Awaiting confirmation to open a session that is running elsewhere.
    pub pending_open: Option<PendingOpen>,
    /// Awaiting confirmation to delete a session forever.
    pub pending_delete: Option<PendingDelete>,
    running_checked: Option<Instant>,
    tree_checked: Option<Instant>,
    running_inflight: bool,
    running_polled: bool,
    running_import_due: bool,
    running_import_at: Option<Instant>,
    /// Sessions closed for a reload; each reopens when the import that follows finishes.
    reloading: HashSet<SessionId>,
    /// Sessions being opened from Home with Cmd+click: they get a window but no view switch.
    background: HashSet<SessionId>,
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
        preflight_error: Option<String>,
        config_path: PathBuf,
        log_dir: PathBuf,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        let (config, config_error) = config::load(&config_path);
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
        let mut app = App {
            view: View::Home,
            home: HomeState {
                preflight_error,
                ..HomeState::default()
            },
            sessions: HashMap::new(),
            open_windows: Vec::new(),
            focused: None,
            editors: Editors::default(),
            tree: FileTree::default(),
            quota: Vec::new(),
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
            config: Config::default(),
            config_watch: Some(config::Watch::new(config_path)),
            chord: Chord::default(),
            sidebar_visible: true,
            keymaps_open: false,
            picker: None,
            picker_generation: 0,
            running: HashMap::new(),
            pending_open: None,
            pending_delete: None,
            running_checked: None,
            tree_checked: None,
            running_inflight: false,
            running_polled: false,
            running_import_due: false,
            running_import_at: None,
            reloading: HashSet::new(),
            background: HashSet::new(),
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
        app.refresh_workspaces();
        app.refresh_cards();
        app.load_saved_quota();
        if app.home.preflight_error.is_none() {
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
            open_windows: Vec::new(),
            focused: None,
            editors: Editors::default(),
            tree: FileTree::default(),
            quota: Vec::new(),
            cmd_tx: tokio::sync::mpsc::unbounded_channel().0,
            rt,
            git: HashMap::new(),
            errors: ErrorQueue::default(),
            warning: None,
            log_dir: None,
            providers: Vec::new(),
            writer,
            reader,
            tiling: Tiling::default(),
            maximized: None,
            raise: None,
            vscode: None,
            config: Config::default(),
            config_watch: None,
            chord: Chord::default(),
            sidebar_visible: true,
            keymaps_open: false,
            picker: None,
            picker_generation: 0,
            running: HashMap::new(),
            pending_open: None,
            pending_delete: None,
            running_checked: None,
            tree_checked: None,
            running_inflight: false,
            running_polled: false,
            running_import_due: false,
            running_import_at: None,
            reloading: HashSet::new(),
            background: HashSet::new(),
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
            let text = editor::read_file(&path);
            bridge.send(UiMsg::FileOpened { path, text });
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
        let (reader, bridge) = (self.reader.clone(), self.bridge.clone());
        self.rt.spawn(async move {
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

    /// Imports Claude Code history; one run at a time, finished by `ImportDone`.
    pub fn start_import(&mut self) {
        if self.home.importing {
            return;
        }
        self.home.importing = self.cmd_tx.send(ManagerCmd::Import).is_ok();
    }

    /// Polls running sessions every `running::POLL`; one poll in flight at a time.
    fn poll_running(&mut self) {
        let due = self
            .running_checked
            .is_none_or(|t| t.elapsed() >= running::POLL);
        if !due || self.running_inflight {
            return;
        }
        let Some(provider) = self.providers.first().cloned() else {
            return;
        };
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
            let list =
                running::exclude_own(provider.running_sessions(), &live, running::is_own_process);
            let transcripts = running::stat_transcripts(jobs);
            bridge.send(UiMsg::Running { list, transcripts });
        });
    }

    fn live_provider_ids(&self) -> HashSet<String> {
        self.sessions
            .values()
            .filter(|vm| live_status(&vm.status))
            .filter_map(|vm| vm.session.provider_session_id.clone())
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
        if self.is_live(card.id) {
            return None;
        }
        self.running.get(card.provider_session_id.as_deref()?)
    }

    /// Opens a card's session, asking first when it is running elsewhere.
    pub fn request_open(&mut self, card: &SessionCard, background: bool) {
        match self.running_for(card) {
            Some(r) => {
                self.pending_open = Some(PendingOpen {
                    id: card.id,
                    location: r.location.clone(),
                    background,
                });
            }
            None => self.open_confirmed(card.id, background),
        }
    }

    /// Asks for confirmation before deleting a card's session.
    pub fn request_delete(&mut self, card: &SessionCard) {
        self.pending_delete = Some(PendingDelete {
            id: card.id,
            title: card.title.clone(),
        });
    }

    pub fn open_confirmed(&mut self, id: SessionId, background: bool) {
        if background {
            self.open_session_background(id);
        } else {
            self.open_session(id);
        }
    }

    pub fn retry_preflight(&self) {
        let (provider, bridge) = (self.providers.first().cloned(), self.bridge.clone());
        self.rt.spawn_blocking(move || {
            let err = provider.and_then(|p| p.preflight().err().map(|e| e.to_string()));
            bridge.send(UiMsg::Preflight(err));
        });
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
            let root_path = folder.path().to_path_buf();
            let display_name = root_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| root_path.display().to_string());
            let added = writer.send(WriteOp::UpsertWorkspace(Workspace {
                id: WorkspaceId::new(),
                root_path,
                display_name,
                last_opened_at: OffsetDateTime::now_utc(),
                is_open: true,
            }));
            if let Err(e) = added {
                let report = ErrorReport::new("Could not add the folder", e.to_string());
                return bridge.send(UiMsg::Report(report));
            }
            xade_runtime::flush(&writer).await;
            match reader.list_workspaces().await {
                Ok(ws) => bridge.send(UiMsg::Workspaces(ws)),
                Err(e) => read_failed(&bridge, "workspaces", &e),
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
        let (Some(ws_id), Some(provider)) = (self.home.scope, self.providers.first()) else {
            return;
        };
        let Some(ws) = self.home.workspaces.iter().find(|w| w.id == ws_id) else {
            return;
        };
        // New sessions start with the model and effort of the last one, across restarts.
        let (cmd_tx, reader) = (self.cmd_tx.clone(), self.reader.clone());
        let (cwd, provider_id, default_model) = (
            ws.root_path.clone(),
            provider.id(),
            provider.default_model(),
        );
        self.rt.spawn(async move {
            let last = reader.last_model_choice().await.unwrap_or_else(|e| {
                tracing::warn!(error = %e, "could not read the last model choice");
                None
            });
            let (model_id, effort) = last.unwrap_or((default_model, xade_core::Effort::Default));
            let _ = cmd_tx.send(ManagerCmd::Create {
                workspace_id: ws_id,
                cwd,
                provider: provider_id,
                model_id,
                effort,
                auto_mode: false,
            });
        });
    }

    /// Opens a stored session (spawning it when not live) and shows it.
    pub fn open_session(&mut self, id: SessionId) {
        if self.home.preflight_error.is_some() {
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
        if self.home.preflight_error.is_some() {
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
            WindowKind::Editor(id) => self.editors.map.contains_key(&id),
        }
    }

    fn add_window(&mut self, kind: WindowKind) {
        if !self.open_windows.contains(&kind) {
            self.open_windows.push(kind);
            self.maximized = None;
            self.tiling.retile();
        }
    }

    pub fn focus(&mut self, kind: WindowKind) {
        self.add_window(kind);
        self.focused = Some(kind);
        self.raise = Some(kind);
        self.view = View::Workspace;
    }

    /// The focused window when it is a live session.
    fn focused_session(&self) -> Option<SessionId> {
        self.focused
            .and_then(WindowKind::session)
            .filter(|id| self.sessions.contains_key(id))
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
            let text = tokio::task::spawn_blocking(move || editor::read_file(&read))
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            bridge.send(UiMsg::FileOpened { path, text });
        });
    }

    fn on_file_opened(&mut self, path: PathBuf, text: Result<String, String>) {
        match text {
            Err(e) => self.report(ErrorReport::new("Could not open the file", e)),
            Ok(text) => {
                let id = match self.editors.find_path(&path) {
                    Some(id) => id,
                    None => self.editors.create(Some(path), text),
                };
                self.focus(WindowKind::Editor(id));
            }
        }
    }

    /// Writes the editor's text, asking for a path first when it has none or `as_new`.
    pub fn save_editor(&mut self, id: EditorId, as_new: bool, close_after: bool) {
        let dir = self.target_dir();
        let Some(vm) = self.editors.map.get_mut(&id).filter(|vm| !vm.busy) else {
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
        let failures: Vec<String> = self
            .editors
            .map
            .values_mut()
            .filter_map(|vm| vm.check_disk().err())
            .collect();
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
                Some(WindowKind::Editor(id)) => {
                    if let Some(vm) = self.editors.map.get(&id) {
                        let parent = vm.path.as_deref().and_then(|p| p.parent());
                        return Some(parent.map_or_else(|| self.target_dir(), PathBuf::from));
                    }
                }
                None => {}
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

    /// Sessions of the workspace whose child is running.
    pub fn live_count(&self, id: WorkspaceId) -> usize {
        self.sessions
            .values()
            .filter(|vm| vm.session.workspace_id == id)
            .count()
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
            Action::Stop => {
                if let Some(vm) = focused.and_then(|id| self.sessions.get(&id)) {
                    if vm.status == SessionStatus::Streaming {
                        let _ = self.cmd_tx.send(ManagerCmd::Interrupt {
                            session_id: vm.session.id,
                        });
                    }
                }
            }
            Action::CloseWindow => {
                if let (true, Some(id)) = (in_workspace, focused) {
                    self.close_session(id);
                } else if let (true, Some(id)) = (in_workspace, focused_editor) {
                    self.request_close_editor(id);
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
                } else if !self.open_windows.is_empty() {
                    self.view = View::Workspace;
                }
            }
            Action::ToggleAuto => {
                if let Some(vm) = focused.and_then(|id| self.sessions.get_mut(&id)) {
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

    fn on_msg(&mut self, msg: UiMsg) {
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
            UiMsg::Running { list, transcripts } => self.on_running(list, transcripts),
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
            UiMsg::FileOpened { path, text } => self.on_file_opened(path, text),
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
            UiMsg::Preflight(err) => {
                let recovered = err.is_none() && self.home.preflight_error.is_some();
                self.home.preflight_error = err;
                if recovered {
                    self.start_import();
                }
            }
            UiMsg::Report(report) => self.report(report),
            UiMsg::SavedQuota(saved) => {
                // A live report may have arrived first; it wins.
                for q in saved {
                    if !self.quota.iter().any(|x| x.kind == q.kind) {
                        self.quota.push(q);
                    }
                }
                self.quota.sort_by_key(|q| q.kind == QuotaKind::SevenDay);
            }
        }
    }

    fn insert_vm(&mut self, session: Session, messages: Vec<Message>) {
        let id = session.id;
        let cwd = self
            .home
            .workspaces
            .iter()
            .find(|w| w.id == session.workspace_id)
            .map(|w| w.root_path.clone())
            .unwrap_or_default();
        self.sessions.insert(
            id,
            SessionVm {
                cwd,
                session,
                status: SessionStatus::Starting,
                queued: Default::default(),
                messages,
                live_text: String::new(),
                thinking_tokens: None,
                pending_permission: None,
                input: String::new(),
                error: None,
                md_cache: CommonMarkCache::default(),
                path_links: PathLinks::default(),
                diff: None,
                settings_pending: false,
                collapsed: false,
                window_rect: None,
                apply_rect: None,
                sync: Default::default(),
                mention: Default::default(),
                attach_note: None,
            },
        );
        if self.background.remove(&id) {
            self.add_window(WindowKind::Session(id));
        } else {
            self.focus(WindowKind::Session(id));
        }
    }

    fn on_app_event(&mut self, ev: AppEvent) {
        let AppEvent { session_id, kind } = ev;
        match (session_id, kind) {
            (_, AppEventKind::Quota(q)) => {
                self.quota.retain(|x| x.kind != q.kind);
                self.quota.push(q);
                self.quota.sort_by_key(|q| q.kind == QuotaKind::SevenDay);
            }
            (_, AppEventKind::ImportDone { imported, updated }) => {
                self.home.importing = false;
                self.home.import_result = Some((imported, updated));
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
            (Some(_), AppEventKind::SessionCreated(s)) => {
                self.insert_vm(s, Vec::new());
                self.refresh_workspaces();
                self.refresh_cards();
            }
            (Some(_), AppEventKind::SessionOpened { session, messages }) => {
                self.insert_vm(session, messages);
                self.refresh_workspaces();
            }
            (Some(id), AppEventKind::Status(status)) => {
                if status == SessionStatus::Closed {
                    return self.drop_session(id);
                }
                if let Some(vm) = self.sessions.get_mut(&id) {
                    if status != SessionStatus::NeedsApproval {
                        vm.pending_permission = None;
                    }
                    vm.status = status;
                    if vm.status == SessionStatus::Idle {
                        if let Some(text) = vm.queued.pop_front() {
                            let delivery = Some(Delivery::Queued);
                            push_user_message(vm, text.clone(), delivery);
                            vm.error = None;
                            let _ = self.cmd_tx.send(ManagerCmd::Send {
                                session_id: id,
                                text,
                                delivery,
                            });
                        }
                    }
                }
            }
            (Some(id), AppEventKind::Provider(p)) => self.on_provider_event(id, p),
            _ => {}
        }
    }

    fn on_provider_event(&mut self, id: SessionId, ev: ProviderEvent) {
        let Some(vm) = self.sessions.get_mut(&id) else {
            return;
        };
        let dir = vm.cwd.clone();
        let mut refresh_git = false;
        let mut refresh_cards = false;
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
                if let Some(data) = diff {
                    vm.diff = Some(DiffState { tool_use_id, data });
                }
            }
            ProviderEvent::TurnComplete { usage, .. } => {
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
    }
}

/// A store read that failed in a background task: the user sees it, with the cause.
fn read_failed(bridge: &Bridge, what: &str, e: &xade_store::StoreError) {
    let report = ErrorReport::new(format!("Could not read {what}"), e.to_string());
    bridge.send(UiMsg::Report(report));
}

/// A session whose child is alive (or starting); an errored one has none.
fn live_status(status: &SessionStatus) -> bool {
    !matches!(status, SessionStatus::Error(_))
}

/// Builds a message for local display (not persisted here; the session actor owns writes).
fn new_message(vm: &SessionVm, role: Role, text: String, tool_use_id: Option<String>) -> Message {
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
    }
}

/// Appends the user's prompt to the local transcript.
pub fn push_user_message(vm: &mut SessionVm, text: String, delivery: Option<Delivery>) {
    let mut m = new_message(vm, Role::User, text, None);
    m.tool_name = delivery.map(|d| d.tag().to_string());
    vm.messages.push(m);
}

impl eframe::App for App {
    /// Drains the bridge and runs timers; no painting here.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.guarded("frame update", |app| app.update(ctx));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.guarded_ui(ui, Self::draw);
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
            let waited = self
                .rt
                .block_on(tokio::time::timeout(Duration::from_secs(5), rx));
            if waited.is_err() {
                tracing::warn!("sessions did not shut down within 5 s");
            }
        }
    }
}

/// The session window under `pointer`, else `fallback`.
fn drop_target(
    pointer: Option<egui::Pos2>,
    rects: &[(SessionId, egui::Rect)],
    fallback: Option<SessionId>,
) -> Option<SessionId> {
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
        self.handle_shortcuts(ctx);
        self.handle_drops(ctx);
        self.reload_config(ctx);
        self.check_editors(ctx, false);
        self.poll_running();
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
        let rects: Vec<(SessionId, egui::Rect)> = self
            .open_windows
            .iter()
            .filter_map(|k| k.session())
            .filter_map(|id| Some((id, self.sessions.get(&id)?.window_rect?)))
            .collect();
        let only = (self.sessions.len() == 1)
            .then(|| self.sessions.keys().next().copied())
            .flatten();
        let target = if self.view == View::Workspace {
            drop_target(pointer, &rects, self.focused_session().or(only))
        } else {
            None
        };
        match target.and_then(|id| self.sessions.get_mut(&id)) {
            Some(vm) => composer::insert_mentions(vm, &dropped),
            None => self.warn("Drop files on a session window to mention them."),
        }
    }

    /// Draws the whole window except the error modal.
    fn draw(&mut self, ui: &mut egui::Ui) {
        home::top_bar(self, ui);
        if status_bar::show(self, ui) {
            self.open_settings();
        }
        if self.sidebar_visible {
            home::sidebar(self, ui);
        }
        let frame = egui::Frame::new()
            .fill(theme::BG_PANEL)
            .inner_margin(egui::Margin::same(16));
        egui::CentralPanel::default()
            .frame(frame)
            .show(ui, |ui| match self.view {
                View::Home => home::grid(self, ui),
                View::Workspace => workspace::show(self, ui),
            });
        let ctx = ui.ctx().clone();
        session_picker::show(self, &ctx);
        running::confirm_modal(self, &ctx);
        delete_modal::show(self, &ctx);
        self.editor_modal(&ctx);
        if self.keymaps_open && keymaps_modal::show(&ctx) {
            self.keymaps_open = false;
        }
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
            text: Ok(text),
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
        assert!(app.picker.is_some());
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
            auto_mode: false,
            usage: xade_core::UsageTotals::default(),
            created_at: now,
            updated_at: now,
        }
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
}
