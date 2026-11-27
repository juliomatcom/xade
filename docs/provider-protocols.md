# Provider protocols

How Xade talks to each provider's CLI. Every provider runs as one long-lived child process per session, with JSON objects one per line on stdin/stdout. Xade spawns the vendor binary unmodified and never reads its credentials.

| | Claude | Antigravity | Codex |
|---|---|---|---|
| Command | `claude -p --input-format stream-json --output-format stream-json …` | `agy acp` | `codex app-server` |
| Protocol | Claude Code stream-json (not JSON-RPC) | ACP (Agent Client Protocol) over JSON-RPC 2.0 | Codex app-server protocol over JSON-RPC 2.0 |
| Framing | NDJSON | NDJSON | NDJSON |
| Process | One per session | One per session | One per session |
| Handshake | None (`initialize` optional) | `initialize` → `session/new` (lazy, on the first prompt) | `initialize` + `initialized` → `thread/start` (lazy, on the first prompt) |
| Send a prompt | `{"type":"user",…}` frame | `session/prompt` | `turn/start` |
| Prompt while a turn runs | Folded into the turn | Second `session/prompt` | `turn/steer` |
| Streaming | `stream_event` partial messages | `session/update` notifications | `item/started`, `item/*/delta`, `item/completed` |
| Approvals | `control_request` (`--permission-prompt-tool stdio`) | `session/request_permission` (agy → Xade) | `item/commandExecution/requestApproval`, `item/fileChange/requestApproval` (codex → Xade) |
| Interrupt | `control_request` `interrupt` | `session/cancel` | `turn/interrupt` |
| Turn end | `result` frame | `session/prompt` response | `turn/completed` (`completed`, `interrupted`, `failed`) |
| Resume | `--resume <id>` flag | `session/resume` (model set again) | `thread/resume` |
| Models and effort | `initialize` → `response.models`; `--effort` | `configOptions` `model`, `thought_level` | `model/list` → `supportedReasoningEfforts`; `effort` per `turn/start` |
| Auto mode | Allows every permission request | Allows every permission request | `approvalPolicy` `never` (off: `untrusted`) per turn |
| Usage | `result.usage` | `usage_update` + prompt response | `thread/tokenUsage/updated` |
| Auth check | `claude auth status --json` | `-32000` on session calls | `account/read` |
| Delete history | Transcript file | Conversation files | `thread/delete` |
| Fixtures | `crates/claude/fixtures` | `crates/antigravity/fixtures` | `crates/codex/fixtures` |

Protocol details per provider live in `wiki-llm/claude-protocol.md`, `wiki-llm/antigravity-protocol.md` and `wiki-llm/codex-protocol.md`.
