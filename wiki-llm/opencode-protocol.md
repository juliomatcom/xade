# OpenCode CLI protocol (ACP)
Verified against opencode 2.4.2 with fixtures in `crates/opencode/fixtures/` (re-record with `crates/opencode/scripts/record.py <scenario> <mktemp -d>` after a CLI upgrade; spends a little quota). Fixture lines are `{"dir": "in"|"out"|"meta", "frame": …}`; meta `restart` marks a second process. Integration is ACP (`opencode acp`, JSON-RPC 2.0, one JSON object per line).

## Spawn
- `opencode acp`, cwd = session directory, no other flags, own process group.
- Environment untouched, except that an Auto OFF process gets `OPENCODE_CONFIG_CONTENT` (see Permissions).
- `opencode acp` starts a private `opencode serve --stdio` in a separate process group; it and running tools exit when the acp process's stdin closes or the process is killed (e2e: shutdown_leaves_none_of_our_processes).
- Binary lookup: `$PATH`, `~/.opencode/bin`, `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`; relative PATH entries are skipped.
- A user's background `opencode serve --service` may be running: Xade never signals processes it did not start. One-shot commands pass `--standalone` so they never contact or start that daemon, and run with cwd `/` (OpenCode runs plugins found in parent folders of its cwd, and `/tmp` is world-writable).
- Unknown method → error `-32601` (unknown_method).

## Handshake
- `initialize` `{protocolVersion:1, clientCapabilities:{fs:{readTextFile:false,writeTextFile:false}, terminal:false}, clientInfo}` → `protocolVersion` 1, `agentInfo` `{name:"OpenCode", version}`, `agentCapabilities.sessionCapabilities` with resume and delete, `_meta.steering.supported` (simple_turn).
- Major version below 2, or a missing or unreadable version, is refused: version 1 uses a different permission key and would run ungated.
- `session/new` `{cwd, mcpServers:[]}` → `sessionId` `ses_…` and `configOptions` `model`, `effort` (category `thought_level`), `mode` (`build`, `plan`; custom primary agents also appear as modes) (simple_turn). Created lazily on the first prompt.
- `session/set_config_option` `{sessionId, configId, value}` → `configOptions`; the effort values depend on the model, so the model is set first; `default` is a value (set_config). An unknown model is `-32602` "model not found" and the session stays usable (bad_model).
- `session/resume` returns `configOptions` (no `sessionId`), replays no history, keeps context, model and effort (resume); an unknown id is `-32602` "session not found" (bad_resume).

## Turn
- `session/prompt` → `{stopReason, usage:{inputTokens, outputTokens, totalTokens, thoughtTokens?, cachedReadTokens?}}`; usage is per turn (two_turns) and `inputTokens` excludes cache reads. A cancelled prompt may carry no usage (interrupt).
- `session/update` kinds used: `agent_message_chunk`, `agent_thought_chunk` (thinking), `tool_call`, `tool_call_update`, `usage_update` `{used,size,cost}`; others are skipped.
- A tool call opens as `tool_call` `pending` with empty or `{cwd}`-only `rawInput`; arguments arrive in `tool_call_update`, which omits `name`; an update without `status` is not terminal (tool_allow, edit).
- Tools: `shell`, `read`, `write`, `edit` (tool_allow, edit); GPT models edit through `patch` with `rawInput.patchText` (patch).
- A completed `write` carries text only, no diff block (edit, edit_auto), so its old text appears only in the diff block of the permission request's `toolCall.content` (edit). `edit` and `patch` carry diff blocks both in the permission request and when completed (edit, patch).
- Steer: `_session/steering` `{sessionId, prompt}` during a prompt → `{outcome:"injected"}`, one prompt response (steer); idle → `{outcome:"promptRequired", reason:"noRunningTurn"}` (steer_idle).
- Cancel: notification `session/cancel`; the prompt resolves `cancelled`, the process stays usable (interrupt); pending requests are answered `cancelled` first (interrupt_pending_permission).

## Permissions
- Request `session/request_permission`, numeric id from 0, options `once` (allow_once), `always` (allow_always), `reject` (reject_once) (tool_allow). Allow replies `once`.
- Rejecting fails the tool ("The user declined this tool call") and ends the prompt `cancelled` (tool_deny, edit_deny).
- OpenCode's defaults allow nearly everything without asking, so Auto OFF sets `OPENCODE_CONFIG_CONTENT` with `ask` rules for `shell`, `edit`, `write`, `patch`, `webfetch`, `websearch`, `execute`, `generate_image`, at top level and for agents `build`, `explore`, `general`; appended to a value the user already set; one that is not a JSON object fails the spawn.
- Rules are last-match-wins and the inline config merges last: only `ask` is ever injected, so under Auto OFF a user `deny` on those actions becomes a prompt (e2e: auto_off_asks_even_when_the_project_allows_the_shell).
- Auto OFF also switches the session to mode `build` (a custom default agent's own `allow` would run unasked) and ends the session if that is refused or not offered (e2e: auto_off_asks_even_with_a_permissive_default_agent).
- Auto ON leaves the environment alone, so user `deny` rules stay `deny`, and any request is answered `allow` (e2e: auto_on_never_weakens_a_deny).
- Not gated: MCP tools, and custom subagents with their own `allow` rules.
- An Auto OFF process never auto-allows, even after Auto is turned on; the session respawns it once idle (`respawn_on_auto_change`).

## Subagents
- Child updates arrive under our `sessionId`, marked `update._meta["opencode/child-session"]`; child `toolCallId`s are `<childSession>:<callId>`; a child permission request carries the marker in `toolCall._meta` (subagent).
- A subagent runs in the background: the parent prompt can end first, then permission requests and the parent's follow-up arrive with no prompt running and no end marker; child tool updates are no longer forwarded (background_subagent). Even a waited-for subagent's follow-up can arrive after the response (subagent).
- `session/cancel` does not stop a background subagent, so a Stop during a turn with no prompt, or after any subagent in that process, kills the child and resumes the session in a new one (e2e: stop_kills_a_background_subagent_before_it_runs).

## Frame → event (`crates/opencode/src/translate.rs`, `patch.rs`)
- Child message, thought and usage frames are skipped; child tool calls are mapped like the parent's.
- `agent_message_chunk` → `TextDelta`, buffered, flushed as `AssistantText` before a thought, tool call or result, permission request and turn end. Thought → `ThinkingProgress` (chars/4).
- One `ToolUse` per call, only once it is in progress, terminal or asked about; never from `pending`.
- Names become Claude's: `shell` → Bash, `read` → Read, `write` → Write, `edit` → Edit; `patch` → Write for a single added file, else Edit with `structuredPatch` hunks (line numbers start at 1; a context line names each file when several or moved; a deletion shows a marker line). A patch that cannot be parsed, or whose reported locations are not all in it, is shown as raw text.
- A terminal status → one `ToolResult` (error unless completed). A completed Write carries the previewed old text; with Auto ON none exists, so the diff shows the whole new content.
- `PermissionRequest` shows the request's own mapped name and input.
- Prompt response → `TurnComplete` (`cost_usd` 0); `cancelled` → interrupted; any other non-`end_turn` stop → error.

## Process and provider (`process.rs`, `provider.rs`, `models.rs`)
- One prompt at a time; a message sent meanwhile steers; a refused steer runs as the next prompt of the same turn (the answer is held until all steers are answered). Prompts sent after Stop wait.
- A turn with no prompt behind it (background report) ends after 5 s of silence, held while a permission is pending or a tool is known to be running. A permission still pending when a prompt ends cleanly keeps the turn open.
- Shutdown: close stdin, 3 s, SIGTERM the group, 2 s, SIGKILL, plus a group SIGKILL after reap. A child dying mid-turn gives `Error` with the stderr tail, an error `TurnComplete`, then `Exited`.
- `preflight`: `opencode models --standalone` (15 s) fills the catalog, one `provider/model` per line; every model offers Low…Max and the process only sends levels the live model lists; default is the first `opencode-go/*` model without `free`, `contributor` or `preview` in its name, else the first such model, else the first.
- `delete_session`: `opencode session delete --standalone <id>` (10 s); "Session not found" counts as done; ids that are not plain tokens are refused. Deleting a session also deletes its subagent sessions.
- No import, running-session detection or quota; no account label (no credential or account is ever read).
- Tests: `tests/fixtures.rs` replays the fixtures; `tests/fake.rs` drives the provider against `tests/fake_opencode.py`; `tests/e2e.rs` uses the real CLI (`XADE_E2E_OPENCODE=1 cargo test -p xade-opencode --test e2e -- --ignored --test-threads=1`).
