#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::path::PathBuf;
use std::thread;

use aw_core::{
    Effort, Message, ProviderId, Role, Session, SessionId, UsageTotals, Workspace, WorkspaceId,
};
use aw_store::{StoreReader, StoreWriter, WriteOp, WriterHandle, SCHEMA_VERSION};
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

fn block_on<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(f)
}

struct Fixture {
    _dir: TempDir,
    path: PathBuf,
    writer: WriterHandle,
    reader: StoreReader,
    ws: WorkspaceId,
    base: OffsetDateTime,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("workspace.db");
        let writer = StoreWriter::spawn(&path).unwrap();
        let reader = StoreReader::open(&path).unwrap();
        let ws = WorkspaceId::new();
        writer
            .send(WriteOp::UpsertWorkspace(workspace(ws, "/tmp/project")))
            .unwrap();
        Self {
            _dir: dir,
            path,
            writer,
            reader,
            ws,
            base: OffsetDateTime::now_utc(),
        }
    }

    /// Inserts a session; `age` seconds older than the fixture base time.
    fn add_session(&self, title: &str, age: i64) -> SessionId {
        self.add_session_in(self.ws, title, age)
    }

    fn add_session_in(&self, ws: WorkspaceId, title: &str, age: i64) -> SessionId {
        let s = session(ws, title, self.base - Duration::seconds(age));
        let id = s.id;
        self.writer.send(WriteOp::InsertSession(s)).unwrap();
        id
    }

    fn add_message(&self, session: SessionId, seq: i64, role: Role, text: &str) {
        self.writer
            .send(WriteOp::InsertMessage(message(session, seq, role, text)))
            .unwrap();
    }

    fn search(&self, scope: Option<WorkspaceId>, q: &str) -> aw_store::Page<aw_store::SessionCard> {
        self.writer.flush().unwrap();
        block_on(self.reader.search(scope, q.to_string(), 0, 24)).unwrap()
    }

    fn titles(&self, q: &str) -> Vec<String> {
        self.search(None, q)
            .items
            .into_iter()
            .map(|c| c.title)
            .collect()
    }
}

fn workspace(id: WorkspaceId, root: &str) -> Workspace {
    Workspace {
        id,
        root_path: root.into(),
        display_name: root.rsplit('/').next().unwrap().to_string(),
        last_opened_at: OffsetDateTime::now_utc(),
        is_pinned: false,
    }
}

fn session(ws: WorkspaceId, title: &str, at: OffsetDateTime) -> Session {
    Session {
        id: SessionId::new(),
        workspace_id: ws,
        provider_id: ProviderId::Claude,
        provider_session_id: None,
        title: title.to_string(),
        preview_snippet: String::new(),
        model_id: "sonnet".into(),
        effort: Effort::Default,
        auto_mode: false,
        usage: UsageTotals::default(),
        created_at: at,
        updated_at: at,
    }
}

fn message(session: SessionId, seq: i64, role: Role, text: &str) -> Message {
    Message {
        id: Uuid::new_v4(),
        session_id: session,
        seq,
        role,
        content_text: text.to_string(),
        thinking_text: None,
        tool_name: None,
        tool_use_id: None,
        tool_payload_json: None,
        is_error: false,
        created_at: OffsetDateTime::now_utc(),
    }
}

fn user_version(path: &std::path::Path) -> u32 {
    rusqlite::Connection::open(path)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

#[test]
fn migrations_are_idempotent_across_reopen() {
    let f = Fixture::new();
    f.add_session("first", 0);
    f.writer.flush().unwrap();
    assert_eq!(user_version(&f.path), SCHEMA_VERSION);
    let before = block_on(f.reader.list_sessions(None, 0, 24)).unwrap();

    for _ in 0..2 {
        let again = StoreWriter::spawn(&f.path).unwrap();
        again.flush().unwrap();
        assert_eq!(user_version(&f.path), SCHEMA_VERSION);
    }
    let after = block_on(f.reader.list_sessions(None, 0, 24)).unwrap();
    assert_eq!(before, after);
}

#[test]
fn wal_mode_is_enabled() {
    let f = Fixture::new();
    let mode: String = rusqlite::Connection::open(&f.path)
        .unwrap()
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn pagination_returns_24_then_6_of_30() {
    let f = Fixture::new();
    for i in 0..30 {
        let id = f.add_session(&format!("session {i:02}"), 100 - i);
        f.add_message(id, 1, Role::User, "hello");
    }
    f.writer.flush().unwrap();
    let p0 = block_on(f.reader.list_sessions(Some(f.ws), 0, 24)).unwrap();
    let p1 = block_on(f.reader.list_sessions(Some(f.ws), 1, 24)).unwrap();
    assert_eq!((p0.items.len(), p0.total), (24, 30));
    assert_eq!((p1.items.len(), p1.total), (6, 30));
    assert_eq!((p1.page, p1.page_size), (1, 24));
}

#[test]
fn sessions_list_newest_first_and_respect_scope() {
    let f = Fixture::new();
    let other = WorkspaceId::new();
    f.writer
        .send(WriteOp::UpsertWorkspace(workspace(other, "/tmp/other")))
        .unwrap();
    f.add_session("old", 50);
    f.add_session("new", 1);
    f.add_session_in(other, "elsewhere", 10);
    f.writer.flush().unwrap();
    let all = block_on(f.reader.list_sessions(None, 0, 24)).unwrap();
    let titles: Vec<_> = all.items.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["new", "elsewhere", "old"]);
    let scoped = block_on(f.reader.list_sessions(Some(f.ws), 0, 24)).unwrap();
    assert_eq!(scoped.total, 2);
}

#[test]
fn workspaces_sorted_by_last_opened() {
    let f = Fixture::new();
    let second = WorkspaceId::new();
    let mut w = workspace(second, "/tmp/second");
    w.last_opened_at += Duration::hours(1);
    f.writer.send(WriteOp::UpsertWorkspace(w)).unwrap();
    f.writer.flush().unwrap();
    let list = block_on(f.reader.list_workspaces()).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, second);

    f.writer.send(WriteOp::TouchWorkspace(f.ws)).unwrap();
    f.writer
        .send(WriteOp::UpsertWorkspace(w_pinned(second)))
        .unwrap();
    f.writer.flush().unwrap();
    let list = block_on(f.reader.list_workspaces()).unwrap();
    assert_eq!(list.len(), 2, "same root_path upserts in place");
}

fn w_pinned(id: WorkspaceId) -> Workspace {
    Workspace {
        is_pinned: true,
        ..workspace(id, "/tmp/second")
    }
}

#[test]
fn message_body_word_finds_session_with_excerpt() {
    let f = Fixture::new();
    let a = f.add_session("Unrelated title", 5);
    f.add_session("Another one", 4);
    f.add_message(
        a,
        1,
        Role::User,
        "please investigate the flaky websocket reconnect",
    );
    f.add_message(
        a,
        2,
        Role::ToolCall,
        "websocket tool call text is not indexed",
    );
    let found = f.search(None, "websocket").items;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, a);
    assert!(found[0].excerpt.as_deref().unwrap().contains("websocket"));
}

#[test]
fn tool_and_system_messages_are_not_indexed() {
    let f = Fixture::new();
    let a = f.add_session("Plain", 1);
    f.add_message(a, 1, Role::ToolResult, "zebrafish output");
    f.add_message(a, 2, Role::System, "zebrafish system");
    assert!(f.titles("zebrafish").is_empty());
}

#[test]
fn fuzzy_title_matches_refactr() {
    let f = Fixture::new();
    f.add_session("Refactor auth middleware", 3);
    f.add_session("Write release notes", 2);
    assert_eq!(f.titles("refactr"), ["Refactor auth middleware"]);
}

#[test]
fn unrelated_fuzzy_noise_is_filtered() {
    let f = Fixture::new();
    f.add_session("Write release notes", 2);
    assert!(f.titles("qzx").is_empty());
}

#[test]
fn two_char_query_uses_like_on_title_and_preview() {
    let f = Fixture::new();
    f.add_session("Fix CI", 3);
    let b = f.add_session("Other", 2);
    f.add_message(b, 1, Role::User, "run the ci pipeline");
    f.add_session("Nothing", 1);
    let titles = f.titles("ci");
    assert_eq!(titles.len(), 2);
    assert!(titles.contains(&"Fix CI".to_string()));
    assert!(titles.contains(&"Other".to_string()));
    assert_eq!(f.titles(" 100% ").len(), 0, "wildcards are literal");
    assert_eq!(f.titles("%").len(), 0);
}

#[test]
fn quotes_and_operators_do_not_error() {
    let f = Fixture::new();
    f.add_session("Use \"quoted\" text", 1);
    for q in [
        "\"",
        "*",
        "a\"b*",
        "\"\"\"",
        "foo*",
        "NEAR(a b)",
        "x AND",
        "(",
        "title:foo",
        "-bar baz",
        "'; DROP TABLE sessions; --",
    ] {
        let _ = f.search(None, q);
    }
    assert_eq!(f.titles("\"quoted\""), ["Use \"quoted\" text"]);
}

#[test]
fn empty_query_lists_everything() {
    let f = Fixture::new();
    f.add_session("a", 2);
    f.add_session("b", 1);
    assert_eq!(f.search(None, "   ").total, 2);
}

#[test]
fn ranking_exact_title_then_fts_then_fuzzy() {
    let f = Fixture::new();
    // Fuzzy-only: "deploy" is a subsequence but not a substring (extra "o").
    let fuzzy = f.add_session("Deplooy staging", 1);
    // FTS-only: body mention, newest of the matching sessions' ties irrelevant.
    let body = f.add_session("Unrelated", 2);
    f.add_message(body, 1, Role::Assistant, "we should deploy on friday");
    // Exact title substring, oldest.
    let exact = f.add_session("Plan the DEPLOY pipeline", 100);
    let hits = f.search(None, "deploy").items;
    let ids: Vec<_> = hits.iter().map(|c| c.id).collect();
    assert_eq!(ids, [exact, body, fuzzy]);
}

#[test]
fn ties_in_a_tier_go_to_most_recently_updated() {
    let f = Fixture::new();
    let old = f.add_session("deploy old", 100);
    let new = f.add_session("deploy new", 1);
    let ids: Vec<_> = f
        .search(None, "deploy")
        .items
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, [new, old]);
}

#[test]
fn search_respects_scope() {
    let f = Fixture::new();
    let other = WorkspaceId::new();
    f.writer
        .send(WriteOp::UpsertWorkspace(workspace(other, "/tmp/other")))
        .unwrap();
    f.add_session("Refactor here", 2);
    f.add_session_in(other, "Refactor there", 1);
    assert_eq!(f.search(Some(f.ws), "refactor").total, 1);
    assert_eq!(f.search(None, "refactor").total, 2);
}

#[test]
fn search_paginates() {
    let f = Fixture::new();
    for i in 0..30 {
        f.add_session(&format!("deploy {i}"), i);
    }
    f.writer.flush().unwrap();
    let p1 = block_on(f.reader.search(None, "deploy".into(), 1, 24)).unwrap();
    assert_eq!((p1.items.len(), p1.total), (6, 30));
}

#[test]
fn title_update_reindexes_and_delete_cascades() {
    let f = Fixture::new();
    let id = f.add_session("Alpha topic", 1);
    f.add_message(id, 1, Role::User, "gamma body");
    assert_eq!(f.titles("alpha").len(), 1);
    f.writer
        .send(WriteOp::UpdateSession {
            id,
            title: Some("Beta topic".into()),
            provider_session_id: None,
            model_id: None,
            effort: None,
            auto_mode: None,
            preview_snippet: None,
            usage: None,
        })
        .unwrap();
    assert!(f.titles("alpha").is_empty());
    assert_eq!(f.titles("beta").len(), 1);

    let conn = rusqlite::Connection::open(&f.path).unwrap();
    conn.execute("PRAGMA foreign_keys = ON", []).unwrap();
    conn.execute("DELETE FROM sessions WHERE id = ?1", [id.0.to_string()])
        .unwrap();
    assert!(f.titles("beta").is_empty());
    assert!(f.titles("gamma").is_empty());
}

#[test]
fn load_session_roundtrip_and_max_seq() {
    let f = Fixture::new();
    let id = f.add_session("Round trip", 1);
    f.writer.flush().unwrap();
    assert_eq!(block_on(f.reader.max_seq(id)).unwrap(), 0);

    let mut m = message(id, 1, Role::ToolCall, "Edit");
    m.tool_name = Some("Edit".into());
    m.tool_use_id = Some("toolu_1".into());
    m.tool_payload_json = Some("{\"a\":1}".into());
    m.thinking_text = Some("hmm".into());
    m.is_error = true;
    f.writer.send(WriteOp::InsertMessage(m.clone())).unwrap();
    f.add_message(id, 2, Role::User, "second");
    f.writer.flush().unwrap();

    let (s, msgs) = block_on(f.reader.load_session(id)).unwrap();
    assert_eq!(s.title, "Round trip");
    assert_eq!(s.provider_id, ProviderId::Claude);
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0], m_roundtrip(&m, &msgs[0]));
    assert_eq!(msgs[1].seq, 2);
    assert_eq!(block_on(f.reader.max_seq(id)).unwrap(), 2);

    let missing = block_on(f.reader.load_session(SessionId::new()));
    assert!(matches!(missing, Err(aw_store::StoreError::NotFound)));
}

/// Timestamps round-trip at microsecond precision; compare everything else exactly.
fn m_roundtrip(sent: &Message, got: &Message) -> Message {
    assert!((sent.created_at - got.created_at).abs() < Duration::milliseconds(1));
    Message {
        created_at: got.created_at,
        ..sent.clone()
    }
}

#[test]
fn update_session_and_usage_accumulate() {
    let f = Fixture::new();
    let id = f.add_session("Usage", 5);
    f.add_message(id, 1, Role::User, "x".repeat(300).as_str());
    f.writer
        .send(WriteOp::UpdateSession {
            id,
            title: None,
            provider_session_id: Some("prov-1".into()),
            model_id: Some("opus".into()),
            effort: Some(Effort::XHigh),
            auto_mode: Some(true),
            preview_snippet: None,
            usage: None,
        })
        .unwrap();
    let turn = |ctx, inp| UsageTotals {
        context_tokens_used: ctx,
        context_window_max: 200_000,
        input_tokens: inp,
        output_tokens: 2,
        cache_read_tokens: 3,
        cost_usd: 0.25,
    };
    for (ctx, inp) in [(10, 1), (40, 10)] {
        f.writer
            .send(WriteOp::ApplyTurnUsage {
                session_id: id,
                turn: turn(ctx, inp),
            })
            .unwrap();
    }
    f.writer.flush().unwrap();
    let (s, _) = block_on(f.reader.load_session(id)).unwrap();
    assert_eq!(s.provider_session_id.as_deref(), Some("prov-1"));
    assert_eq!(s.model_id, "opus");
    assert_eq!(s.effort, Effort::XHigh);
    assert!(s.auto_mode);
    assert_eq!(s.title, "Usage", "None fields are left unchanged");
    assert_eq!(s.preview_snippet.chars().count(), 200);
    assert_eq!(s.usage.input_tokens, 11);
    assert_eq!(s.usage.output_tokens, 4);
    assert_eq!(s.usage.cost_usd, 0.5);
    assert_eq!(s.usage.context_tokens_used, 40);
    assert!(s.updated_at > s.created_at);
}

#[test]
fn bad_ops_are_logged_not_fatal() {
    let f = Fixture::new();
    let id = f.add_session("Dup", 1);
    f.add_message(id, 1, Role::User, "one");
    f.add_message(id, 1, Role::User, "duplicate seq");
    f.add_message(SessionId::new(), 1, Role::User, "orphan");
    f.add_session("Still alive", 0);
    f.writer.flush().unwrap();
    let all = block_on(f.reader.list_sessions(None, 0, 24)).unwrap();
    assert_eq!(all.total, 2);
}

#[test]
fn writer_thread_stops_when_handles_drop() {
    let f = Fixture::new();
    let clone = f.writer.clone();
    drop(f.writer);
    clone.flush().unwrap();
}

#[test]
fn concurrent_writes_and_reads_never_hit_busy() {
    let f = Fixture::new();
    let id = f.add_session("Busy", 0);
    f.writer.flush().unwrap();

    let writer = f.writer.clone();
    let producer = thread::spawn(move || {
        for seq in 1..=1000 {
            writer
                .send(WriteOp::InsertMessage(message(
                    id,
                    seq,
                    Role::Assistant,
                    &format!("streamed block number {seq}"),
                )))
                .unwrap();
        }
        writer.flush().unwrap();
    });

    let mut errors = Vec::new();
    let mut loops = 0;
    block_on(async {
        while !producer.is_finished() || loops < 5 {
            loops += 1;
            if let Err(e) = f.reader.list_sessions(None, 0, 24).await {
                errors.push(e.to_string());
            }
            if let Err(e) = f.reader.search(None, "streamed".into(), 0, 24).await {
                errors.push(e.to_string());
            }
        }
    });
    producer.join().unwrap();
    assert!(errors.is_empty(), "reader errors: {errors:?}");
    assert_eq!(block_on(f.reader.max_seq(id)).unwrap(), 1000);
    let (_, msgs) = block_on(f.reader.load_session(id)).unwrap();
    assert_eq!(msgs.len(), 1000);
}
