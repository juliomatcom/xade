#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::timeout;
use xade_core::{AgentProvider, Effort, ProviderId, SessionStatus, Workspace, WorkspaceId};
use xade_runtime::{AppEvent, AppEventKind, ManagerCmd, SessionManager};
use xade_store::{StoreReader, StoreWriter, WriteOp, WriterHandle};

pub struct Harness {
    pub tx: UnboundedSender<ManagerCmd>,
    pub rx: UnboundedReceiver<AppEvent>,
    pub reader: StoreReader,
    pub writer: WriterHandle,
    pub workspace_id: WorkspaceId,
    pub cwd: PathBuf,
}

/// Temp store with one workspace at `cwd`, and a manager over `provider`.
pub async fn start(dir: &Path, provider: Arc<dyn AgentProvider>) -> Harness {
    let cwd = dir.join("project");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let db = dir.join("workspace.db");
    let writer = StoreWriter::spawn(&db).expect("writer");
    let workspace_id = WorkspaceId::new();
    writer
        .send(WriteOp::UpsertWorkspace(Workspace {
            id: workspace_id,
            root_path: cwd.clone(),
            display_name: "project".into(),
            last_opened_at: OffsetDateTime::now_utc(),
            is_pinned: false,
        }))
        .expect("upsert");
    xade_runtime::flush(&writer).await;
    let reader = StoreReader::open(&db).expect("reader");
    let (tx, rx) = SessionManager::start(vec![provider], writer.clone(), reader.clone());
    Harness {
        tx,
        rx,
        reader,
        writer,
        workspace_id,
        cwd,
    }
}

impl Harness {
    pub fn create(&self, model: &str) {
        self.tx
            .send(ManagerCmd::Create {
                workspace_id: self.workspace_id,
                cwd: self.cwd.clone(),
                provider: ProviderId::Claude,
                model_id: model.into(),
                effort: Effort::Default,
                auto_mode: true,
            })
            .expect("create");
    }

    /// Collects events until `done` matches one; returns all events seen, last included.
    pub async fn until(&mut self, secs: u64, done: impl Fn(&AppEvent) -> bool) -> Vec<AppEvent> {
        let mut seen = Vec::new();
        timeout(Duration::from_secs(secs), async {
            while let Some(ev) = self.rx.recv().await {
                let stop = done(&ev);
                seen.push(ev);
                if stop {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for event");
        seen
    }

    pub async fn until_status(&mut self, secs: u64, want: SessionStatus) -> Vec<AppEvent> {
        self.until(
            secs,
            |e| matches!(&e.kind, AppEventKind::Status(s) if *s == want),
        )
        .await
    }
}

pub fn statuses(events: &[AppEvent]) -> Vec<SessionStatus> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            AppEventKind::Status(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}
