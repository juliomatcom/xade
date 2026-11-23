//! `App`: all UI state, the event bridge, and the shared shell (top bar, status bar, sidebar).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::RichText;
use egui_commonmark::CommonMarkCache;
use time::OffsetDateTime;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use xade_core::{
    AgentProvider, Message, ProviderEvent, QuotaKind, QuotaWindow, Role, RunningSession, Session,
    SessionId, SessionStatus, Workspace, WorkspaceId,
};
use xade_runtime::git::{git_status, GitState};
use xade_runtime::{AppEvent, AppEventKind, ManagerCmd};
use xade_store::{Page, SessionCard, StoreReader, WriteOp, WriterHandle};

use crate::diff::{is_code_change, DiffData};
use crate::home::HomeState;
use crate::keymap::{self, Action, Chord};
use crate::running::{self, PendingOpen};
use crate::widgets::keymaps_modal;
use crate::widgets::session_picker::{self, PickerState};
use crate::workspace::Tiling;
use crate::{home, icons, status_bar, theme, workspace};

const GIT_REFRESH: Duration = Duration::from_secs(5);
/// A running-set change re-imports at most this often.
const RUNNING_IMPORT_GAP: Duration = Duration::from_secs(30);

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
    PickerCards {
        generation: u64,
        page: Page<SessionCard>,
    },
    /// One `running_sessions` poll.
    Running(Vec<RunningSession>),
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
    pub messages: Vec<Message>,
    /// Streaming text of the block in flight; replaced by the finalized block.
    pub live_text: String,
    pub thinking_tokens: Option<u64>,
    pub pending_permission: Option<PendingPermission>,
    pub input: String,
    pub error: Option<String>,
    pub md_cache: CommonMarkCache,
    /// The diff modal, when open.
    pub diff: Option<DiffState>,
    /// A model or effort change waits for the next turn.
    pub settings_pending: bool,
    pub collapsed: bool,
    /// Last rect of the expanded window.
    pub window_rect: Option<egui::Rect>,
    /// One-frame layout override (tiling, collapse, expand).
    pub apply_rect: Option<egui::Rect>,
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
    pub open_windows: Vec<SessionId>,
    pub focused: Option<SessionId>,
    pub quota: Vec<QuotaWindow>,
    pub cmd_tx: UnboundedSender<ManagerCmd>,
    pub rt: Handle,
    pub git: HashMap<PathBuf, GitState>,
    pub error: Option<String>,
    pub providers: Vec<Arc<dyn AgentProvider>>,
    pub writer: WriterHandle,
    pub reader: StoreReader,
    pub tiling: Tiling,
    /// The session window filling the central area; the others are collapsed.
    pub maximized: Option<SessionId>,
    /// Window to bring to the front on the next frame.
    pub raise: Option<SessionId>,
    pub vscode: Option<PathBuf>,
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
    running_checked: Option<Instant>,
    running_inflight: bool,
    running_polled: bool,
    running_import_due: bool,
    running_import_at: Option<Instant>,
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
    ) -> Self {
        theme::apply(&cc.egui_ctx);
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
            quota: Vec::new(),
            cmd_tx,
            rt,
            git: HashMap::new(),
            error: None,
            providers,
            writer,
            reader,
            tiling: Tiling::default(),
            maximized: None,
            raise: None,
            vscode: crate::widgets::diff_modal::find_code(),
            chord: Chord::default(),
            sidebar_visible: true,
            keymaps_open: false,
            picker: None,
            picker_generation: 0,
            running: HashMap::new(),
            pending_open: None,
            running_checked: None,
            running_inflight: false,
            running_polled: false,
            running_import_due: false,
            running_import_at: None,
            background: HashSet::new(),
            events_rx,
            bridge,
            git_inflight: HashSet::new(),
            git_stale: HashSet::new(),
            git_dir: None,
            git_checked: Instant::now(),
        };
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
            quota: Vec::new(),
            cmd_tx: tokio::sync::mpsc::unbounded_channel().0,
            rt,
            git: HashMap::new(),
            error: None,
            providers: Vec::new(),
            writer,
            reader,
            tiling: Tiling::default(),
            maximized: None,
            raise: None,
            vscode: None,
            chord: Chord::default(),
            sidebar_visible: true,
            keymaps_open: false,
            picker: None,
            picker_generation: 0,
            running: HashMap::new(),
            pending_open: None,
            running_checked: None,
            running_inflight: false,
            running_polled: false,
            running_import_due: false,
            running_import_at: None,
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

    // ----- background work -----

    pub fn refresh_workspaces(&self) {
        let (reader, bridge) = (self.reader.clone(), self.bridge.clone());
        self.rt.spawn(async move {
            match reader.list_workspaces().await {
                Ok(ws) => bridge.send(UiMsg::Workspaces(ws)),
                Err(e) => tracing::error!(error = %e, "list workspaces failed"),
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
                Err(e) => tracing::error!(error = %e, "session query failed"),
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
                Err(e) => tracing::error!(error = %e, "picker query failed"),
            }
        });
    }

    fn load_saved_quota(&self) {
        let (reader, bridge) = (self.reader.clone(), self.bridge.clone());
        self.rt.spawn(async move {
            match reader.load_quota().await {
                Ok(q) => bridge.send(UiMsg::SavedQuota(q)),
                Err(e) => tracing::error!(error = %e, "load quota failed"),
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
        let bridge = self.bridge.clone();
        self.rt.spawn_blocking(move || {
            bridge.send(UiMsg::Running(provider.running_sessions()));
        });
    }

    /// Stores a poll without Xade's own live sessions and queues a re-import when the set changed.
    fn on_running(&mut self, list: Vec<RunningSession>) {
        self.running_inflight = false;
        let own: HashSet<&str> = self
            .sessions
            .values()
            .filter(|vm| !matches!(vm.status, SessionStatus::Error(_)))
            .filter_map(|vm| vm.session.provider_session_id.as_deref())
            .collect();
        let next: HashMap<String, RunningSession> = list
            .into_iter()
            .filter(|r| !own.contains(r.provider_session_id.as_str()))
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
            let _ = writer.send(WriteOp::UpsertWorkspace(Workspace {
                id: WorkspaceId::new(),
                root_path,
                display_name,
                last_opened_at: OffsetDateTime::now_utc(),
                is_pinned: false,
            }));
            xade_runtime::flush(&writer).await;
            if let Ok(ws) = reader.list_workspaces().await {
                bridge.send(UiMsg::Workspaces(ws));
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
        let _ = self.cmd_tx.send(ManagerCmd::Create {
            workspace_id: ws_id,
            cwd: ws.root_path.clone(),
            provider: provider.id(),
            model_id: provider.default_model(),
            effort: xade_core::Effort::Default,
            auto_mode: false,
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
        self.focus(id);
    }

    /// Opens a stored session as a window without leaving the current view (Cmd+click on Home).
    pub fn open_session_background(&mut self, id: SessionId) {
        if self.home.preflight_error.is_some() {
            return;
        }
        if self.is_live(id) {
            self.add_window(id);
        } else if self.background.insert(id) {
            let _ = self.cmd_tx.send(ManagerCmd::Open { session_id: id });
        }
    }

    fn is_live(&self, id: SessionId) -> bool {
        self.sessions
            .get(&id)
            .is_some_and(|vm| !matches!(vm.status, SessionStatus::Error(_)))
    }

    fn add_window(&mut self, id: SessionId) {
        if !self.open_windows.contains(&id) {
            self.open_windows.push(id);
            self.maximized = None;
        }
    }

    fn focus(&mut self, id: SessionId) {
        self.add_window(id);
        self.focused = Some(id);
        self.raise = Some(id);
        self.view = View::Workspace;
    }

    pub fn close_session(&mut self, id: SessionId) {
        let _ = self.cmd_tx.send(ManagerCmd::Close { session_id: id });
        self.drop_session(id);
    }

    fn drop_session(&mut self, id: SessionId) {
        self.sessions.remove(&id);
        self.open_windows.retain(|s| *s != id);
        if self.maximized == Some(id) {
            self.maximized = None;
            self.tiling.retile();
        }
        if self.focused == Some(id) {
            self.focused = self.open_windows.last().copied();
        }
        if self.open_windows.is_empty() {
            self.view = View::Home;
        }
    }

    /// The directory the status bar describes.
    pub fn active_dir(&self) -> Option<PathBuf> {
        if self.view == View::Workspace {
            if let Some(vm) = self.focused.and_then(|id| self.sessions.get(&id)) {
                return Some(vm.cwd.clone());
            }
        }
        let ws = &self.home.workspaces;
        match self.home.scope {
            Some(id) => ws.iter().find(|w| w.id == id),
            None => ws.first(),
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
        let focused = self.focused.filter(|id| self.sessions.contains_key(id));
        let in_workspace = self.view == View::Workspace;
        match action {
            Action::NewSession => {
                if self.home.scope.is_none() {
                    self.error = Some("Select a workspace in the sidebar first.".into());
                } else {
                    self.new_session();
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
                }
            }
            Action::FocusWindow(n) => {
                let id = self
                    .open_windows
                    .get(usize::from(n).saturating_sub(1))
                    .copied();
                if let Some(id) = id.filter(|id| self.sessions.contains_key(id)) {
                    self.focus(id);
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
                if let (true, Some(id)) = (in_workspace, focused) {
                    self.maximized = (self.maximized != Some(id)).then_some(id);
                    self.tiling.retile();
                    self.raise = Some(id);
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
            // Handled by the composer and the modals themselves.
            Action::Send | Action::CloseModal => {}
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
            UiMsg::Running(list) => self.on_running(list),
            UiMsg::Workspaces(ws) => {
                if self
                    .home
                    .scope
                    .is_some_and(|s| !ws.iter().any(|w| w.id == s))
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
                messages,
                live_text: String::new(),
                thinking_tokens: None,
                pending_permission: None,
                input: String::new(),
                error: None,
                md_cache: CommonMarkCache::default(),
                diff: None,
                settings_pending: false,
                collapsed: false,
                window_rect: None,
                apply_rect: None,
            },
        );
        if self.background.remove(&id) {
            self.add_window(id);
        } else {
            self.focus(id);
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
            }
            (_, AppEventKind::Git { dir, state }) => {
                self.git.insert(dir.clone(), state);
                self.git_inflight.remove(&dir);
                if self.git_stale.remove(&dir) {
                    self.refresh_git(dir);
                }
            }
            (_, AppEventKind::Error(msg)) => self.error = Some(msg),
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
pub fn push_user_message(vm: &mut SessionVm, text: String) {
    let m = new_message(vm, Role::User, text, None);
    vm.messages.push(m);
}

impl eframe::App for App {
    /// Drains the bridge and runs timers; no painting here.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.events_rx.try_recv() {
            self.on_msg(msg);
        }
        self.handle_shortcuts(ctx);
        self.poll_running();
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

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        home::top_bar(self, ui);
        status_bar::show(self, ui);
        if self.sidebar_visible {
            home::sidebar(self, ui);
        }
        let frame = egui::Frame::new()
            .fill(theme::BG_PANEL)
            .inner_margin(egui::Margin::same(16));
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            if let Some(err) = self.error.clone() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&err).color(theme::DANGER));
                    if ui
                        .small_button(icons::with(icons::CLOSE, "Dismiss"))
                        .clicked()
                    {
                        self.error = None;
                    }
                });
            }
            match self.view {
                View::Home => home::grid(self, ui),
                View::Workspace => workspace::show(self, ui),
            }
        });
        let ctx = ui.ctx().clone();
        session_picker::show(self, &ctx);
        running::confirm_modal(self, &ctx);
        if self.keymaps_open && keymaps_modal::show(&ctx) {
            self.keymaps_open = false;
        }
    }

    /// eframe calls this on the window close button and on Cmd+Q (via `exiting`),
    /// possibly twice; a repeat `ShutdownAll` returns at once.
    fn on_exit(&mut self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.cmd_tx.send(ManagerCmd::ShutdownAll(tx)).is_ok() {
            let _ = self
                .rt
                .block_on(tokio::time::timeout(Duration::from_secs(5), rx));
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
}
