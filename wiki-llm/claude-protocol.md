# Claude CLI protocol
Verified against Claude Code 2.1.330 with fixtures in `crates/claude/fixtures/` (re-record with `crates/claude/scripts/record.py` after a CLI upgrade). Fixture lines are `{"dir": "in"|"out"|"meta", "frame": …}`.

## Spawn
- Args: `-p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --permission-prompt-tool stdio`, plus `--model <alias>`, `--effort <low|medium|high|xhigh|max>` (omit for CLI default), `--resume <id>` or `--session-id <uuid>`.
- `--setting-sources project` is used by the recorder only, for isolation; the app keeps the user's settings.
- Models: aliases `fable`, `opus`, `sonnet`, `haiku`. No thinking on/off flag exists.
- Auth preflight: `claude auth status --json`; exit 0 + `loggedIn: true` when logged in, exit 1 when not. Read only `loggedIn`.
- GUI apps on macOS lack the shell PATH; the default install is `~/.local/bin/claude`.

## Stdin (we send)
- Prompt: `{"type":"user","session_id":"<id or empty>","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"text","text":…}]}}`.
- Allow: `{"type":"control_response","response":{"subtype":"success","request_id":R,"response":{"behavior":"allow","updatedInput":<input>}}}`; deny: `…"response":{"behavior":"deny","message":"…"}`.
- Interrupt: `{"type":"control_request","request_id":<ours>,"request":{"subtype":"interrupt"}}`; acked by `control_response`. Process stays alive.
- No `initialize` request is needed (fixture `init_first` shows it is accepted but optional).

## Stdout (we read)
- `system/init` is emitted at the start of **every** turn; carries `session_id`, `model`, `permissionMode`.
- `stream_event` wraps API events: `content_block_delta` with `text_delta`, `thinking_delta`, `input_json_delta`, `signature_delta`.
- `assistant` frames arrive **one per content block** (`text` | `thinking` | `tool_use`), before that block's `content_block_stop`.
- Thinking text is not exposed: `thinking_display:"updates"`, deltas and finalized blocks are empty; `system/thinking_tokens` gives `estimated_tokens`.
- `user` frames from the CLI carry `tool_result` blocks (`tool_use_id`, `content`, `is_error`) plus a `tool_use_result` object; Edit/Write include `originalFile` and `structuredPatch` (hunks with old/new line numbers).
- `control_request/can_use_tool`: `request_id`, `tool_name`, `input`, `tool_use_id`, `description`, `permission_suggestions`. Arrives after the tool's `assistant` frame. Read-only tools (e.g. `ls`) may run without asking.
- `rate_limit_event.rate_limit_info.unifiedWindows.{five_hour,seven_day}`: `utilization` (0–1), `resetsAt` (epoch s).
- `result`: `subtype`, `is_error`, `total_cost_usd`, `usage`, `modelUsage[model].contextWindow`, `permission_denials`. After an interrupt: `subtype:"error_during_execution"`, `is_error:true`, preceded by a `user` text frame `[Request interrupted by user]` — treat as interrupted, not an error.
- Context used = last `assistant.message.usage` input + cache_read + cache_creation tokens.

## Lifecycle
- Closing stdin ends the process with exit 0 after the current turn.
