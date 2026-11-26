//! What happens on screen. A solo session fixes a slow search while a three-model team adds rate
//! limiting to the same shop: the lead plans and hands off, two teammates code in parallel, and
//! their broadcasts bring the work back together. All names and code are made up.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use egui::{Key, Modifiers};
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use time::OffsetDateTime;
use xade_core::{
    AgentProvider, Effort, ImportedMessage, ImportedSession, MultiMemberSpec, ProviderId, Role,
    UsageTotals,
};

use crate::provider::{DemoProvider, Stage, FABLE, NEON, OPUS, SONNET};
use crate::stage::{Camera, FPS, SCALE, SIZE};

const SOLO_PROMPT: &str = "Product search takes 2s on big catalogs. Find out why and fix it.";
const TEAM_PROMPT: &str =
    "#opus-1: Add rate limiting to checkout. Plan it, then hand the UI and tests to the team.";

const CHECKOUT_RS: &str = r#"pub async fn checkout(cart: Cart) -> Result<Order, CheckoutError> {
    let order = Order::from(cart);
    payments::charge(&order).await?;
    Ok(order)
}
"#;

const RATE_LIMIT_RS: &str = r#"use std::collections::HashMap;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(60);
const LIMIT: usize = 5;

#[derive(Default)]
pub struct RateLimiter {
    hits: HashMap<CustomerId, Vec<Instant>>,
}

impl RateLimiter {
    /// `Err` holds how long the customer has to wait.
    pub fn check(&mut self, id: CustomerId, now: Instant) -> Result<(), Duration> {
        let hits = self.hits.entry(id).or_default();
        hits.retain(|t| now.duration_since(*t) < WINDOW);
        if hits.len() >= LIMIT {
            return Err(WINDOW - now.duration_since(hits[0]));
        }
        hits.push(now);
        Ok(())
    }
}
"#;

const BUTTON_TSX: &str = r#"export function CheckoutButton({ onPay }: Props) {
  const [wait, setWait] = useCountdown();

  async function pay() {
    const res = await onPay();
    if (res.status === 429) setWait(Number(res.headers.get("Retry-After")));
  }

  return (
    <button onClick={pay} disabled={wait > 0} aria-live="polite">
      {wait > 0 ? `Try again in ${wait}s` : "Pay now"}
    </button>
  );
}
"#;

const TESTS_RS: &str = r#"#[test]
fn sixth_request_in_a_minute_is_limited() {
    let mut limiter = RateLimiter::default();
    let now = Instant::now();
    for _ in 0..5 {
        assert!(limiter.check(CUSTOMER, now).is_ok());
    }
    assert_eq!(limiter.check(CUSTOMER, now), Err(Duration::from_secs(60)));
}

#[test]
fn the_window_slides() {
    let mut limiter = RateLimiter::default();
    let start = Instant::now();
    for _ in 0..5 {
        limiter.check(CUSTOMER, start).unwrap();
    }
    assert!(limiter.check(CUSTOMER, start + WINDOW).is_ok());
}
"#;

const MIGRATION_SQL: &str = r#"CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE INDEX CONCURRENTLY products_name_trgm
    ON products USING gin (name gin_trgm_ops);
"#;

const SEARCH_RS: &str = r#"pub async fn search(db: &Db, term: &str) -> Result<Vec<Product>> {
    db.query(
        "SELECT id, name, price FROM products
         WHERE name ILIKE $1 ORDER BY similarity(name, $2) DESC LIMIT 20",
        &[&format!("%{term}%"), &term],
    )
    .await
}
"#;

const OLD_SEARCH_RS: &str = r#"pub async fn search(db: &Db, term: &str) -> Result<Vec<Product>> {
    db.query(
        "SELECT * FROM products WHERE lower(name) LIKE $1",
        &[&format!("%{}%", term.to_lowercase())],
    )
    .await
}
"#;

pub fn record(out: &Path) {
    std::fs::create_dir_all(out).unwrap();
    isolate();
    // Under the target dir, not the system temp dir: imports skip temp dirs.
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("demo");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    let shop = shop(&root);

    let stage = Arc::new(Stage::default());
    let claude = history(&root, &shop);
    let providers: Vec<Arc<dyn AgentProvider>> = vec![
        Arc::new(DemoProvider {
            id: ProviderId::Claude,
            stage: stage.clone(),
            history: claude,
        }),
        Arc::new(DemoProvider {
            id: ProviderId::Antigravity,
            stage: stage.clone(),
            history: Vec::new(),
        }),
    ];
    let (_rt, launch) = xade::backend_with(&root.join("data"), providers).expect("backend");
    let workspace = shop.clone();
    let ui = Harness::builder()
        .with_size(SIZE)
        .with_pixels_per_point(SCALE)
        .with_step_dt(1.0 / FPS as f32)
        .with_max_steps(16)
        .with_accessibility_check(false)
        .wgpu()
        .build_eframe(|cc| launch.into_app(cc, Some(workspace)));
    let mut cam = Camera::new(ui, stage, shop);

    team_and_solo(&mut cam, out);
    home(&mut cam, out);
    let _ = std::fs::remove_dir_all(&root);
}

fn team_and_solo(cam: &mut Camera, out: &Path) {
    cam.run_until_ui("the workspace's session window", |c| {
        c.ui.query_by_label("🚀 Auto: OFF").is_some()
    });
    cam.run(30);

    // Before the camera rolls: the solo session is already at work.
    cam.click("🚀 Auto: OFF");
    cam.ui
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .click();
    cam.run(3);
    cam.type_text(SOLO_PROMPT, 200);
    cam.press(Modifiers::COMMAND, Key::Enter);
    cam.await_prompt(FABLE, 1);
    cam.ui.remove_cursor();
    let t = cam.now();
    let t = cam.think(t, FABLE, 15, 90);
    let t = cam.stream(
        t,
        FABLE,
        "I'll look at the query before touching anything.",
        3,
    );
    let solo = cam.say(
        t,
        FABLE,
        "I'll look at the query before touching anything.",
        None,
    );
    cam.run_until(solo + 4);

    cam.roll(&out.join("master.mp4"));
    solo_turn(cam, cam.now());

    // Pick the team.
    cam.run(2 * FPS);
    cam.press(Modifiers::COMMAND | Modifiers::SHIFT, Key::T);
    cam.run_until_ui("the New Multi-Session modal", |c| {
        c.ui.query_by_label("New Multi-Session").is_some()
    });
    let member = |provider_id, model: &str, effort| MultiMemberSpec {
        provider_id,
        model_id: model.into(),
        effort,
        auto_mode: true,
    };
    cam.ui
        .state_mut()
        .multi_create
        .as_mut()
        .expect("the modal's state")
        .rows = vec![member(ProviderId::Claude, OPUS, Effort::High)];
    cam.run(18);
    for (provider, model, effort) in [
        (ProviderId::Claude, SONNET, Effort::Medium),
        (ProviderId::Antigravity, NEON, Effort::High),
    ] {
        let state = cam.ui.state_mut().multi_create.as_mut().unwrap();
        state.rows.push(member(provider, model, effort));
        cam.run(18);
    }
    cam.still(&out.join("modal.png"));
    cam.run(FPS);
    cam.click("Start Multi-Session");
    cam.run_until_ui("the multi-session window", |c| {
        c.ui.query_by_label("Team").is_some()
    });
    cam.run(FPS / 2);

    // Brief the lead.
    cam.focus_chat("Team");
    cam.type_text(TEAM_PROMPT, 2);
    cam.run(FPS / 2);
    cam.press(Modifiers::COMMAND, Key::Enter);
    cam.await_prompt(OPUS, 1);
    lead_plans(cam);

    // The hand-off reaches both teammates at once.
    cam.await_prompt(SONNET, 1);
    cam.await_prompt(NEON, 1);
    let start = cam.now();

    let neon_intro = "On it. I'll read `Retry-After` and keep the button off until it expires.";
    let t = cam.think(start, NEON, 24, 210);
    let t = cam.stream(t, NEON, neon_intro, 2);
    let t = cam.say(t, NEON, neon_intro, None);
    let t = cam.edit(t + 6, NEON, "web/CheckoutButton.tsx", BUTTON_TSX, 70);
    let neon_done = "The button now counts down, **Try again in 42s**, then turns itself back on.\n\n\
        ```tsx\n<button disabled={wait > 0}>\n  {wait > 0 ? `Try again in ${wait}s` : \"Pay now\"}\n</button>\n```";
    let t = cam.think(t + 4, NEON, 20, 380);
    let t = cam.stream(t, NEON, neon_done, 2);
    let t = cam.say(t, NEON, neon_done, None);
    let neon_end = cam.done(t + 2, NEON, 31_000);

    let tests_intro = "I'll cover the limit, the wait time and the sliding window.";
    let t = cam.think(start + 8, SONNET, 36, 260);
    let t = cam.stream(t, SONNET, tests_intro, 2);
    let t = cam.say(t, SONNET, tests_intro, None);
    let read = serde_json::json!({ "file_path": "src/middleware/rate_limit.rs" }).to_string();
    let t = cam.tool(t + 4, SONNET, "Read", read, RATE_LIMIT_RS, 16);
    let t = cam.edit(t + 4, SONNET, "tests/rate_limit.rs", TESTS_RS, 45);
    let test = serde_json::json!({ "command": "cargo test rate_limit" }).to_string();
    let sonnet_tests = cam.tool(
        t + 4,
        SONNET,
        "Bash",
        test,
        "running 4 tests\n....\ntest result: ok. 4 passed; 0 failed",
        120,
    );
    let green = "Tests are green, **4 passed**. Request six gets `429` with `Retry-After: 60`.";
    let t = cam.think(sonnet_tests, SONNET, 15, 520);
    let t = cam.say(
        t,
        SONNET,
        "I added two tests in `tests/rate_limit.rs`.",
        Some(green),
    );
    let sonnet_end = cam.done(t + 2, SONNET, 28_000);

    // Watch both teammates at once, then look over one's shoulder while it codes.
    cam.run_until(start + 105);
    cam.still(&out.join("team.png"));
    cam.run_until(neon_end.max(sonnet_tests - 125));
    cam.tab("#sonnet-1");
    cam.run_until(sonnet_tests - 55);
    cam.still(&out.join("member.png"));
    cam.run_until(sonnet_tests - 10);
    cam.tab("Team");

    // The green light goes back to the lead, who wraps up.
    cam.await_prompt(OPUS, 2);
    cam.await_prompt(NEON, 2);
    let t = cam.now();
    cam.done(t + 8, NEON, 32_000);
    let summary = "Checkout is rate limited end to end:\n\n\
        | Piece | Owner |\n|---|---|\n\
        | Sliding-window limiter, `429` | #opus-1 |\n\
        | Countdown on the pay button | #neon-1 |\n\
        | 4 passing tests | #sonnet-1 |\n\n\
        Review the diff whenever you're ready.";
    let t = cam.think(t, OPUS, 28, 640);
    let t = cam.stream(t, OPUS, summary, 4);
    let t = cam.say(t, OPUS, summary, None);
    let end = cam.done(t + 2, OPUS, 44_000);
    cam.run_until(end.max(sonnet_end) + 15);
    cam.still(&out.join("team-done.png"));
    cam.run(4 * FPS);
    cam.cut();
}

fn lead_plans(cam: &mut Camera) {
    let intro = "I'll check how checkout is wired first.";
    let t = cam.think(cam.now(), OPUS, 30, 340);
    let t = cam.stream(t, OPUS, intro, 2);
    let t = cam.say(t, OPUS, intro, None);
    let read = serde_json::json!({ "file_path": "src/routes/checkout.rs" }).to_string();
    let t = cam.tool(t + 2, OPUS, "Read", read, CHECKOUT_RS, 14);
    let t = cam.edit(
        t + 2,
        OPUS,
        "src/middleware/rate_limit.rs",
        RATE_LIMIT_RS,
        26,
    );
    let plan =
        "The limiter is in: **5 requests a minute** per customer. Handing off the UI and tests.";
    let t = cam.think(t, OPUS, 12, 910);
    let t = cam.stream(t, OPUS, plan, 3);
    let contract =
        "**Contract:** over the limit, `POST /checkout` answers `429` with `Retry-After`.\n\n\
        ```rust\nErr(CheckoutError::RateLimited { retry_after })\n```\n\
        - #neon-1: show a countdown on the pay button\n\
        - #sonnet-1: test the limit and the wait time";
    let t = cam.say(t, OPUS, plan, Some(contract));
    cam.done(t + 2, OPUS, 38_000);
}

/// The solo session's work, paced to run alongside the whole team story.
fn solo_turn(cam: &mut Camera, start: u32) {
    let read = serde_json::json!({ "file_path": "src/search.rs" }).to_string();
    let t = cam.tool(start + 10, FABLE, "Read", read, OLD_SEARCH_RS, 30);
    let explain = serde_json::json!({
        "command": "psql -c \"EXPLAIN ANALYZE SELECT * FROM products WHERE lower(name) LIKE '%lamp%'\""
    })
    .to_string();
    let t = cam.tool(
        t + 10,
        FABLE,
        "Bash",
        explain,
        "Seq Scan on products  (actual time=0.031..1874.220 rows=312)",
        75,
    );
    let found = "Found it. `lower(name) LIKE '%…%'` can't use any index, so every search is a \
        **sequential scan** of 1.2M rows (1.9s). A trigram index fixes that, and `SELECT *` can go too.";
    let t = cam.think(t + 5, FABLE, 40, 450);
    let t = cam.stream(t, FABLE, found, 3);
    let t = cam.say(t, FABLE, found, None);
    let t = cam.edit(
        t + 20,
        FABLE,
        "migrations/0042_search_index.sql",
        MIGRATION_SQL,
        60,
    );
    let t = cam.edit(t + 30, FABLE, "src/search.rs", SEARCH_RS, 60);
    let again = serde_json::json!({ "command": "./scripts/bench-search.sh" }).to_string();
    let t = cam.tool(
        t + 30,
        FABLE,
        "Bash",
        again,
        "p95 before: 1874 ms\np95 after:    11 ms",
        110,
    );
    let fixed = "Search is down from **1.9s to 11ms** at p95:\n\n\
        - A GIN trigram index on `products.name`, built `CONCURRENTLY` so writes keep flowing.\n\
        - `ILIKE` replaces `lower(name) LIKE`, and results are ranked by similarity.\n\
        - Only the three columns the page shows are selected.";
    let t = cam.think(t + 10, FABLE, 30, 820);
    let t = cam.stream(t, FABLE, fixed, 4);
    let t = cam.say(t, FABLE, fixed, None);
    cam.done(t + 3, FABLE, 52_000);
}

/// The Home grid, with the team listed as one card among older sessions.
fn home(cam: &mut Camera, out: &Path) {
    cam.click("🏠 Home");
    cam.run(FPS);
    let all = cam
        .ui
        .get_all_by_label("All Workspaces")
        .last()
        .expect("the sidebar's All Workspaces");
    all.click();
    cam.run(3);
    cam.ui.remove_cursor();
    cam.run(2 * FPS);
    cam.still(&out.join("home.png"));
}

/// Keeps the app away from this machine: no real CLI, history or home folder.
fn isolate() {
    let home = std::env::temp_dir().join(format!("xade-demo-home-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("HOME", &home);
    std::env::set_var("PATH", "/usr/bin:/bin");
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=demo", "-c", "user.email=demo@example.com"])
        .args(args)
        .status()
        .expect("git is installed");
    assert!(status.success(), "git {args:?} failed");
}

/// The made-up project the sessions work in: a real git repository, so the status bar and the
/// change summaries are real.
fn shop(root: &Path) -> PathBuf {
    let dir = root.join("acme-store");
    for (name, text) in [
        ("README.md", "# Acme Store\n"),
        ("Cargo.toml", "[package]\nname = \"acme-store\"\n"),
        ("src/routes/checkout.rs", CHECKOUT_RS),
        ("src/search.rs", OLD_SEARCH_RS),
        ("tests/checkout.rs", "// checkout tests\n"),
        (
            "web/CheckoutButton.tsx",
            "export function CheckoutButton() {}\n",
        ),
        ("migrations/0041_orders.sql", "-- orders\n"),
        ("scripts/bench-search.sh", "#!/bin/sh\n"),
    ] {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "Initial commit"]);
    dir
}

/// Older sessions for the Home grid.
fn history(root: &Path, shop: &Path) -> Vec<ImportedSession> {
    let web = root.join("web-app");
    let payments = root.join("payments-api");
    std::fs::create_dir_all(&web).unwrap();
    std::fs::create_dir_all(&payments).unwrap();
    let session = |i: usize, cwd: &Path, title: &str, model: &str, hours: i64, reply: &str| {
        let at = OffsetDateTime::now_utc() - time::Duration::hours(hours);
        let message = |role, text: &str| ImportedMessage {
            role,
            content_text: text.into(),
            tool_name: None,
            tool_use_id: None,
            tool_payload_json: None,
            is_error: false,
            created_at: at,
        };
        ImportedSession {
            provider_session_id: format!("past-{i}"),
            cwd: cwd.into(),
            title: title.into(),
            model_id: model.into(),
            created_at: at,
            updated_at: at,
            usage: UsageTotals::default(),
            messages: vec![message(Role::User, title), message(Role::Assistant, reply)],
        }
    };
    let claude = vec![
        session(1, shop, "Fix token expiry bug", OPUS, 1, "`verify()` never checked the `exp` claim. It now rejects expired tokens, and a new test covers an expired token and a valid one."),
        session(2, &web, "Why is the build slow on CI?", SONNET, 3, "The cache key ignored the lockfile, so every run reinstalled dependencies. Keying on the lockfile hash cut the install step from 94s to 6s."),
        session(3, &payments, "Rename Account to Workspace", OPUS, 26, "Renamed the type, its table and 14 call sites. The migration keeps the old name as a view for one release so rollbacks stay safe."),
        session(4, &web, "Flaky e2e test on Safari", SONNET, 50, "The test raced the animation. Waiting for the transition end event instead of a fixed timeout made it pass 200 runs in a row."),
        session(5, shop, "Add request logging middleware", FABLE, 5, "Added a middleware that logs each request's method, path, status and duration, and wired it in where the router is built."),
        session(6, &web, "Audit accessibility of the nav", SONNET, 28, "Contrast passes. The menu button had no accessible name and focus was lost when the drawer closed. Both are fixed."),
        session(7, &payments, "Write release notes for v2.3", FABLE, 8, "Grouped the changes into Added, Changed and Fixed, with the user-facing items first, and linked each entry to its pull request."),
    ];
    claude
}
