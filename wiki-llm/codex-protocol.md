# Codex CLI protocol (app-server)
Verified against `codex-cli` 0.172.0 (`gpt-6-luna`) with fixtures in `crates/codex/fixtures/` (re-record with `crates/codex/scripts/record.py <scenario> <sandbox outside $HOME>` after a CLI upgrade; costs API tokens). Fixture lines are `{"dir": "in"|"out"|"meta", "frame": …}`; `meta {"restart":true}` marks a process restart (`resume`). Integration is `codex app-server` (JSON-RPC 2.0, NDJSON on stdio); `codex exec --json` has no approval channel or interrupt and is not used.

## Spawn
- `codex app-server` with `cwd` = session directory, no other flags, environment untouched; child leads its own process group (`process_group(0)`, `kill_on_drop(true)`).
- Closing stdin exits 0. Responses omit `"jsonrpc"`; notifications may carry top-level `emittedAtMs`; treat both as optional.
- Binary lookup: `$PATH`, `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`.

## Handshake and preflight
- `initialize {clientInfo:{name:"xade",title:"Xade",version}}` → `{userAgent,codexHome,…}`, followed by notification `initialized` (no `params`); required before any other request (`simple_turn`).
- `thread/start {cwd, model?, approvalPolicy, sandbox:"workspace-write"}` → `result.thread.id` (UUIDv7) + `result.model`; creates a persisted rollout immediately, so fresh sessions call it lazily on the first prompt (`simple_turn`).
- `thread/resume {threadId, cwd, model?, approvalPolicy, sandbox:"workspace-write", excludeTurns:true}` runs immediately after `initialized` on resume and keeps context (`resume`); unknown id returns `-32600 "no rollout found for thread id …"` (`bad_resume`), fatal for that spawn.
- `thread/delete {threadId}` → `{}` + `thread/deleted`; MUST await the response before closing stdin or codex exits without deleting. Errors containing `"no rollout"` count as already deleted.
- Preflight runs one short `codex app-server` process (10 s timeout): `initialize`, `initialized`, `account/read {}`, `model/list {}` (`account`). `account: null` with `requiresOpenaiAuth: true` → `NotLoggedIn`; `chatgpt` → display-only `email`, `apiKey` → `"API key"`. `model/list` filters `hidden`, maps `low|medium|high|xhigh|max` (drops `ultra`), picks `isDefault`; fallback model `gpt-6-luna`.
- `account/rateLimits/read {}` returns `-32600` on API-key accounts (`account`, treated as empty `Ok(vec![])`); on ChatGPT accounts the response shape `{rateLimits, rateLimitsByLimitId}` with `primary`/`secondary` windows (`windowDurationMins` 300 = 5h, 10080 = weekly, `usedPercent`, `resetsAt` unix s) is schema-only (unverified live).
- History import runs one short app-server (60 s budget): pages `thread/list {limit:100, cursor?, sourceKinds:["cli","vscode","exec","appServer"]}`, skips `ephemeral`, `parentThreadId != null` and `originator` starting with `"xade"`, then `thread/read {threadId, includeTurns:true}` (`import`).

## Turn, approvals, steer, interrupt
- `turn/start {threadId, input:[{type:"text",text}], approvalPolicy, summary:"concise", model?, effort?}` → `result.turn.id` (needed for steer/interrupt); `effort` is omitted for `Default` or when the model lacks that level (`simple_turn`, `thinking`). Auto OFF sends `approvalPolicy:"untrusted"`, Auto ON sends `"never"` (`auto`), on both thread and every turn so live toggles apply from the next turn.
- Streaming notifications: `turn/started`, `item/started`, `item/agentMessage/delta`, `item/reasoning/summaryTextDelta` / `item/reasoning/textDelta` (`thinking`), `item/completed`, `thread/tokenUsage/updated`, `error`, `turn/completed` (THE end of turn). Unknown notifications (`hook/*`, `thread/status/changed`, `warning`, `turn/diff/updated`, `serverRequest/resolved`, `thread/goal/cleared`, `item/commandExecution/outputDelta`, …) and unknown item types are skipped.
- Approvals (server → Xade requests, numeric `id` starting at 0): `item/commandExecution/requestApproval` (`tool_allow`) and `item/fileChange/requestApproval` (`edit`, `edit_update`), always preceded by `item/started`. Reply `{"id":<same id>,"result":{"decision":"accept"|"decline"}}`; declined items finish with `status:"declined"` and the turn continues (`tool_deny`). Any other server request is answered `-32601 "unsupported"` immediately so codex never blocks.
- Only the session's own `threadId` is handled: other threads' notifications are dropped and their approvals declined (sub-agent threads are not shown in v1).
- Steer: a prompt while a turn runs sends `turn/steer {threadId, expectedTurnId, input}` → `{turnId}`; one `turn/completed` ends both (`steer`). On steer error (turn just ended), fall back to `turn/start`.
- Interrupt: `turn/interrupt {threadId, turnId}` → `{}` then `turn/completed status:"interrupted"` (`interrupt`). With an approval pending, send `turn/interrupt` directly and drop pending ids (`interrupt_pending_permission`). Approvals arriving after Stop are declined immediately and never shown. A prompt sent after Stop waits for that `turn/completed`, then starts a new turn (never steers).
- Model errors: unknown models are accepted by `turn/start`, emit `warning` then `error {willRetry:false}` (`bad_model`); `willRetry:true` is ignored.

## Frame → event (`crates/codex/src/translate.rs`)
- `thread/start`/`resume` ok → `Started{provider_session_id: thread.id, model_id: result.model}`.
- `item/agentMessage/delta` → `TextDelta` (buffered); `item/completed` `agentMessage` → `AssistantText` (preferring authoritative `item.text`); starting a new item or ending the turn flushes any unfinished buffer (`steer`, `interrupt`).
- Reasoning deltas → flush text, `ThinkingProgress{chars/4}`, reset per turn (`thinking`).
- `commandExecution` → `ToolUse` `"Bash"` (`{"command": commandActions[0].command ?? command, "description": ""}`) and `ToolResult` (`output: aggregatedOutput`, `is_error: status!="completed" || exitCode∉{0,null}`).
- `fileChange`: single `add` → `"Write"` (`{"file_path", "content": diff}`) with `structured_json` `{"type":"create","structuredPatch":[],"originalFile":null}` (`edit`); otherwise → `"Edit"` (`{"file_path", "structuredPatch"}` so pending approvals also render in the diff modal) and `ToolResult` with `"<path>: edited"` per change + parsed unified diff `structuredPatch` (`edit_update`).
- Approval requests → flush text, `PermissionRequest` reusing the tool's name and `input_json`; Auto ON accepts automatically (`SetAutoMode(true)` also accepts already-pending approvals).
- Usage on `turn/completed`: `Δtotal` since `turn/started` (first turn after resume without a pre-turn update baselines at `total - last`, `resume`), `input_tokens = Δinput - Δcached`, `cache_read_tokens = Δcached`, `output_tokens = Δoutput`, `context_tokens_used = last.totalTokens`, `context_window_max = modelContextWindow`, `cost_usd = 0`.
- `error` (`willRetry:false`) → `Error`; `turn/completed` `status:"failed"` without prior `error` emits `Error("Codex turn failed")` + error `TurnComplete`.

## Lifecycle and tests
- Shutdown: close stdin, wait 3 s, SIGTERM process group, wait 2 s, SIGKILL; post-reap SIGKILL reaps background children. Mid-turn unexpected exit emits `Error` + error `TurnComplete` + `Exited`.
- Tests: `tests/fixtures.rs` replays all 16 recorded fixtures; `tests/fake.rs` drives `CodexProvider` against `tests/fake_codex.py`; `tests/e2e.rs` runs 4 real-codex tests (`XADE_E2E_CODEX=1 cargo test -p xade-codex --test e2e -- --ignored --test-threads=1`).
