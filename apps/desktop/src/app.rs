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
    AgentProvider, Message, ProviderEvent, QuotaKind, QuotaWindow, Role, Session, SessionId,
    SessionStatus, Workspace, WorkspaceId,
};
use xade_runtime::git::{git_status, GitState};
use xade_runtime::{AppEvent, AppEventKind, ManagerCmd};
use xade_store::{Page, SessionCard, StoreReader, WriteOp, WriterHandle};

use crate::diff::{is_code_change, DiffData};
use crate::home::HomeState;
use crate::workspace::Tiling;
use crate::{home, icons, status_bar, theme, workspace};

const GIT_REFRESH: Duration = Duration::from_secs(5);

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
        let live = self
            .sessions
            .get(&id)
            .is_some_and(|vm| !matches!(vm.status, SessionStatus::Error(_)));
        if !live {
            let _ = self.cmd_tx.send(ManagerCmd::Open { session_id: id });
        }
        self.focus(id);
    }

    fn focus(&mut self, id: SessionId) {
        if !self.open_windows.contains(&id) {
            self.open_windows.push(id);
            self.maximized = None;
        }
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
        self.focus(id);
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
        home::sidebar(self, ui);
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
