# Claude CLI protocol
Verified against Claude Code 2.1.330 with fixtures in `crates/claude/fixtures/` (re-record with `crates/claude/scripts/record.py` after a CLI upgrade). Fixture lines are `{"dir": "in"|"out"|"meta", "frame": …}`.

## Spawn
- Args: `-p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --permission-prompt-tool stdio`, plus `--model <alias>`, `--effort <low|medium|high|xhigh|max>` (omit for CLI default), `--resume <id>` or `--session-id <uuid>`.
- Child env `CLAUDE_CODE_ENTRYPOINT=xade-code` (the app's own id, not `sdk-cli`/`sdk-ts` and not another client's): unset, `-p` reports `sdk-cli` and the 5-hour meter moved ~2× further per dollar than under `claude-vscode` (A/B). Unverified that an unknown id meters like `claude-vscode`; re-measure before trusting or changing it.
- `--setting-sources project` is used by the recorder only, for isolation; the app keeps the user's settings.
- Models: preflight sends `control_request initialize` to a short `claude -p` process (no prompt, free) and reads `response.models` (`value`, `resolvedModel`, `displayName`, `description`, `supportedEffortLevels`); sessions store `resolvedModel`, deduped with the "default" entry. Static aliases `fable`, `opus`, `sonnet`, `haiku` are the fallback. No thinking on/off flag exists.
- Auth preflight: `claude auth status --json`; exit 0 + `loggedIn: true` when logged in, exit 1 when not. Read only `loggedIn`, plus `email` as the status-bar account label, display only.
- GUI apps on macOS lack the shell PATH: look up `claude` in `$PATH`, then `~/.claude/local`, `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin` (the default install).

## Stdin (we send)
- Prompt: `{"type":"user","session_id":"<id or empty>","parent_tool_use_id":null,"message":{"role":"user","content":[{"type":"text","text":…}]}}`.
- Allow: `{"type":"control_response","response":{"subtype":"success","request_id":R,"response":{"behavior":"allow","updatedInput":<input>}}}`; deny: `…"response":{"behavior":"deny","message":"…"}`.
- `@path` and `@"path with spaces"` in the prompt text are expanded by the CLI itself (fixtures `at_mention`, `at_mention_spaces`): the file is read with no tool call or permission prompt.
- Images (`@red.png`, inside or outside the cwd) are expanded the same way (fixtures `at_image`, `at_image_outside`); PDFs and binaries are not: the model reaches for Read/Bash (fixtures `at_pdf`, `at_binary`), which can raise permission prompts.
- A prompt written while a turn runs is folded into it at the next tool boundary; one `result` ends both (fixture `steer`).
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

## Frame → event (`crates/claude/src/translate.rs`)
- `system/init` → `Started` (every turn; consumers dedupe). `system/thinking_tokens` → `ThinkingProgress`.
- `stream_event` `text_delta` → `TextDelta`; other deltas ignored.
- `assistant` block `text` → `AssistantText`, `tool_use` → `ToolUse`; `thinking` skipped.
- `user` `tool_result` → `ToolResult` (`tool_use_result` → `structured_json`); `user` text frames ignored.
- `rate_limit_event` → one `Quota` per `unifiedWindows` entry (utilization × 100).
- `result` → `TurnComplete`; `interrupted` comes from the command loop's flag, not the frame, and `is_error` is cleared when interrupted.
- `control_request/can_use_tool` → `PermissionRequest`; in auto mode the reader allows it itself and emits nothing.
- A manual allow echoes the stored original `input` as `updatedInput`; deny sends "Denied by user".
- Unknown or unparsable frames are skipped.

## Transcripts (`crates/claude/src/transcript.rs`, fixture `fixtures/transcripts/`)
- The CLI writes `~/.claude/projects/<dir>/<sessionId>.jsonl`; the file stem is the `--resume` id and a resume keeps it. Entries: `user`, `assistant`, `ai-title` matter; `isSidechain`/`isMeta`/`isCompactSummary` and every other type are skipped.
- Mapping: `cwd` = workspace, last `aiTitle` (else first user text) = title, last assistant `model`; user text and `tool_result` (+ `toolUseResult`) and assistant `text`/`tool_use` become messages; usage is deduped per `message.id`.
- Live sessions: `~/.claude/sessions/<pid>.json` (`pid`, `sessionId`, `status` busy|idle, `entrypoint`) per CLI process (`crates/claude/src/running.rs`); read only `*.json`, never the `<pid>.<hex>.key` files; drop entries whose pid is dead (`kill(pid,0)`, EPERM = alive) since crashes leave stale files.

## Lifecycle
- Closing stdin ends the process after the current turn (exit 0; exit 1 if the last turn was interrupted).
- Shutdown: close stdin, wait 3 s, SIGTERM the process group, wait 2 s, SIGKILL; `Exited` is always the last event.
- Parent death: after `kill -9` of the parent the child exits on stdin EOF in about 1 s (no pidfile needed); re-check with `cargo run -p xade-claude --example parent_death`.
- `delete_session` removes `~/.claude/projects/*/<id>.jsonl` and the `<id>/` dir beside it; the id must match `[A-Za-z0-9_-]{1,128}` (no separators or `..`), project dirs are never followed through symlinks, a missing file is fine.
