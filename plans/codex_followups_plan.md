# Codex follow-ups: logo, quota, history import

Director: #opus-1. Executor: #neon-1. Branch: `feat/codex-provider` (continue on it; base `8d026c4`). Same rules as `plans/codex_integration_plan.md` §1 and §7: one commit per phase (`codex: F<n> …`), fmt, clippy and `cargo test --workspace` green, plus `cargo test -p xade --test e2e` before each commit. No breaking change: existing DBs open unchanged; Claude and Antigravity behave as today.

## F1 — Logo
- Facts: no icon ships with the codex CLI. Existing marks live in `assets/*-symbol.{svg,png}` (80×80 RGBA PNG, rasterized from the SVG) and are loaded by `apps/desktop/src/conversation.rs::{company_symbol, cli_symbol}` (today Codex → `None`).
- Source the OpenAI mark SVG from Simple Icons via `https://cdn.jsdelivr.net/npm/simple-icons@<exact version>/icons/openai.svg` (pick a released version, note it in the commit message). Save `assets/openai-symbol.svg`, rasterize with `rsvg-convert -w 80 -h 80` to `assets/openai-symbol.png`.
- Use it for both `company_symbol` and `cli_symbol` (no distinct Codex mark exists).
- The mark is monochrome black: it must be visible on light and dark themes. Pick the simplest of: (a) a white-on-transparent PNG tinted at draw time with a `theme.rs` token, or (b) a light badge/circle behind it in the PNG. Check how the other symbols are drawn before choosing; keep the code change minimal.
- Update the `[ProviderId::Claude, ProviderId::Antigravity]` symbol test in `conversation.rs` to include Codex.
- Acceptance: unit test that both symbols load for Codex; manual: avatar visible in a Codex session window in light and dark themes.

## F2 — Quota bar
- Protocol (schema-verified, NOT live-verified: the user's account is an API key, which returns `-32600 "chatgpt authentication required to read rate limits"`, fixture `account`): `account/rateLimits/read {}` → `{rateLimits: RateLimitSnapshot, rateLimitsByLimitId?: {<id>: RateLimitSnapshot}}`; `RateLimitSnapshot {limitId?, limitName?, primary?: RateLimitWindow, secondary?: RateLimitWindow, …}`; `RateLimitWindow {usedPercent: int, windowDurationMins?: int, resetsAt?: int (unix s)}`. Full schema: `codex app-server generate-json-schema --out <dir>`.
- `CodexProvider::quota()` (+ `AgentProvider::quota` impl): one short `codex app-server` (reuse the preflight `ShortServer`), `account/rateLimits/read`, wait for the response, close stdin.
  - Any error (API key, timeout, not logged in) → `Ok(vec![])` (log at `debug`); never an `Err` that would show a banner for API-key users.
  - Map each snapshot to one `QuotaGroup` (`name`: `limitName` or "" for the main one; prefer `rateLimitsByLimitId` when present, else `rateLimits`). Window kind from `windowDurationMins`: 300 → 5-hour kind, 10080 → weekly kind (use the existing `QuotaKind` variants; skip windows that match none). `utilization_pct = usedPercent`, `resets_at` from `resetsAt`.
- Check that the top bar already calls `quota()` for every available provider (it does for Antigravity); fix only if Codex is excluded.
- Tests: pure parser unit tests on hand-written samples built from the schema (5h+weekly, by-limit-id, missing windows, unknown duration). One fake-server test for the error → empty path, using `account.ndjson` frames.
- Wiki: one line in `codex-protocol.md` marking the shape as schema-only (unverified live).

## F3 — Import past Codex sessions
- Verified live (read-only probe, no tokens): `thread/list {limit, cursor?, sourceKinds?}` → `{data: Thread[], nextCursor}`; `Thread {id, createdAt, updatedAt (unix s), cwd, model, name, preview, source, originator, parentThreadId, ephemeral, turns: []}`. Threads created by Xade have **`originator: "xade"`** (from our `initialize.clientInfo.name`); the user's own terminal threads have `originator: "codex-tui"`. `thread/read {threadId, includeTurns:true}` → `thread.turns[] {id, status, items[], startedAt, completedAt}`, items are the same `ThreadItem`s as live (`userMessage`, `agentMessage`, `reasoning`, `commandExecution`, `fileChange`, …).
- Record a fixture first: extend `crates/codex/scripts/record.py` with an `import` scenario (create a thread with one tool-allow turn, then `thread/list` + `thread/read includeTurns:true` in a new process, then delete it). Costs one short turn.
- `CodexProvider::import_sessions()`: one short app-server process; page `thread/list` (`limit:100`, follow `nextCursor`, `sourceKinds: ["cli","vscode","exec","appServer"]` to exclude sub-agent threads); skip `ephemeral`, `parentThreadId != null`, and any `originator` starting with `"xade"`; `thread/read includeTurns:true` for each kept thread (use the same process; bound total time, e.g. 60 s, and return what was read so far on timeout).
- Map to `ImportedSession`: `provider_session_id` = id, `cwd`, `title` = `name` else `Session::derive_title(preview)`, `model_id` = `model` or "", timestamps from `createdAt`/`updatedAt`, `usage` default. Messages from items in order: `userMessage` → User text (join text parts); `agentMessage` → Assistant text; `commandExecution`/`fileChange` → the same tool-use + tool-result message pair Claude's importer produces, reusing the `Translator`'s `Bash`/`Edit`/`Write` input and `structuredPatch` shaping so the diff modal works on imported edits; skip `reasoning` and unknown items. Item timestamps: `startedAtMs`/`completedAtMs`, else the turn's.
- Origin, the only cross-crate change. Today imported rows are hard-coded `SessionOrigin::ClaudeCode`:
  - `xade-core`: add `SessionOrigin::Codex`; store string `"codex"` in `crates/store/src/schema.rs` `origin_str`/parse (TEXT column, no migration).
  - `crates/runtime/src/manager.rs::imported_rows`: origin by provider (Claude → `ClaudeCode`, Codex → `Codex`, Antigravity → `ClaudeCode` unreachable: it has no import).
  - `crates/store/src/writer.rs`: the `ImportSession` insert uses `session.origin` instead of the hard-coded `ClaudeCode`; `prune_throwaway` selects every non-Xade origin (`origin != 'xade'`).
  - UI: `home.rs::origin_label`/`origin_badge` (`"Codex"`, `theme::text_muted()`), the origin filter list, and `workspace.rs` tests listing origins.
- The existing manager rules then apply unchanged: Xade-origin, live and not-newer sessions are skipped; fork throwaways are skipped by the psid set and the forks-dir check.
- Tests: translator/import mapping unit tests from the new `import` fixture; a fake-server test for paging + filters (xade originator, sub-agent, ephemeral skipped); a manager test that a Codex import gets `SessionOrigin::Codex` and a Claude import still gets `ClaudeCode`; store round-trip of `"codex"`.
- Acceptance: `cargo test --workspace`, app e2e green. Live check (free, read-only): run `import_sessions()` once against the real CLI through an `#[ignore]` e2e test gated by `XADE_E2E_CODEX=1`, asserting no `originator: "xade"` thread is returned. The user currently has exactly one own thread (`codex-tui`, "Respond to greeting") — it should import.

## F4 — Docs
- `docs/codex.md`: replace "no history import" and "no quota bar" lines (quota shows only for ChatGPT sign-in; API-key accounts have none). `wiki-llm/codex-protocol.md`: import and quota facts, one line each with fixture or "schema-only". `wiki-llm/architecture.md` Import line: "only Claude and Codex import". Remove the three items from `plans/codex_integration_plan.md` §8.

## Hand-back
Broadcast to #opus-1: commit hashes, tests run with results, live checks done, deviations. #opus-1 then reviews (Sonnet sub-agent, high effort) and fixes.
