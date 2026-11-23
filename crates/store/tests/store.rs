#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::path::PathBuf;
use std::thread;

use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;
use xade_core::{
    Effort, Message, ProviderId, QuotaKind, QuotaWindow, Role, Session, SessionId, SessionOrigin,
    UsageTotals, Workspace, WorkspaceId,
};
use xade_store::{StoreReader, StoreWriter, WriteOp, WriterHandle, SCHEMA_VERSION};

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

    fn search(
        &self,
        scope: Option<WorkspaceId>,
        q: &str,
    ) -> xade_store::Page<xade_store::SessionCard> {
        self.writer.flush().unwrap();
        block_on(self.reader.search(scope, None, q.to_string(), 0, 24)).unwrap()
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
        origin: SessionOrigin::Xade,
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
    let before = block_on(f.reader.list_sessions(None, None, 0, 24)).unwrap();

    for _ in 0..2 {
        let again = StoreWriter::spawn(&f.path).unwrap();
        again.flush().unwrap();
        assert_eq!(user_version(&f.path), SCHEMA_VERSION);
    }
    let after = block_on(f.reader.list_sessions(None, None, 0, 24)).unwrap();
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
    let p0 = block_on(f.reader.list_sessions(Some(f.ws), None, 0, 24)).unwrap();
    let p1 = block_on(f.reader.list_sessions(Some(f.ws), None, 1, 24)).unwrap();
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
    let all = block_on(f.reader.list_sessions(None, None, 0, 24)).unwrap();
    let titles: Vec<_> = all.items.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["new", "elsewhere", "old"]);
    let scoped = block_on(f.reader.list_sessions(Some(f.ws), None, 0, 24)).unwrap();
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
    let p1 = block_on(f.reader.search(None, None, "deploy".into(), 1, 24)).unwrap();
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
    assert!(matches!(missing, Err(xade_store::StoreError::NotFound)));
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
    let all = block_on(f.reader.list_sessions(None, None, 0, 24)).unwrap();
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
            if let Err(e) = f.reader.list_sessions(None, None, 0, 24).await {
                errors.push(e.to_string());
            }
            if let Err(e) = f.reader.search(None, None, "streamed".into(), 0, 24).await {
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

fn import_op(
    f: &Fixture,
    psid: &str,
    title: &str,
    texts: &[&str],
    age: i64,
) -> (SessionId, WriteOp) {
    let mut s = session(WorkspaceId::new(), title, f.base - Duration::seconds(age));
    s.provider_session_id = Some(psid.into());
    s.origin = SessionOrigin::ClaudeCode;
    s.usage.input_tokens = 7;
    let messages = texts
        .iter()
        .enumerate()
        .map(|(i, t)| message(s.id, i as i64 + 1, Role::User, t))
        .collect();
    let id = s.id;
    (
        id,
        WriteOp::ImportSession {
            session: s,
            messages,
            workspace_root: "/tmp/imported".into(),
        },
    )
}

#[test]
fn migration_v2_adds_origin_quota_and_unique_provider_session() {
    let f = Fixture::new();
    let a = f.add_session("a", 0);
    f.writer.flush().unwrap();
    let (s, _) = block_on(f.reader.load_session(a)).unwrap();
    assert_eq!(s.origin, SessionOrigin::Xade);
    let mut dup = session(f.ws, "dup1", f.base);
    dup.provider_session_id = Some("same".into());
    let mut dup2 = session(f.ws, "dup2", f.base);
    dup2.provider_session_id = Some("same".into());
    f.writer.send(WriteOp::InsertSession(dup)).unwrap();
    f.writer.send(WriteOp::InsertSession(dup2)).unwrap();
    f.writer.flush().unwrap();
    // The second insert violates the unique index and is dropped (logged, not fatal).
    assert_eq!(
        block_on(f.reader.list_sessions(None, None, 0, 24))
            .unwrap()
            .total,
        2
    );
}

#[test]
fn origin_filter_applies_to_list_and_search() {
    let f = Fixture::new();
    f.add_session("deploy from xade", 5);
    let (_, op) = import_op(&f, "p1", "deploy from claude code", &["hello"], 1);
    f.writer.send(op).unwrap();
    f.writer.flush().unwrap();
    let list = |o| block_on(f.reader.list_sessions(None, o, 0, 24)).unwrap();
    assert_eq!(list(None).total, 2);
    let cc = list(Some(SessionOrigin::ClaudeCode));
    assert_eq!(cc.items.len(), 1);
    assert_eq!(cc.items[0].origin, SessionOrigin::ClaudeCode);
    assert_eq!(
        list(Some(SessionOrigin::Xade)).items[0].title,
        "deploy from xade"
    );
    for q in ["deploy", "de", "hello"] {
        let search = |o| block_on(f.reader.search(None, o, q.into(), 0, 24)).unwrap();
        assert!(search(None).total >= 1, "{q}");
        assert!(search(Some(SessionOrigin::Xade))
            .items
            .iter()
            .all(|c| c.origin == SessionOrigin::Xade));
        assert!(search(Some(SessionOrigin::ClaudeCode))
            .items
            .iter()
            .all(|c| c.origin == SessionOrigin::ClaudeCode));
    }
    let hello = block_on(
        f.reader
            .search(None, Some(SessionOrigin::Xade), "hello".into(), 0, 24),
    )
    .unwrap();
    assert_eq!(hello.total, 0);
}

#[test]
fn quota_upsert_keeps_last_window_per_kind() {
    let f = Fixture::new();
    assert!(block_on(f.reader.load_quota()).unwrap().is_empty());
    let reset = f.base + Duration::hours(2);
    let q = |kind, pct, resets_at| {
        WriteOp::UpsertQuota(QuotaWindow {
            kind,
            utilization_pct: pct,
            resets_at,
        })
    };
    f.writer
        .send(q(QuotaKind::FiveHour, 10.0, Some(reset)))
        .unwrap();
    f.writer.send(q(QuotaKind::SevenDay, 20.0, None)).unwrap();
    f.writer
        .send(q(QuotaKind::FiveHour, 55.5, Some(reset)))
        .unwrap();
    f.writer.flush().unwrap();
    let got = block_on(f.reader.load_quota()).unwrap();
    assert_eq!(got.len(), 2);
    let five = got.iter().find(|w| w.kind == QuotaKind::FiveHour).unwrap();
    assert_eq!(five.utilization_pct, 55.5);
    assert_eq!(
        five.resets_at.unwrap().unix_timestamp(),
        reset.unix_timestamp()
    );
    let seven = got.iter().find(|w| w.kind == QuotaKind::SevenDay).unwrap();
    assert_eq!((seven.utilization_pct, seven.resets_at), (20.0, None));
}

#[test]
fn import_inserts_then_replaces_messages_and_search_rows() {
    let f = Fixture::new();
    let (id, op) = import_op(
        &f,
        "p1",
        "first title",
        &["alpha question", "beta question"],
        10,
    );
    f.writer.send(op).unwrap();
    f.writer.flush().unwrap();
    let (s, msgs) = block_on(f.reader.load_session(id)).unwrap();
    assert_eq!(
        (s.origin, s.usage.input_tokens),
        (SessionOrigin::ClaudeCode, 7)
    );
    assert_eq!(msgs.len(), 2);
    assert_eq!(s.preview_snippet, "beta question");
    let ws = block_on(f.reader.list_workspaces()).unwrap();
    let imported = ws
        .iter()
        .find(|w| w.root_path.ends_with("imported"))
        .unwrap();
    assert_eq!(
        (imported.display_name.as_str(), s.workspace_id),
        ("imported", imported.id)
    );
    assert_eq!(f.titles("alpha question"), ["first title"]);

    // Re-import with a new id in the op: the stored id is kept, messages are replaced.
    let (_, op) = import_op(&f, "p1", "second title", &["gamma question"], 1);
    if let WriteOp::ImportSession { session, .. } = &op {
        assert_ne!(session.id, id);
    }
    f.writer.send(op).unwrap();
    f.writer.flush().unwrap();
    let (s, msgs) = block_on(f.reader.load_session(id)).unwrap();
    assert_eq!(s.title, "second title");
    assert_eq!(msgs.len(), 1);
    assert_eq!(
        block_on(f.reader.list_sessions(None, None, 0, 24))
            .unwrap()
            .total,
        1
    );
    assert_eq!(f.titles("gamma question"), ["second title"]);
    assert!(
        f.titles("alpha question").is_empty(),
        "stale search rows were removed"
    );
    let ws = block_on(f.reader.list_workspaces()).unwrap();
    assert_eq!(
        ws.iter()
            .filter(|w| w.root_path.ends_with("imported"))
            .count(),
        1
    );
}

#[test]
fn import_never_touches_xade_origin_and_keeps_local_fields() {
    let f = Fixture::new();
    let mut own = session(f.ws, "mine", f.base);
    own.provider_session_id = Some("shared".into());
    let own_id = own.id;
    f.writer.send(WriteOp::InsertSession(own)).unwrap();
    let (_, op) = import_op(&f, "shared", "overwrite attempt", &["x"], 0);
    f.writer.send(op).unwrap();
    f.writer.flush().unwrap();
    let (s, msgs) = block_on(f.reader.load_session(own_id)).unwrap();
    assert_eq!(
        (s.title.as_str(), s.origin, msgs.len()),
        ("mine", SessionOrigin::Xade, 0)
    );
    assert_eq!(
        block_on(f.reader.list_sessions(None, None, 0, 24))
            .unwrap()
            .total,
        1
    );

    // Imported rows keep effort, cost and a known context max across updates.
    let (id, op) = import_op(&f, "other", "t", &["x"], 5);
    f.writer.send(op).unwrap();
    f.writer
        .send(WriteOp::ApplyTurnUsage {
            session_id: id,
            turn: UsageTotals {
                cost_usd: 1.5,
                context_window_max: 1000,
                ..Default::default()
            },
        })
        .unwrap();
    f.writer
        .send(WriteOp::UpdateSession {
            id,
            title: None,
            provider_session_id: None,
            model_id: None,
            effort: Some(Effort::High),
            auto_mode: None,
            preview_snippet: None,
            usage: None,
        })
        .unwrap();
    let (_, op) = import_op(&f, "other", "t2", &["y"], 0);
    f.writer.send(op).unwrap();
    f.writer.flush().unwrap();
    let (s, _) = block_on(f.reader.load_session(id)).unwrap();
    assert_eq!(
        (
            s.title.as_str(),
            s.effort,
            s.usage.cost_usd,
            s.usage.context_window_max
        ),
        ("t2", Effort::High, 1.5, 1000)
    );
    let idx = block_on(f.reader.import_index(ProviderId::Claude)).unwrap();
    assert_eq!(idx["shared"].id, own_id);
    assert_eq!(idx["shared"].origin, SessionOrigin::Xade);
    assert_eq!(idx["other"].origin, SessionOrigin::ClaudeCode);
}

#[test]
fn prune_removes_throwaway_imports_and_empty_workspaces_only() {
    let f = Fixture::new();
    let real = env!("CARGO_MANIFEST_DIR");
    let mut keep_ws = workspace(WorkspaceId::new(), real);
    keep_ws.display_name = "real".into();
    let keep_id = keep_ws.id;
    f.writer.send(WriteOp::UpsertWorkspace(keep_ws)).unwrap();
    // A user-added workspace that exists outside temp and has no sessions stays.
    let user_ws = workspace(WorkspaceId::new(), &format!("{real}/src"));
    f.writer.send(WriteOp::UpsertWorkspace(user_ws)).unwrap();
    // An empty workspace in temp, and one that no longer exists, both go.
    f.writer
        .send(WriteOp::UpsertWorkspace(workspace(
            WorkspaceId::new(),
            "/tmp/empty-ws",
        )))
        .unwrap();
    f.writer
        .send(WriteOp::UpsertWorkspace(workspace(
            WorkspaceId::new(),
            "/definitely/not/here",
        )))
        .unwrap();
    // Imported session in temp (pruned), imported session in a real dir (kept),
    // Xade-origin session in temp (kept, so its workspace stays).
    let (junk_id, junk_op) = import_op(&f, "junk", "junk title", &["junk body"], 10);
    f.writer.send(junk_op).unwrap();
    let mut good = session(keep_id, "good title", f.base);
    good.provider_session_id = Some("good".into());
    good.origin = SessionOrigin::ClaudeCode;
    let good_id = good.id;
    f.writer.send(WriteOp::InsertSession(good)).unwrap();
    let own_id = f.add_session("own title", 5);
    f.add_message(junk_id, 2, Role::User, "junk body two");
    f.writer.flush().unwrap();
    assert_eq!(f.titles("junk").len(), 1);

    let (tx, rx) = std::sync::mpsc::channel();
    f.writer.send(WriteOp::PruneThrowaway(tx)).unwrap();
    f.writer.flush().unwrap();
    let counts = rx.recv().unwrap();
    assert_eq!(
        (counts.sessions, counts.workspaces),
        (1, 3),
        "junk session; its workspace, the empty temp one and the missing one"
    );
    assert!(f.titles("junk").is_empty(), "search rows are gone");
    assert!(block_on(f.reader.load_session(junk_id)).is_err());
    assert!(block_on(f.reader.load_session(good_id)).is_ok());
    assert!(block_on(f.reader.load_session(own_id)).is_ok());
    let roots: Vec<String> = block_on(f.reader.list_workspaces())
        .unwrap()
        .into_iter()
        .map(|w| w.root_path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(roots.len(), 3, "{roots:?}");
    assert!(roots.contains(&"/tmp/project".to_string()));
    assert!(roots.contains(&real.to_string()));
    assert!(roots.contains(&format!("{real}/src")));

    // Second run is a no-op.
    let (tx, rx) = std::sync::mpsc::channel();
    f.writer.send(WriteOp::PruneThrowaway(tx)).unwrap();
    f.writer.flush().unwrap();
    assert_eq!(rx.recv().unwrap(), xade_store::PruneCounts::default());
}

#[test]
fn a_dead_writer_thread_is_reported_once() {
    let f = Fixture::new();
    assert!(f.writer.take_failure().is_none(), "healthy writer");
    f.writer.send(WriteOp::Poison).unwrap();
    let mut reason = None;
    for _ in 0..200 {
        reason = f.writer.take_failure();
        if reason.is_some() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(25));
    }
    let reason = reason.expect("writer death reported");
    assert!(reason.contains("forced store writer failure"), "{reason}");
    assert!(f.writer.take_failure().is_none(), "reported once");
    assert!(f.writer.send(WriteOp::TouchWorkspace(f.ws)).is_err());
    assert!(f.writer.flush().is_err());
}
