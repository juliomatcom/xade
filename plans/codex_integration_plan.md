# Xade — Codex CLI Integration Plan (MVP)

Director: #opus-1 (plan, review, final fixes). Executor: #neon-1 (P1–P4). Branch: `feat/codex-provider` (shared; only #neon-1 commits until it hands back).
Goal: Codex is a third provider, additive, no breaking change: Claude and Antigravity behave exactly as today, existing DBs open unchanged.
Spirit: pragmatic and simple. Mirror `crates/antigravity` (same shape, same tests, same rules); rough edges listed in §8 are accepted for v1.

## 0. TL;DR of decisions
- Transport: `codex app-server` (JSON-RPC 2.0, NDJSON, stdio), one process per session. `codex exec --json` is rejected: no approval channel, no interrupt.
- New crate `crates/codex` = package `xade-codex`, copied in shape from `xade-antigravity` (`rpc.rs`, `translate.rs`, `process.rs`, `provider.rs`, `models.rs`). No shared crate refactor: copy `rpc.rs`, do not touch antigravity.
- `ProviderId::Codex`, stored as `"codex"`. Display name `Codex`.
- Auto OFF → `approvalPolicy:"untrusted"`; Auto ON → `"never"`; always `sandbox:"workspace-write"`. Sent on `thread/start`/`thread/resume` and on every `turn/start` (so a live toggle applies from the next turn).
- Effort → `turn/start.effort` (`Default` omits it). Model → `thread/start.model` and `turn/start.model`.
- Approvals for shell commands AND file edits (Codex gates both, unlike agy).
- File edits are translated to Claude-shaped `Edit`/`Write` tool events with a `structuredPatch`, so the existing diff modal works with zero UI change.
- No import, no running-sessions, no quota (rate limits need ChatGPT auth; API-key accounts get an error → empty). No logo asset yet (symbol fns return `None`).

## 1. Constraints (read `wiki-llm/conventions.md` first; these are the ones that bite here)
- Dependency rule becomes: xade-core ← xade-claude, xade-antigravity, xade-codex, xade-store ← xade-runtime ← xade. Runtime never names Codex (dev-dependency for tests only, like antigravity).
- Never read `~/.codex/auth.json` or any credential. Auth status only via `account/read`; the account label is display-only (never logged/persisted).
- Spawn `codex app-server` unmodified, env untouched, `cwd` = session dir, `kill_on_drop(true)` + `process_group(0)` + the same parent-death handling as antigravity `process.rs`.
- Unknown frames/fields/notifications are skipped (there are many: `hook/*`, `thread/status/changed`, `remoteControl/status/changed`, `warning`, `turn/diff/updated`, `serverRequest/resolved`, `thread/goal/cleared`…).
- Pin exact versions; reuse the exact versions antigravity's `Cargo.toml` pins. No new dependency needed.
- No `unwrap`/`expect` outside tests; `tracing` never logs prompt/message text above `debug`.

## 2. Verified protocol facts (codex-cli 0.172.0, gpt-6-luna, recorded 2026-11-27)
Fixtures: `crates/codex/fixtures/*.ndjson`, recorder `crates/codex/scripts/record.py <scenario|all> <sandbox-dir>` (costs API tokens; sandbox outside $HOME; deletes its threads). Line format identical to antigravity: `{"dir":"in"|"out"|"meta","frame":…}`, `meta {"restart":true}` between two processes (`resume`).
Full schema on demand: `codex app-server generate-json-schema --out <dir>`.

### 2.1 Process and handshake
- `codex app-server` with cwd; closing stdin exits 0. Responses have no `"jsonrpc"` field; notifications may carry a top-level `emittedAtMs`; treat both as optional.
- `initialize {clientInfo:{name:"xade",title:"Xade",version}}` → `{userAgent,codexHome,…}`; then notification `initialized` (no params). Required before anything else.
- `thread/start {cwd, model, approvalPolicy, sandbox}` → `result.thread.id` (UUIDv7) = `provider_session_id`; also `result.model`, `result.reasoningEffort`. Creates a persisted rollout immediately → create it lazily on the first prompt (like agy `session/new`).
- `thread/resume {threadId, cwd, model, approvalPolicy, sandbox, excludeTurns:true}` → same shape; keeps context (`resume`: recalls "walrus"). Unknown id → error `-32600 "no rollout found for thread id …"` (`bad_resume`) → `Error` event + treat as fatal for the spawn (same as agy bad_resume handling).
- `thread/delete {threadId}` → `{}` + notification `thread/deleted`. MUST wait for the response before closing stdin, or the server exits without deleting (verified).
- `model/list {}` → `data[]` of `{id, displayName, description, hidden, isDefault, supportedReasoningEfforts:[{reasoningEffort}], defaultReasoningEffort}` (`account`). Efforts seen: `low|medium|high|xhigh|max|ultra`.
- `account/read {}` → `{account: null | {type:"apiKey"} | {type:"chatgpt", email, planType}, requiresOpenaiAuth}` (`account`). `account == null && requiresOpenaiAuth` → not logged in.
- `account/rateLimits/read` with API key → `-32600 "chatgpt authentication required…"` (`account`).

### 2.2 A turn
- `turn/start {threadId, input:[{type:"text",text}], effort?, model?, approvalPolicy?}` → `result.turn.id` (store it: needed for steer/interrupt), `status:"inProgress"`.
- Then: `turn/started`, `item/started`/`item/completed` per item, deltas, `thread/tokenUsage/updated` (one per model call), `turn/completed {turn:{id,status,error}}`. `turn/completed` is THE end of turn.
- Item types (`params.item.type`): `userMessage` (ignore), `agentMessage` (`text`, `phase:"commentary"|"final_answer"`), `reasoning` (`summary[]`, `content[]`), `commandExecution` (`command`, `commandActions[0].command` = unwrapped cmd, `cwd`, `status`, `aggregatedOutput`, `exitCode`), `fileChange` (`changes[{path, kind:{type:"add"|"update"|"delete"}, diff}]`, `status`). Unknown types → skip.
- Deltas: `item/agentMessage/delta {itemId, delta}`; `item/reasoning/summaryTextDelta` and `item/reasoning/textDelta {delta}` (`thinking`, needs `summary:"concise"` on turn/start to appear — send it); `item/commandExecution/outputDelta` (ignore; use `aggregatedOutput` on completion).
- `fileChange` diff: `add` → the full new content (`edit`); `update` → unified hunks `"@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n"` (`edit_update`).
- Terminal item statuses: `completed`, `failed`, `declined` (`tool_deny`).
- Usage: `thread/tokenUsage/updated {tokenUsage:{total, last, modelContextWindow}}`, breakdown `{inputTokens, cachedInputTokens, cacheWriteInputTokens, outputTokens, reasoningOutputTokens, totalTokens}`. `total` is cumulative for the thread; `last` is the last model call.
- Model errors: an unknown model is NOT rejected by thread/start/turn/start; it yields notification `warning`, then `error {error:{message, codexErrorInfo}, willRetry:false}`, then `turn/completed status:"failed"` (`bad_model`). `error` with `willRetry:true` → ignore.

### 2.3 Approvals (codex → Xade requests, numeric `id` starting at 0)
- `item/commandExecution/requestApproval {threadId, turnId, itemId, command, cwd, reason?}` (`tool_allow`), `item/fileChange/requestApproval {threadId, turnId, itemId, reason?, grantRoot?}` (`edit`). Always preceded by the matching `item/started` (so the tool is already announced).
- Reply `{"id":<same id value>,"result":{"decision":"accept"|"decline"}}`. Accept → item completes `completed`; decline → item `declined`, turn continues and ends normally (`tool_deny`). Followed by `serverRequest/resolved` (ignore).
- Any other server request (`item/tool/requestUserInput`, `mcpServer/elicitation/request`, `item/permissions/requestApproval`, `item/tool/call`, legacy `applyPatchApproval`/`execCommandApproval`, `account/chatgptAuthTokens/refresh`, `attestation/generate`) → reply JSON-RPC error `{code:-32601,message:"unsupported"}` immediately so codex never blocks.
- Auto ON (`approvalPolicy:"never"`): no requests at all; commands run directly (`auto`).

### 2.4 Steer and interrupt
- Prompt while a turn runs → `turn/steer {threadId, expectedTurnId, input}` → `{turnId}` (same turn); one `turn/completed` ends both (`steer`). If steer returns an error (turn just ended), send it as a fresh `turn/start` instead.
- Interrupt → `turn/interrupt {threadId, turnId}` → `{}`, then `turn/completed status:"interrupted"` (`interrupt`). With an approval pending, just send `turn/interrupt` (no need to answer first; verified `interrupt_pending_permission`), then drop the pending request ids. Process stays usable.

## 3. Domain mapping (adapter contract → `xade_core`)
| Codex | ProviderEvent |
|---|---|
| `thread/start`/`resume` ok | `Started{provider_session_id: thread.id, model_id: result.model}` |
| `item/agentMessage/delta` | `TextDelta` (buffered) |
| `item/completed` agentMessage | flush buffer as `AssistantText{text}` (prefer `item.text` when the buffer is empty or differs: item.text is authoritative) |
| reasoning deltas | `ThinkingProgress{estimated_tokens: chars/4}` (flush text first); reset per turn |
| `item/started` commandExecution | `ToolUse{tool_use_id:item.id, name:"Bash", input_json:{"command": commandActions[0].command ?? command, "description": ""}}` |
| `item/completed` commandExecution | `ToolResult{output: aggregatedOutput ?? "", is_error: status!="completed" \|\| exitCode∉{0,null}, structured_json: None}` |
| `item/started` fileChange, 1 change, kind add | `ToolUse{name:"Write", input:{"file_path":path,"content":diff}}` |
| `item/started` fileChange otherwise | `ToolUse{name:"Edit", input:{"file_path":first path}}` (one ToolUse per item; multi-file: first path, all hunks) |
| `item/completed` fileChange | `ToolResult{output:"<path>: edited" per change, is_error: status!="completed", structured_json: Some({"structuredPatch":[{oldStart,newStart,lines}…]})}` parsed from the unified `@@ -a,b +c,d @@` hunks (lines keep their ` `/`-`/`+` prefix); for `add` use `{"type":"create","structuredPatch":[],"originalFile":null}` |
| approval request | flush text, `PermissionRequest{request_id: json id as string, tool_use_id:itemId, tool_name:"Bash"\|"Edit"\|"Write", input_json: same as its ToolUse}` |
| `thread/tokenUsage/updated` | store latest; no event |
| `error` (willRetry false) | `Error(message)` |
| `turn/completed` | flush text; `TurnComplete{usage, is_error: status=="failed", interrupted: status=="interrupted"}` |
| process exit | `Exited{code}` |

Usage for `TurnComplete`: `input_tokens = Δtotal.inputTokens − Δtotal.cachedInputTokens`, `output_tokens = Δtotal.outputTokens`, `cache_read_tokens = Δtotal.cachedInputTokens` (Δ = total at turn end − total at turn start; first turn after resume: baseline is the first `total` seen minus its `last`), `context_tokens_used = last.totalTokens`, `context_window_max = modelContextWindow ?? 0`, `cost_usd = 0`.
Check that `"Bash"`/`"Edit"`/`"Write"` render well in the chat (they are Claude's names; the diff modal keys on them in `apps/desktop/src/diff.rs::build_with`). If the Edit path needs `old_string`, it will not be reached because `structuredPatch` is non-empty.

## 4. Adapter design (`crates/codex`)
Copy the antigravity crate layout and adapt; keep files small.
- `rpc.rs`: line codec + id allocator + pending-request map (copy from antigravity). Codex request ids are integers; reply with the exact JSON value received.
- `translate.rs`: pure `Translator` (no I/O), state: text buffer, thinking chars, announced tools, usage baseline/latest, current turn id. Unit-tested per fixture (§6).
- `process.rs`: session task. Spawn → `initialize` → `initialized` → if `cfg.resume` then `thread/resume` immediately, else wait for the first prompt then `thread/start` → emit `Started` → `turn/start`. Commands:
  - `SendPrompt`: no turn running → `turn/start`; running → `turn/steer` (fallback `turn/start` on error); before the thread exists → queue.
  - `Interrupt`: no turn id yet → drop queued prompts and emit interrupted `TurnComplete`; else `turn/interrupt`, clear pending approvals.
  - `RespondPermission{request_id, allow}` → reply `accept`/`decline` to that id.
  - `SetAutoMode(b)`: store; used on the next `turn/start`; if `true`, also accept all pending approvals (same as agy).
  - `Shutdown`: close stdin, wait ≤ 5 s, kill.
- `models.rs`: parse `model/list` (non-hidden) → `ModelOption{id, label: displayName, description, effort_levels}` mapping `low|medium|high|xhigh|max` (drop `ultra`/unknown). Static fallback when preflight failed: `gpt-6-luna`.
- `provider.rs`: `CodexProvider: AgentProvider`.
  - `id` Codex, `display_name` "Codex".
  - Binary lookup: `$PATH`, `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin` (same helper style as agy; missing → `NotInstalled`).
  - `preflight`: one short `codex app-server` process: `initialize`, `initialized`, `account/read`, `model/list`, close stdin (after reading both responses), 10 s timeout. Cache models + default (`isDefault`) + account label (`chatgpt` → email; `apiKey` → "API key"). Not logged in → `NotLoggedIn`.
  - `models`/`default_model`/`effort_levels` from the cache.
  - `spawn` → `process.rs`.
  - `delete_session(id)`: short process, `thread/delete`, wait for the response (errors containing "no rollout" → Ok).
  - `import_sessions`, `running_sessions`, `quota`: defaults.

## 5. Phases (#neon-1 executes in order, one commit per phase, `cargo fmt` + `cargo clippy --workspace -- -D warnings` + `cargo test --workspace` green at every commit)

### P1 — Crate, Translator, fixture tests (not wired)
- Add `crates/codex` to workspace members; `Cargo.toml` mirrors antigravity's (version 0.5.1, same pinned deps).
- `rpc.rs`, `translate.rs`, `models.rs` + unit tests. A fixture replay test (`tests/fixtures.rs`) feeding every `out` frame of each fixture to the Translator and asserting the event sequence for: `simple_turn`, `two_turns`, `thinking`, `tool_allow`, `tool_deny`, `edit`, `edit_update`, `auto`, `interrupt`, `interrupt_pending_permission`, `steer`, `bad_model`. Assert the `structuredPatch` for `edit_update` equals `[{oldStart:1,newStart:1,lines:[" alpha","-beta","+BETA"," gamma"]}]`.
- Acceptance: crate builds, all translator tests pass, nothing else changed.

### P2 — Process + provider (crate complete, still not wired)
- `process.rs`, `provider.rs`.
- `tests/fake_codex.py` + `tests/fake.rs` modelled on antigravity's `fake_agy.py`/`fake.rs`: a scripted app-server that replays a fixture's `out` frames in reaction to the `in` frames. Cover: start+turn, approval allow/deny, interrupt, steer, resume, bad_resume, shutdown reaps the child, unsupported server request gets `-32601`.
- `tests/e2e.rs`: real-codex tests `#[ignore]`, gated by `XADE_E2E_CODEX=1`, model `gpt-6-luna`, effort low, sandbox in a tempdir outside $HOME, every thread deleted after. Keep it to 3 tests (turn, tool allow, resume) — they cost tokens.
- Acceptance: `cargo test -p xade-codex` green; `XADE_E2E_CODEX=1 cargo test -p xade-codex --test e2e -- --ignored --test-threads=1` green once.

### P3 — Wire into the app
- `xade-core`: `ProviderId::Codex`. Then fix every exhaustive match the compiler reports; known sites:
  - `crates/store/src/schema.rs` `provider_str`/`parse_provider`: `"codex"` (no migration: TEXT column).
  - `crates/runtime/src/handoff.rs` `provider_name`/`parse_provider` list: add Codex.
  - `crates/runtime/src/multi.rs` team roster label: `"Codex"`.
  - `apps/desktop/src/conversation.rs` `company_symbol`/`cli_symbol`: Codex → `None` (make the match return `Option<(&str,&[u8])>`; no asset in v1).
  - `apps/desktop/src/home.rs` `provider_color`: `theme::text_muted()`.
  - `apps/desktop/src/widgets/delete_modal.rs` history label: `"Codex history"`.
  - `apps/desktop/src/lib.rs` `backend`: add `Arc::new(CodexProvider::new())` after Antigravity; `apps/desktop/Cargo.toml` + `crates/runtime/Cargo.toml` (dev-dep) add `xade-codex`.
- Check the provider picker, status bar, quota bar and fork menu work with 3 providers (they iterate providers; fix any `[Claude, Antigravity]` hard-coded list outside tests). Add Codex to `App::StubProvider` (`StubProvider::codex()`) and extend the existing "both providers" tests where a 3rd provider changes behavior.
- Acceptance: `cargo test --workspace` and `cargo test -p xade --test e2e` green; manual: run `cargo run -p xade -- <dir>`, pick Codex, send a prompt, approve a command, see an edit in the diff modal, Stop mid-turn, close and reopen the session and continue it (resume), fork a Codex session to Claude and back, delete a Codex session (thread removed from `~/.codex/sessions`).

### P4 — Docs and wiki (prose: keep it short)
- `wiki-llm/codex-protocol.md`: §2 + §3 condensed in the style of `antigravity-protocol.md`, each fact naming its fixture.
- `wiki-llm/index.md`: a row for Codex; `architecture.md`: crate list + dependency rule; `conventions.md`: dependency rule + credentials line mentions codex; `testing.md`: recorder and `XADE_E2E_CODEX` lines.
- `docs/codex.md` (user-facing, like `docs/antigravity.md`): install/sign in with `codex login`, Auto OFF gates commands and file edits, Auto ON = never ask (workspace-write sandbox), no history import, no quota bar.
- `README.md`: name Codex next to Claude Code and Antigravity where providers are listed; link `docs/codex.md`. `docs/provider-protocols.md` already exists.
- Acceptance: links resolve; nothing restates code.

## 6. Corner cases (each needs a test or a manual-check line)
- Prompt sent before `thread/start` returns → queued, sent after.
- Two quick prompts → second becomes `turn/steer`; steer error → new `turn/start`.
- Stop before the thread exists / before `turn/start` returns → interrupted `TurnComplete`, no stray turn.
- Stop with an approval pending → `turn/interrupt`, pending dropped, the UI prompt disappears (same path as agy).
- Approval arriving after Stop → reply `decline` at once, never shown.
- Auto toggled ON with approvals pending → all accepted.
- `error` with `willRetry:true` → no event.
- Unknown item type / notification / server request → skipped / `-32601`.
- `turn/completed status:"failed"` without prior `error` → `Error("Codex turn failed")` + error `TurnComplete`.
- Process dies mid-turn → `Error` + `TurnComplete{is_error}` + `Exited`.
- Resume of a deleted thread → `Error`, session shows the error state (same as agy).
- Effort `Default` → `effort` omitted; a model whose efforts lack the chosen one → omit.
- Fork hand-off: the first prompt starts with `@<path>`; Codex does not expand `@` itself — verify manually that the model reads the file with a shell read (read-only commands are auto-approved under `untrusted`). If it does not, prepend nothing and note it in §8; no special code in v1.

## 7. Execution protocol
- #neon-1 owns every file in this plan except `plans/` and `docs/provider-protocols.md`. #opus-1 does not edit code until #neon-1 broadcasts "done".
- One commit per phase on `feat/codex-provider`, message `codex: P<n> <summary>`, ending with the repo's attribution line. Never push, never rebase, never touch `main`.
- Blocked or a fact in §2 proves wrong → re-record with `record.py`, fix the fact here in the same commit, continue. Ask #opus-1 via broadcast only for a real design decision.
- When P4 is committed, broadcast: commit hashes, test commands run and their results, manual checks done, and any deviation from this plan.
- Then #opus-1 runs a high-effort code review (Sonnet sub-agent) and fixes the findings.

## 8. Accepted rough edges for v1 (next iterations)
- No Codex/OpenAI logo; no theme colour.
- No quota bar (ChatGPT-plan rate limits via `account/rateLimits/read` could be added later).
- No history import of terminal Codex sessions (`thread/list` makes it possible later).
- Auto ON keeps the workspace-write sandbox (no network, no writes outside cwd), unlike Claude's full bypass.
- Multi-file `fileChange` items show as one `Edit` on the first path with all hunks.
- User Codex hooks (`~/.codex/hooks.json`) run as configured; their frames are ignored.
- Sub-agent threads (other `threadId`s) are not shown; their approvals are declined.
