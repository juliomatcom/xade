# Provider protocols

How Xade talks to each provider's CLI. Every provider runs as one long-lived child process per session, with JSON objects one per line on stdin/stdout. Xade spawns the vendor binary unmodified and never reads its credentials.

| | Claude | Antigravity | Codex | OpenCode |
|---|---|---|---|---|
| Command | `claude -p --input-format stream-json --output-format stream-json …` | `agy acp` | `codex app-server` | `opencode acp` |
| Protocol | Claude Code stream-json (not JSON-RPC) | ACP (Agent Client Protocol) over JSON-RPC 2.0 | Codex app-server protocol over JSON-RPC 2.0 | ACP over JSON-RPC 2.0 |
| Framing | NDJSON | NDJSON | NDJSON | NDJSON |
| Process | One per session | One per session | One per session | One per session (replaced after some Stops) |
| Handshake | None (`initialize` optional) | `initialize` → `session/new` (lazy, on the first prompt) | `initialize` + `initialized` → `thread/start` (lazy, on the first prompt) | `initialize` → `session/new` (lazy, on the first prompt) |
| Send a prompt | `{"type":"user",…}` frame | `session/prompt` | `turn/start` | `session/prompt` |
| Prompt while a turn runs | Folded into the turn | Second `session/prompt` | `turn/steer` | `_session/steering` |
| Streaming | `stream_event` partial messages | `session/update` notifications | `item/started`, `item/*/delta`, `item/completed` | `session/update` notifications |
| Approvals | `control_request` (`--permission-prompt-tool stdio`) | `session/request_permission` (agy → Xade) | `item/commandExecution/requestApproval`, `item/fileChange/requestApproval` (codex → Xade) | `session/request_permission` (OpenCode → Xade) |
| Interrupt | `control_request` `interrupt` | `session/cancel` | `turn/interrupt` | `session/cancel` (process restart if subagents are involved) |
| Turn end | `result` frame | `session/prompt` response | `turn/completed` (`completed`, `interrupted`, `failed`) | `session/prompt` response; a quiet period for background reports |
| Resume | `--resume <id>` flag | `session/resume` (model set again) | `thread/resume` | `session/resume` |
| Models and effort | `initialize` → `response.models`; `--effort` | `configOptions` `model`, `thought_level` | `model/list` → `supportedReasoningEfforts`; `effort` per `turn/start` | `opencode models`; `configOptions` `model`, `effort` |
| Auto mode | Allows every permission request | Allows every permission request | `approvalPolicy` `never` (off: `untrusted`) per turn | Off: ask rules in `OPENCODE_CONFIG_CONTENT`; on: allows every request; a change respawns the process |
| Usage | `result.usage` | `usage_update` + prompt response | `thread/tokenUsage/updated` | `usage_update` + prompt response |
| Auth check | `claude auth status --json` | `-32000` on session calls | `account/read` | None (`opencode models`) |
| Delete history | Transcript file | Conversation files | `thread/delete` | `opencode session delete` |
| Fixtures | `crates/claude/fixtures` | `crates/antigravity/fixtures` | `crates/codex/fixtures` | `crates/opencode/fixtures` |

Protocol details per provider live in `wiki-llm/claude-protocol.md`, `wiki-llm/antigravity-protocol.md`, `wiki-llm/codex-protocol.md` and `wiki-llm/opencode-protocol.md`.
