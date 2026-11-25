# Antigravity CLI protocol (ACP)
Verified against `agy` 1.5.16 with fixtures in `crates/antigravity/fixtures/` (re-record with `crates/antigravity/scripts/record.py <scenario> <mktemp -d>` after a CLI upgrade). Fixture lines are `{"dir": "in"|"out"|"meta", "frame": …}`; `meta` marks `restart` between two processes. Integration is ACP (`agy acp`, JSON-RPC 2.0, one JSON object per line); `agy -p --input-format stream-json` has no approval channel and is not used.

## Spawn
- `agy acp` with `cwd` = the session directory, no other flags, environment untouched; logs go to `~/.gemini/antigravity-cli/log/`, never read.
- Closing stdin exits 0 (fixtures end with `exit_code: 0`).
- Binary lookup: `$PATH`, `~/.gemini/bin/agy`, `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`.
- Unknown method sent to agy: error `-32601 "Method not found: <m>"` (fixture `unknown_method`).

## Handshake
- `initialize` `{protocolVersion:1, clientCapabilities:{fs:{readTextFile:false,writeTextFile:false}, terminal:false}}`; result has `protocolVersion:1`, `agentCapabilities.loadSession`, `sessionCapabilities.resume`, `promptCapabilities.image`, `agentInfo`, `authMethods`.
- `session/new` `{cwd, mcpServers:[]}` → `sessionId` (UUID), `models.currentModelId`, `modes`, `configOptions` with ids `model`, `thought_level`, `mode`. `session/new` persists a conversation even with no prompt, so create it lazily on the first prompt.
- `configOptions.model` values are **base ids** (`gemini-4-neon`, `gemini-3.8-flash`); `models.availableModels` carries the effort-suffixed ids (`gemini-4-neon-high`). `thought_level` values: `low|medium|high|xhigh`, default `high`.
- `session/set_config_option` `{sessionId, configId, value}` → `{configOptions}` (`set_config`); an unknown model is `-32602 "Unknown model ID: x"` and the session stays usable (`bad_model`). A `config_option_update` notification follows later; ignore it.
- `session/resume` `{sessionId, cwd, mcpServers:[]}` returns the session state with **no history replay** and keeps the conversation, but the **model resets to the CLI default** (`currentModelId` `gemini-4-neon-high`): set it again after every resume (`resume`). A resume of an unknown id fails with `-32603 "Failed to read session <id>; …"` (`bad_resume`).
- Auth failure (`-32000 "Authentication required"` per the docs) is unverified: a signed-out run could not be recorded. Treat `-32000` or a message containing "auth" on `session/new`/`resume` as not signed in.
- After a prompt or `set_config_option` comes `usage_update {used,size}` (context tokens, window 1048576). `available_commands_update` is huge and lists the user's skills: ignored and stripped from fixtures.

## Turn
- `session/prompt` `{sessionId, prompt:[{type:"text",text}]}`; the response is `{stopReason, usage:{inputTokens,outputTokens,cachedReadTokens,cachedWriteTokens,thoughtTokens,totalTokens}}`. `usage` is per turn, not cumulative (`two_turns`). A prompt to an unknown session is `-32602 "Session not found: <id>"` (`bad_prompt_session`).
- Streaming is `session/update` with `params.update.sessionUpdate`: `agent_message_chunk` (text delta), `agent_thought_chunk` (thinking text, exposed on longer prompts: `interrupt`, `steer`, `thinking`), `tool_call`, `tool_call_update`, `usage_update`, `session_info_update` (auto title, ignored), `config_option_update` and `user_message_chunk` (ignored).
- A tool call is `tool_call` (`status:"in_progress"`), then `tool_call_update` `pending` (before a permission request), `in_progress`, then terminal. Fields: `toolCallId`, `name` (`read_file`, `edit_file`, `run_command`), `kind`, `title`, `args`, `rawInput`, `rawOutput`, `content[]`.
- Terminal results: `completed` with `rawOutput.output` for commands (`tool_allow`); `edit_file` has `rawOutput:{}` and `content:[{type:"diff",path,oldText,newText}]` (`edit`); a denied or cancelled command is `status:"failed"`, with `rawOutput` a plain string (deny) or `{output:""}` (cancel) (`tool_deny`, `interrupt_pending_permission`).
- Default mode gates `run_command` but not workspace reads and edits (`edit` has no request).
- Permission request: agy → Xade request `session/request_permission` with a **string** id (`"1"`), `params.toolCall`, `options:[{optionId:"allow",kind:"allow_once"},{optionId:"deny",kind:"reject_once"}]`; reply `{"outcome":{"outcome":"selected","optionId":…}}` with the original id value. Deny is a failed tool plus a normal `end_turn` (`tool_deny`).
- Cancel: notification `session/cancel {sessionId}`; the prompt resolves `stopReason:"cancelled"` with that turn's usage and the process stays usable (`interrupt`). With a request pending, answer it `{"outcome":{"outcome":"cancelled"}}` first, then send `session/cancel` (`interrupt_pending_permission`). A request that arrives after Stop (interrupted, prompts outstanding) is answered `cancelled` at once and never shown, even in auto mode.
- Steer: a second `session/prompt` mid-turn resolves the first at once with `end_turn` while the turn continues; the last response ends it (`steer`). The turn ends when no prompt is outstanding.
- Quota is not in ACP: `AgentProvider::quota` runs `agy -p /usage --output-format json` (stdout: `command.data.groups[]` of `{name, buckets[{window:"5h"|"weekly", remaining_fraction, reset_time}]}`, `crates/antigravity/src/usage.rs`, recorded sample in its tests). Not available: cost, auth status, fork, effort `max`.
- Any agy request other than `session/request_permission` must be answered `-32601`, or agy blocks.

## Frame → event (`crates/antigravity/src/translate.rs`)
- `agent_message_chunk` → `TextDelta`, buffered; the buffer is flushed as `AssistantText` before a thought, a tool call or result, a permission request and at turn end (so cancel keeps the text).
- `agent_thought_chunk` → `ThinkingProgress{chars/4}`; text is never stored; resets each turn.
- First sight of a `toolCallId` (from `tool_call`, an update, or a permission request) → one `ToolUse` (`rawInput`, else `args`, else `{}`).
- Terminal `tool_call_update` (any status but `pending`/`in_progress`) → one `ToolResult`, `is_error` unless `completed`, `structured_json` = `{"content":…}`. Output: `rawOutput.output`, else `rawOutput` as a string, else `content` text, else `"<path>: edited"` per diff, else empty.
- `usage_update` → stored as the context figures; no event.
- `session/prompt` response: not the last outstanding → usage summed, no event; last → `TurnComplete` (usage summed, `cost_usd` 0). `cancelled` or the interrupt flag → `interrupted`; any `stopReason` but `end_turn` → `is_error` plus `Error("Antigravity stopped: …")`. A JSON-RPC error on a prompt → `Error`, plus an error `TurnComplete` if it was the last.
- Updates for another `sessionId` (`rpc::update_for`), unknown `sessionUpdate` values and unparsable lines are skipped.

## Process and provider (`process.rs`, `provider.rs`, `models.rs`)
- One task owns the child and its stdin; a reader task per pipe. Lines have no length cap; non-UTF-8 and unparsable lines are skipped. The child leads its own process group.
- Start: `initialize` at once, `session/new` (or `session/resume` when `resume` is set) only on the first prompt, so an unused session creates no conversation. Prompts sent earlier are queued.
- After new/resume: `Started`, then `set_config_option` `model` if it differs from the option's current value (always needed after resume), then `thought_level` if the effort maps to a value the model offers (`Default` and `Max` set nothing; the valid levels depend on the model), each awaited before the queue drains. A rejected option is an `Error`, not fatal.
- Not signed in (`-32000` or "auth" in the message) or a failed resume: `Error`, the queued prompts fail as one error `TurnComplete`, then shutdown and `Exited`.
- `Interrupt` before the session is ready drops the queued prompts and emits an interrupted `TurnComplete`; otherwise it answers pending permissions `cancelled`, then sends `session/cancel`.
- `SetAutoMode(true)` also allows the permissions already pending (Claude leaves them to the user).
- Shutdown and parent death: close stdin, wait 3 s, SIGTERM the group, wait 2 s, SIGKILL; no `session/close`. A child that dies on its own gives `Error` with the stderr tail, then `Exited`. Once the child is reaped by any path the group gets SIGKILL, so tool processes it started never outlive the session.
- `preflight` runs `agy models` (15 s limit): exit 0 with models fills the catalog; auth words in stdout/stderr give `NotLoggedIn`; any other non-zero exit (e.g. offline) or a timeout is `Protocol` with a short reason. Only stdout is parsed (the spinner is on stderr). Slugs `<base>-(low|medium|high|xhigh|max)` sharing a base become one model with its levels, equal to the ACP `model` values.
- `delete_session` removes `conversations/<id>.db` (+`-wal`/`-shm`), `annotations/<id>.pbtxt` and `brain/<id>` under `~/.gemini/antigravity-cli`, never following symlinks and never touching `conversation_summaries.db`.
- Tests: `tests/fake.rs` drives the process against `tests/fake_agy.py` replaying fixtures; `tests/e2e.rs` uses the real `agy` (`XADE_E2E_AGY=1 cargo test -p xade-antigravity --test e2e -- --ignored --test-threads=1`).
