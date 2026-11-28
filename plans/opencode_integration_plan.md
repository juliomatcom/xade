# Xade — OpenCode CLI Integration Plan (MVP)

Lead: `#neon-1` (plan, review fixes, final word). Implementer: `#sonnet-1` (plan review round, P1–P4 e2e one phase at a time). Contrarian reviewer: `#sonnet-2` (per-phase adversarial review so no errors reach users in production).
Branch: `feat/opencode-provider` (shared worktree; one branch for everything). Issue: `Closes #7` (`juliomatcom/xade#7`).
Goal: OpenCode is a fourth provider, additive, zero breaking changes: Claude, Antigravity, and Codex behave identically to today, existing DBs open unchanged.
Spirit: pragmatic and simple. Mirror `crates/antigravity` (ACP over JSON-RPC 2.0) + `crates/codex` (Claude-shaped tool mapping, diff modal support, and steer fallback); rough edges in §8 are accepted for v1.

Status: implemented (2026-11-28); P1–P4 on `feat/opencode-provider`.

---

## 0. TL;DR of decisions

| # | Decision | Why |
|---|---|---|
| D1 | Integrate via **ACP**: `opencode acp` (JSON-RPC 2.0 NDJSON over stdio), one process per session. Require OpenCode major version `>= 2` (checked at `initialize`). | Official ACP server with `session/new`, `session/resume`, `session/set_config_option`, `session/prompt`, `session/cancel`, `session/request_permission`, `_session/steering`, and `session/delete`. Verified live against `opencode v2.4.2` (§2). v1 used a different permission config key (`permission` vs `permissions`) and would silently run ungated. |
| D2 | New crate **`crates/opencode`** (`xade-opencode`), sibling of `xade-antigravity` and `xade-codex`. Depends only on `xade-core`. Only `apps/desktop` constructs it (`crates/runtime` dev-dep only if needed). | Preserves dependency rule: `xade-core ← xade-claude, xade-antigravity, xade-codex, xade-opencode, xade-store ← xade-runtime ← xade`. No shared ACP crate refactor in v1: copy `rpc.rs` cleanly. |
| D3 | Add `ProviderId::OpenCode` (serializes as `"opencode"`, display name `"OpenCode"`) and one defaulted trait method `AgentProvider::respawn_on_auto_change(&self) -> bool { false }` (overridden `true` for OpenCode). No change to `ProviderEvent`, `SessionCommand`, or DB schema. | `sessions.provider_id` is free `TEXT` (zero migrations). Auto ON vs Auto OFF differs in spawned process env (D7), so `SessionActor` needs to respawn on Auto toggle at idle, just like model/effort changes. |
| D4 | Create the ACP session **lazily** on the first prompt; spawn `opencode acp` and run `initialize` eagerly. | Opening a window without prompting must not litter OpenCode's session history. |
| D5 | Map core OpenCode tools to Claude-shaped names (`shell` → `Bash`, `read` → `Read`, `write` → `Write`, `edit` → `Edit`, `patch` → `Write`/`Edit` with `structuredPatch`) in `translate.rs`; **never** emit `ToolUse` from `status:"pending"` (wait for `in_progress`, terminal, or permission request). | OpenCode emits `tool_call` (`status:"pending"`, empty or `{cwd}`-only `rawInput`), then `tool_call_update` (`status:"in_progress"`, real `rawInput`, omitting `name`). `gpt-6-luna` edits exclusively via `patch`. Caching metadata + Claude-shaped mapping makes tool cards, syntax highlighting, diff modal, and changes summary work with zero UI changes and satisfies the "no inline diffs" invariant. |
| D6 | Mid-turn steer uses **`_session/steering`** (not a second `session/prompt`). On `outcome:"promptRequired"` or RPC error, fall back to a fresh `session/prompt` once idle. | A second `session/prompt` while active fails with `-32603 "Session already has an active ACP prompt"`. `_session/steering` injects into the running turn (`outcome:"injected"`) and resolves with one `session/prompt` response. |
| D7 | **Auto OFF** sets `OPENCODE_CONFIG_CONTENT` with **only `ask` rules** for `shell`, `edit`, `write`, `patch`, `webfetch`, `websearch`, `execute`, `generate_image` (resource `*`) in both top-level `permissions` and `agents.{build,explore,general}.permissions` (appending to any user-set `OPENCODE_CONFIG_CONTENT`; invalid JSON fails the spawn). **Auto ON** leaves the env untouched and auto-answers `session/request_permission` with `allow_once`. | OpenCode defaults to `* * allow` (only `.env` reads ask), so leaving env untouched on Auto OFF gates nothing. Rules are last-match-wins (built-in → global → project → `OPENCODE_CONFIG_CONTENT`), and project agent-level rules otherwise override top-level rules. Because `OPENCODE_CONFIG_CONTENT` lands last, with Auto OFF any user `deny` rule for a guarded action becomes an `ask` prompt instead of a silent block; therefore a process spawned with Auto OFF never auto-allows on `SetAutoMode(true)` (toggling Auto ON takes effect after the idle respawn starts a clean process where user `deny` rules apply again). Leaving `read` and `external_directory` out of the injected `ask` list keeps read-only ops and fork snapshots (which read `/tmp/xade-forks-<uid>`) working cleanly. |
| D8 | `preflight()` runs `opencode models --standalone` (15 s timeout, ~0.4 s live). `delete_session(id)` runs `opencode session delete --standalone <id>` (10 s timeout, ~0.2 s live). | Without `--standalone`, both commands try to contact/start the managed background daemon (verified: hangs 20 s then fails in isolated HOME, or leaves a daemon outliving Xade). With `--standalone`, both are fast, isolated, and leave no daemon. |
| D9 | Xade ships its own neutral monochrome mark for OpenCode (`assets/opencode-symbol.{svg,png}`); only history import, running-sessions polling and quota are deferred out of v1. | Xade's own neutral glyph is not OpenCode's trademark; import, running-session polling and quota keep v1 small. |

---

## 1. Constraints (read `wiki-llm/conventions.md` first)

- **Dependency rule:** `xade-core ← xade-claude, xade-antigravity, xade-codex, xade-opencode, xade-store ← xade-runtime ← xade`. `xade-runtime` never names `xade-opencode` in library code.
- **Credentials invariant:** Never read, copy, log, or transmit OpenCode credentials (`~/.local/share/opencode/auth.json`, `~/.config/opencode`, `opencode auth export`, etc.). Never run `opencode auth list` or log auth output. `account_label()` returns `None`.
- **Unmodified binary:** Spawn `opencode acp` unmodified, `cwd` = session dir, `kill_on_drop(true)` + `process_group(0)` + process-group SIGKILL reap on exit/shutdown. Only `OPENCODE_CONFIG_CONTENT` is appended on Auto OFF spawns (D7); on Auto ON, env is completely untouched. Never touch `OPENCODE_CONFIG_DIR` (it replaces global config).
- **Never kill `opencode` by name:** A user's `opencode serve --service` background daemon may be running on the machine; tests and shutdown must only reap their own child PID / process group, never `pkill`/`pgrep` globally.
- **Resilience:** Unknown frames, fields, notifications, or tool names are skipped, never fatal. Any server → client request other than `session/request_permission` is answered `-32601` immediately so OpenCode never blocks.
- **Code changes invariant:** All file edits (`write`, `edit`, `patch`) map to Claude-shaped `Write`/`Edit` so code changes are reviewed in the diff modal, never inline.
- **Dependencies:** Pin exact versions matching `crates/antigravity/Cargo.toml` (`libc = "=0.2.191"`, `serde_json = "=1.0.154"`, `tokio = "=1.54.1"`, `time = "=0.3.55"`, `tracing = "=0.1.44"`, dev `tempfile = "=3.27.0"`). No new dependencies.
- **Code quality:** No `unwrap`/`expect` outside tests; `tracing` never logs prompt or message text above `debug`; `cargo fmt`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`, and `cargo test -p xade --test e2e` must be green at every phase gate.

---

## 2. Verified ACP protocol facts (`opencode v2.4.2`, probed 2026-11-28)

All facts below were verified live against `opencode v2.4.2` (`opencode-go/glm-5.3-flash` and `opencode-go/gpt-6-luna`). Phase 1 turns each into a redacted fixture in `crates/opencode/fixtures/*.ndjson` + replay test.

### 2.1 Process and binary lookup
- Command: `opencode acp` with `cwd` = session dir, no extra flags. Closing stdin exits 0.
- `opencode acp` internally spawns `opencode serve --stdio --port 0` in its own process group; closing stdin, `session/cancel`, and SIGKILL of the `acp` process group all cleanly terminate running child tools and the internal server (verified both idle and mid-turn).
- Binary lookup order: `$PATH`, `~/.opencode/bin`, `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`.
- Unknown method sent to OpenCode → `-32601 "\"Method not found\": <m>"`.

### 2.2 Handshake and config options
- `initialize` params: `{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"clientInfo":{"name":"xade","version":<pkg>}}`.
  - Result: `protocolVersion: 1`, `agentInfo: {name:"OpenCode", version:"2.4.2"}`, `agentCapabilities: {loadSession:true, promptCapabilities:{embeddedContext:true,image:true}, sessionCapabilities:{additionalDirectories,close,delete,fork,list,resume}, _meta:{"opencode/child-session-updates":true}}`, `_meta: {steering:{supported:true}}`, `authMethods: [{id:"opencode-login", name:"Login with opencode", description:"Run `opencode auth login` in the terminal"}]`.
  - Version guard: unless `agentInfo.version` parses a major version `>= 2` (missing or unparsable included), fail the spawn with `"OpenCode 2.0 or newer is required; please update OpenCode."` (v1 uses key `permission` instead of `permissions`).
- `session/new` params: `{"cwd":<abs path>,"mcpServers":[]}`.
  - Result: `{"sessionId":"ses_...", "configOptions":[...]}` (no top-level `models`/`modes`) followed by `available_commands_update` (ignore and strip from fixtures).
  - `configOptions` has three entries:
    - `id: "model"` (`category: "model"`): `currentValue` (default `"opencode/seaglass-alpha-free"`), `options: [{value, name}]` (`provider/model`).
    - `id: "effort"` (`category: "thought_level"`): `currentValue: "default"`, `options` vary by model (e.g. `none|minimal|low|medium|high|xhigh|max|default`). **Note:** `configId` is `"effort"` (not Antigravity's `"thought_level"`).
    - `id: "mode"` (`category: "mode"`): `currentValue: "build"`, options `build`, `plan`.
- `session/set_config_option` params: `{"sessionId","configId":"model"|"effort","value"}` → `{"configOptions":[...]}`.
  - Changing `model` updates valid `effort` options in the returned `configOptions`, so set `model` first, then `effort`.
  - For `Effort::Default`, target `"default"` (important on resume, which preserves the prior session's effort!). Send `set_config_option effort` whenever `current != Some(want)` and `values.contains(&want)`.
  - Unknown model → `-32602 "Invalid params: model not found: <m>"` (non-fatal: emit `Error` and continue on current model).
- `session/resume` params: `{"sessionId","cwd","mcpServers":[]}`.
  - Returns `{"configOptions":[...]}` preserving previous model/effort, with no history replay (only `available_commands_update`). Keeps conversation context (verified recall). Still apply `cfg.model_id` and `cfg.effort` if they differ from `currentValue`.
  - Unknown sessionId → `-32602 "Invalid params: session not found: <id>"` (fatal for that spawn: emit `Error`, fail queued turn, shut down).
- `session/delete` params: `{"sessionId"}` → `{}` (idempotent).
- `opencode session delete --standalone <id>`: exits 0 on delete (cross-cwd works); exits 1 with `"Session not found: <id>"` when missing (treat as `Ok(())`).

### 2.3 A turn, tools, and subagents
- `session/prompt` params: `{"sessionId","prompt":[{"type":"text","text":...}]}`.
  - Result: `{"stopReason":"end_turn"|"cancelled"|..., "usage":{"inputTokens":N,"outputTokens":N,"totalTokens":N,"thoughtTokens"?:N,"cachedReadTokens"?:N},"_meta":{}}`.
  - `usage` is **per turn** (non-cumulative); `inputTokens` excludes `cachedReadTokens`. On mid-stream cancel, `usage` may be omitted.
- Streaming notifications (`session/update`, `params.update.sessionUpdate`):
  - `agent_message_chunk`: `{messageId, content:{type:"text",text}}`.
  - `agent_thought_chunk`: `{messageId, content:{type:"text",text}}`.
  - `tool_call`: `{toolCallId, name, title, kind, status:"pending", locations, rawInput}` — arrives with empty `{}` or partial `{"cwd":...}` `rawInput` BEFORE arguments are populated!
  - `tool_call_update`: **omits `name`** and may omit `status`!
    - `status: "in_progress"` carries populated `rawInput`, `title`, `kind`, `locations`. May repeat multiple times.
    - Missing `status` is non-terminal (never treat absent `status` as terminal).
    - `status: "completed"` carries `content` (`[{"type":"content","content":{"type":"text","text":...}}]`, and for `edit`/`write`/`patch` also `{"type":"diff","path","oldText","newText"}`) and `rawOutput`.
    - `status: "failed"` carries error text in `content`.
  - `usage_update`: `{used, size, cost:{amount, currency}}` — context tokens used & window size.
  - `available_commands_update`, `session_info_update`, `config_option_update`, `user_message_chunk`: ignore.
- Observed tool shapes:
  - `shell`: `rawInput: {"command": "...", "cwd": "..."}`, `rawOutput.metadata: {"status":"completed","truncated":false,"exit":0}`.
  - `read`: `rawInput: {"path": "..."}`.
  - `write`: `rawInput: {"path": "...", "content": "..."}`; completed `content` is text only (`"Created file successfully: …"` / `"Wrote file successfully: …"`), **never a diff block** (fixtures `edit`, `edit_auto`).
  - The diff preview of `write`/`edit`/`patch` (`oldText: null|string`) is in the permission request's `toolCall.content`, so under Auto ON a `write`'s old text is never available.
  - `rawInput.path` may be relative or absolute; `locations[0].path` is absolute. Pending `rawInput` is `{}` for `write`/`read`/`edit`/`patch` and `{cwd}` for `shell`.
  - `edit`: `rawInput: {"path": "...", "oldString": "...", "newString": "..."}`; completed `content` includes `{type:"diff", path, oldText, newText}`.
  - `patch` (used by `opencode-go/gpt-6-luna`): `rawInput: {"patchText": "*** Begin Patch\n...\n*** End Patch"}`; completed `content` includes diff blocks `{type:"diff", path, oldText: null|string, newText}`.
  - `question` tool: fails cleanly under ACP (no special handling needed).
- **Child / subagent updates (`_meta["opencode/child-session"]`):**
  - Child session updates arrive under **our** `sessionId`, distinguished by `update._meta["opencode/child-session"]`.
  - Skip child `agent_message_chunk`, `agent_thought_chunk`, and `usage_update` (so subagent text/thinking/context never corrupts the parent's reply or context meter). Map child `tool_call`/`tool_call_update` normally.
  - Child `toolCallId`s are namespaced `<childSessionId>:<callId>`, in updates and permission requests alike.
  - Child permission requests arrive under our `sessionId` with the marker in `params.toolCall._meta` (none on `params`) and are handled normally.
- **Background subagents (verified):**
  - A parent `session/prompt` can resolve with `stopReason: "end_turn"` while a background subagent is still running. Later, child permission requests and parent follow-up `agent_thought_chunk`/`agent_message_chunk` arrive with **no active `session/prompt`** and **no end marker**.
  - Once the parent's prompt resolved, the child's `tool_call`/`tool_call_update` frames are **not forwarded**: a call announced only by such a permission request never gets a `ToolResult` (fixture `background_subagent`).
  - The parent follow-up lands after the response even when the model was told to wait for the subagent (fixture `subagent`).
  - Handled by a synthetic turn with a quiet timer in `process.rs` (§4).

### 2.4 Approvals, permissions env, cancel, and steer
- **Auto OFF permission config (`OPENCODE_CONFIG_CONTENT`):**
  - Rules are last-match-wins (`findLast`): built-in/agent defaults → global → project → `OPENCODE_CONFIG_CONTENT`.
  - Injected rules (ONLY `ask`, never `allow`):
    - Actions: `shell`, `edit`, `write`, `patch`, `webfetch`, `websearch`, `execute`, `generate_image`; resource `"*"`; effect `"ask"`.
    - Placed in BOTH top-level `permissions` AND `agents.build.permissions`, `agents.explore.permissions`, `agents.general.permissions` (otherwise a project agent-level `allow` lands after the top-level rule).
  - Because our `ask` rules land last, with Auto OFF any user `deny` rule for a guarded action becomes an `ask` prompt (never silently allowed). A process spawned with Auto OFF ignores `SetAutoMode(true)` so denied actions are never auto-allowed mid-turn; switching to Auto ON takes effect after the idle respawn brings a clean environment where user `deny` rules apply again.
  - Auto OFF also switches session `mode` to `"build"` (and fails the session if `"build"` is not offered or rejected).
  - If `OPENCODE_CONFIG_CONTENT` is already set in the environment: parse as JSON object and append our rules to each array (creating arrays/objects as needed). If it is not valid JSON (or not an object), fail the spawn cleanly with a clear `ProviderError::Protocol` error rather than silently dropping gating.
  - Auto ON spawns leave `OPENCODE_CONFIG_CONTENT` untouched so user `deny` rules stay `deny`.
- **Permission request** (OpenCode → Xade JSON-RPC request; numeric `id` starting at `0`):
  - Method: `session/request_permission`.
  - Params: `{"sessionId", "toolCall": {toolCallId, name, title, kind, status:"pending", locations, rawInput}, "options": [{"optionId":"once","kind":"allow_once","name":"Allow once"}, {"optionId":"always","kind":"allow_always","name":"Always allow"}, {"optionId":"reject","kind":"reject_once","name":"Reject"}]}`.
  - Reply: `{"jsonrpc":"2.0","id":<same JSON id>,"result":{"outcome":{"outcome":"selected","optionId":...}}}` (or `{"outcome":{"outcome":"cancelled"}}`).
  - On deny (`reject`): tool updates to `status: "failed"` (`"The user declined this tool call"`) and `session/prompt` resolves with `stopReason: "cancelled"` (mapped to `TurnComplete { is_error: false, interrupted: true }`).
- **Cancel:** notification `session/cancel {"sessionId"}`.
  - Clear `queued` and invalidate any in-flight `Pending::Steer` (so a late `promptRequired`/error after Stop never starts a ghost turn).
  - Answer all pending permission requests with `{"outcome":{"outcome":"cancelled"}}`, then send `session/cancel`.
  - Active `session/prompt` resolves with `stopReason: "cancelled"`. Process stays alive and reusable.
  - Any prompt sent *after* Stop while the cancelled turn unwinds queues in `queued` and starts as a fresh `session/prompt` once the cancelled prompt resolves (never steers a cancelling turn).
- **Steer:** while a non-interrupted `session/prompt` is active, send `_session/steering {"sessionId", "prompt":[{"type":"text","text":...}]}`:
  - `result.outcome == "injected"` → injected into the running turn; original `session/prompt` ends both with one `TurnComplete`.
  - `result.outcome == "promptRequired"` (`reason: "noRunningTurn"` when idle, fixture `steer_idle`) or RPC error (and steer was NOT invalidated by `Interrupt`) → if idle, send `session/prompt(text)` now; else push to `queued`.

### 2.5 Preflight (`opencode models --standalone`)
- `opencode models --standalone` runs with `current_dir("/")` (never `/tmp`: OpenCode executes `.opencode/plugin(s)/*.js` in ancestors of cwd, and `/tmp` is world-writable on shared Linux) and exits 0 in ~0.4 s, printing one model ID per line (`opencode-go/...`, `opencode/...`, etc.).
- Works even with zero auth (returns free `opencode/*` models).
- Non-zero exit mentioning auth/login/sign-in → `ProviderError::NotLoggedIn`; other non-zero exit or timeout → `ProviderError::Protocol(...)`; empty stdout on exit 0 → `ProviderError::Protocol(...)`.
- Every `ModelOption` sets `effort_levels: vec![Effort::Low, Effort::Medium, Effort::High, Effort::XHigh, Effort::Max]` (required so Xade's `effort_choices()` shows all levels instead of only `Default`; `process.rs` filters against the live model's `configOptions.effort`).
- Default model selection: prefer the first `opencode-go/*` model whose slug does not contain `free`, `contributor`, or `preview` (case-insensitive); else the first model without `free`/`contributor`/`preview`; else the first model. Fallback before preflight: empty catalog, `default_model()` = `""`.

---

## 3. Domain mapping (adapter contract → `xade_core`)

| OpenCode ACP | `ProviderEvent` / behavior |
|---|---|
| `session/new` or `session/resume` ok | `Started{provider_session_id: sessionId, model_id}` (once per process) |
| `agent_message_chunk` (non-child) | Append to `text_buf`, emit `TextDelta(text)` |
| `text_buf` non-empty before thought, tool call/result, permission, or turn end | Flush `AssistantText{text}` first, clear `text_buf` |
| `agent_thought_chunk` (non-child) | `thought_chars += text.chars().count()`, emit `ThinkingProgress{estimated_tokens: thought_chars / 4}`; reset at turn end |
| `agent_message_chunk` / `agent_thought_chunk` / `usage_update` with `update._meta["opencode/child-session"]` present | Skip (do not merge subagent text/thinking/context into parent) |
| `tool_call` with `status:"pending"` (or missing status) | Cache `toolCallId -> {name, kind, title, locations, rawInput}`; do **not** emit `ToolUse` yet |
| `tool_call` / `tool_call_update` with `status:"in_progress"` or terminal, OR `session/request_permission` | Merge cached `name`/`kind`/etc., flush text, emit `ToolUse` once per `toolCallId` using Claude-shaped mapping below |
| `tool_call_update` with missing `status`, or `status ∈ {"pending","in_progress"}` | Non-terminal (no `ToolResult`) |
| `tool_call_update` terminal (`status` present and `∉ {"pending","in_progress"}`) | Flush text, emit `ToolResult` once per `toolCallId` (`is_error = status != "completed"`). See structured_json rules below. |
| `usage_update {used, size}` (non-child) | Store `context_used = used`, `context_size = size`; no event |
| `session/request_permission`, Auto ON | Reply at once with `allow_once` (fallback `allow_always` / `allow*`); no event |
| `session/request_permission`, Auto OFF | Flush text, ensure `ToolUse` emitted, emit `PermissionRequest{request_id, tool_use_id, tool_name, input_json}` (same mapped name & input as `ToolUse`) |
| `session/prompt` result, `last == false` (more `queued` prompts ready to send) | Sum usage into `turn_usage`, flush text; no `TurnComplete` yet |
| `session/prompt` result, `last == true` | Flush text; `TurnComplete{usage, is_error, interrupted}` (`interrupted = stopReason=="cancelled" \|\| interrupt_flag`, `is_error = !interrupted && stopReason != "end_turn"`; emit `Error("OpenCode stopped: <stopReason>")` when `is_error`). `cost_usd = 0.0`. |
| JSON-RPC error on `session/prompt` | Flush text, `Error(message)`, and if `last` emit `TurnComplete{is_error:true, interrupted:false}` |
| Synthetic turn completion (quiet timer or Stop) | Flush text; `TurnComplete{usage: last context figures, is_error:false, interrupted}` |
| Unknown server request | Reply `-32601 "Method not found"` immediately |
| Other `sessionId` / unknown `sessionUpdate` / invalid line | Skip (`tracing::debug!`) |
| Child exit | Flush remaining text; if unexpected & non-zero emit `Error` with stderr tail (and error `TurnComplete` if mid-turn or synthetic turn or queued); emit `Exited{code}` last |

**Tool name & input mapping (`translate.rs`):**
- `shell` → `"Bash"`, `input_json`: `{"command": rawInput.command ?? title ?? "", "description": ""}`
- `read` → `"Read"`, `input_json`: `{"file_path": rawInput.path ?? locations[0].path ?? ""}`
- `write` → `"Write"`, `input_json`: `{"file_path": rawInput.path ?? locations[0].path ?? "", "content": rawInput.content ?? ""}`
  - Terminal `structured_json` (only when `status == "completed"`, else `None`): from the first `type:"diff"` block seen for the call (permission preview) — `oldText` string → `{"type":"update","structuredPatch":[],"originalFile": oldText}`, `null` → `create`. No block seen (Auto ON): `originalFile: null`, `type` `create` when the output starts with `"Created"`, else `update` (the modal then shows the whole new content).
- `edit` → `"Edit"`, `input_json`: `{"file_path": rawInput.path ?? locations[0].path ?? "", "old_string": rawInput.oldString ?? "", "new_string": rawInput.newString ?? ""}`
  - Terminal `structured_json`: `None` (so `diff.rs` uses `old_string`/`new_string`, and failed/denied edits carry no patch).
- `patch` → parse `rawInput.patchText` (`*** Begin Patch` … `*** End Patch`, supporting `*** Add File:`, `*** Update File:`, `*** Move to:`, `*** Delete File:`):
  - Single `Add File` entry → `"Write"`, `input_json`: `{"file_path": path, "content": added_lines}` (terminal `structured_json`: as for `write`).
  - Otherwise → `"Edit"`, `input_json`: `{"file_path": first_path, "structuredPatch": [hunks...]}` where hunks follow `diff.rs` `{oldStart, newStart, lines}` (keeping ` `/`+`/`-` prefixes; `Delete File` gets a visible marker hunk `oldStart:1, newStart:1, lines:["-<path> (deleted)"]` or deleted lines). Terminal `structured_json`: `None` so `diff.rs` uses the input's `structuredPatch` both during pending approval and after completion; on failure/deny emit `structured_json: None`.
  - With several files or a move, a context line names each file (the modal shows one path only).
  - If `patchText` is unparsable/empty → fall back to `"Edit"` with `{"file_path": locations[0].path ?? ""}`, plus the raw text as one context hunk so the approval is never blind.
- Any other tool (`glob`, `grep`, `webfetch`, MCP tools, etc.) → original `name` (fallback `title`), `rawInput` (fallback `{}`).

**Tool output extraction (`tool_output`):**
1. Joined `text` of `content[]` entries where `content.text` is a string;
2. Else `rawOutput.output` string or `rawOutput` as string;
3. Else `"<path>: edited"` per `content[]` item with `type == "diff"`;
4. Else `""`.
Never dump raw JSON of `rawOutput`.

**Effort mapping:**
- `Effort::Default` → `"default"` (only sent when current value differs from `"default"`, e.g. on resume).
- `Effort::Low|Medium|High|XHigh|Max` → `"low"|"medium"|"high"|"xhigh"|"max"`.
- `provider.effort_levels()` and every `ModelOption.effort_levels` → `[Low, Medium, High, XHigh, Max]`.
- `process.rs::next_config` checks `configOptions` for `id == "effort"` and only sends `session/set_config_option` when `values.contains(&want)` and `current != Some(want)`.

---

## 4. Adapter design (`crates/opencode`)

Mirror `crates/antigravity` file by file:

```
crates/opencode/
  Cargo.toml            # package xade-opencode, version = "0.6.0", pinned deps matching antigravity
  src/lib.rs            # pub use provider::OpenCodeProvider;
  src/rpc.rs            # JSON-RPC 2.0 line framing (copied from antigravity, adapted comments)
  src/translate.rs      # pure Translator state machine + Claude-shaped tool mapping + child-frame filtering
  src/patch.rs          # `patchText` parser -> Write content / Edit hunks
  src/models.rs         # run & parse `opencode models --standalone` -> Vec<ModelOption> + default model pick
  src/process.rs        # spawn `opencode acp` (with Auto OFF OPENCODE_CONFIG_CONTENT), version guard, lazy session/new, steer via _session/steering, synthetic background-subagent turn, cancel, shutdown
  src/provider.rs       # OpenCodeProvider: binary lookup, preflight, models, spawn, delete_session
  scripts/record.py     # redacted fixture recorder (temp XDG_CONFIG_HOME, sandbox outside $HOME, production permission env, deletes created sessions)
  fixtures/*.ndjson     # recorded fixtures
  tests/fixtures.rs     # replays fixtures through Translator
  tests/fake_opencode.py# stub binary replaying fixtures + handling `models --standalone` and `session delete --standalone`
  tests/fake.rs         # process + provider integration tests (no network, no quota)
  tests/e2e.rs          # #[ignore], XADE_E2E_OPENCODE=1 real-opencode tests (deletes created sessions)
```

### Key `process.rs` state & invariants
- `phase: Handshaking | NeedSession | CreatingSession | Ready | Closing`.
- `active_prompt: Option<u64>`: at most one `session/prompt` in flight at a time.
- `interrupted: bool`: set on `Interrupt` while `active_prompt.is_some()`; cleared when `active_prompt` finishes.
- `steer_epoch: u64`: incremented on `Interrupt`. `Pending::Steer { text, epoch }` ignores its response if `epoch != self.steer_epoch` (a late `promptRequired`/error after Stop never starts a turn).
- `queued: VecDeque<String>`: prompts received before `Ready`, prompts sent *after* Stop while the cancelled turn unwinds, or prompts deferred from a non-invalidated steer fallback.
- **Draining `queued` on `Pending::Prompt` resolution:**
  - While steers of the current epoch are unanswered, hold the prompt's answer (`active_prompt` stays set) and process it after the last steer answer, so a refused steer chains instead of following a `TurnComplete`. Stop processes a held answer at once.
  - Clear `active_prompt`; unless the prompt ended cleanly (`end_turn`, not interrupted), answer pending permissions `cancelled`.
  - A clean end with a permission still pending (a subagent's) and nothing queued: no `TurnComplete`; the turn continues as a synthetic one, so the request stays on screen. OpenCode sends one permission request at a time (verified with two parallel shell calls).
  - If `!self.queued.is_empty() && !self.closing()`: pass `last = false` to `translator.on_prompt_result` (sums usage, flushes text, emits NO `TurnComplete` yet!), reset `interrupted = false`, and immediately send `queued.pop_front()` as the next `session/prompt`. (Prevents `SessionActor` from going `Idle` and triggering `apply_pending` respawn or dropping queued work.)
  - Else (`queued` empty or closing): pass `last = true` to `translator.on_prompt_result` (emits `TurnComplete`), reset `interrupted = false`.
- **Synthetic turns (background subagents):**
  - Track `in_flight_tools: HashSet<String>` (added on non-terminal tool update while a turn is in progress, never from an update trailing a turn nor from a permission request, whose call may get no update at all; removed on terminal tool update; cleared on turn end / interrupt) + `synthetic_turn: bool` + `synthetic_deadline: Option<tokio::time::Instant>` (default quiet period `5 s`, overridable for fast unit/fake tests via an internal test helper).
  - When a translated event (`TextDelta`, `ThinkingProgress`, `AssistantText`, `ToolUse`, `PermissionRequest`; a lone `ToolResult` opens nothing, as in the actor) arrives while `active_prompt.is_none()` and `phase == Ready`: enter `synthetic_turn = true`.
  - Whenever `synthetic_turn`: if `permissions.is_empty() && in_flight_tools.is_empty()`, set `synthetic_deadline = Some(now + quiet_timeout)`; else set `synthetic_deadline = None`.
  - When `synthetic_deadline` fires: clear `synthetic_turn`/`synthetic_deadline`/`in_flight_tools`, flush text, emit `TurnComplete` carrying latest context figures (`is_error: false, interrupted: false`).
  - On `Interrupt` while `synthetic_turn`: cancel pending permissions, send `session/cancel`, clear `synthetic_turn`/`synthetic_deadline`/`in_flight_tools`/`queued`, flush text, emit `TurnComplete { is_error: false, interrupted: true }`.
  - On `SendPrompt(text)` while `synthetic_turn`: clear `synthetic_turn`/`synthetic_deadline` and send normal `session/prompt(text)`.
- **Auto mode env (`OPENCODE_CONFIG_CONTENT`):**
  - On `spawn_session` with `!cfg.auto_mode`: build the `ask` rules object (top-level `permissions` + `agents.{build,explore,general}.permissions`), merge/append onto existing `std::env::var("OPENCODE_CONFIG_CONTENT")` if present (returning `Err(ProviderError::Protocol(...))` if existing value is invalid JSON or not an object), and pass via `cmd.env("OPENCODE_CONFIG_CONTENT", merged)`.
  - On `spawn_session` with `cfg.auto_mode`: do NOT touch `OPENCODE_CONFIG_CONTENT`.
  - Auto OFF also sets `configId: "mode"` to `build` when the session is in another agent: a custom default agent's own `allow` rules would run tools unasked (verified). A refused change ends the session.
  - Stop stays in force for permission requests arriving with no prompt running until the next prompt is sent.
- **Unexpected child exit:** if mid-turn (`active_prompt.is_some()` || `synthetic_turn` || `!queued.is_empty()`), flush text, emit `Error` with stderr tail, emit `TurnComplete { is_error: true, interrupted: false }`, then `Exited { code }`.
- **Shutdown:** close stdin, wait 3 s, SIGTERM process group, wait 2 s, SIGKILL; post-reap SIGKILL on process group (never kill `opencode` by name).

### `delete_session(id)` in `provider.rs`
- Validate `id` against `[A-Za-z0-9_-]{1,128}` and `!id.starts_with('-')`.
- Spawn `opencode session delete --standalone <id>` via `run_opencode` (`current_dir("/")`, never `/tmp`) with 10 s timeout, stdin null, capturing stdout/stderr.
- Exit 0, OR stderr/stdout containing `"Session not found"` (case-insensitive) → `Ok(())`.
- Timeout or other error → `Err(ProviderError::Protocol(...))`.

---

## 5. Phases (executed sequentially on `feat/opencode-provider`, one commit per phase)

Workflow per phase:
1. `#sonnet-1` implements the phase, runs `cargo fmt`, `cargo clippy --workspace -- -D warnings`, and phase tests, commits with `opencode: P<n> <summary>` + `Co-Authored-By` trailer, and broadcasts completion to `#sonnet-2` and `#neon-1`.
2. `#sonnet-2` runs a contrarian review of the phase diff & tests (goal: zero production bugs), and broadcasts findings (or clean sign-off) to `#neon-1` and `#sonnet-1`.
3. `#neon-1` reviews findings, fixes anything necessary on the same branch (new fix commit if edits were made), verifies tests/clippy/fmt green, and broadcasts green light for the next phase.

### P1 — Crate scaffold, recorder, recorded fixtures, `rpc.rs`, `models.rs`, `translate.rs` (not wired)
**Goal:** Prove the OpenCode ACP protocol with redacted live fixtures and a fully tested pure Translator + model parser.
**Tasks:**
1. Add `crates/opencode` to workspace `members` in `Cargo.toml`; create `crates/opencode/Cargo.toml` (version `"0.6.0"`, exact pinned deps matching `crates/antigravity/Cargo.toml`).
2. Write `crates/opencode/scripts/record.py` mirroring `crates/antigravity/scripts/record.py`:
   - Runs `opencode acp` in a sandbox outside `$HOME` (refuses sandboxes under `$HOME`), with temp `XDG_CONFIG_HOME` (keeps user plugins/config out while preserving auth in `XDG_DATA_HOME`), and with the production Auto OFF `OPENCODE_CONFIG_CONTENT` env for permission scenarios.
   - Uses `opencode-go/glm-5.3-flash` by default (and `opencode-go/gpt-6-luna` for `patch`).
   - Redacts `$HOME`, username, and emails; drops `available_commands_update`; trims the 77-model `configOptions.model.options` array to a compact representative subset.
   - Always deletes every created session afterwards (`session/delete` + fallback `opencode session delete --standalone <id>`). Never kills `opencode` by name.
3. Record these redacted fixtures in `crates/opencode/fixtures/`:
   - `simple_turn`
   - `two_turns`
   - `thinking` (`effort: "high"`)
   - `tool_allow` (Auto OFF `shell` command → `session/request_permission` → `once`)
   - `tool_deny` (Auto OFF `shell` command → `session/request_permission` → `reject`)
   - `edit` (`write` + overwrite `write` + `edit` in sandbox, verifying `diff` block shape)
   - `patch` (`opencode-go/gpt-6-luna` using `patch` tool: add + update + delete if triggerable)
   - `interrupt` (`session/cancel` mid-stream, then follow-up prompt in same process)
   - `interrupt_pending_permission` (`session/cancel` while `shell` permission is pending)
   - `steer` (`_session/steering` mid-turn → `outcome: "injected"`)
   - `resume` (two processes: `session/new` + secret word, restart, `session/resume` + recall)
   - `set_config` (`model` + `effort`)
   - `bad_model` (`set_config_option` unknown model → `-32602`)
   - `bad_resume` (`session/resume` nonexistent id → `-32602`)
   - `subagent` (subagent / child-session updates carrying `_meta["opencode/child-session"]`)
   - `background_subagent` (background subagent emitting frames after parent `end_turn`, if cleanly recordable; else synthetic fixture in `tests/fake/` plus unit test)
4. Implement `src/lib.rs`, `src/rpc.rs`, `src/models.rs`, `src/translate.rs` per §3 with thorough unit tests:
   - `status:"pending"` NEVER emits `ToolUse` (even with `rawInput: {"cwd": "..."}`); `tool_call_update` without `status` is non-terminal; `tool_call_update` merges cached `name` and emits mapped `Bash`/`Read`/`Write`/`Edit`.
   - `write` create (`oldText: null`) vs overwrite (`oldText: string`) `structured_json`; `edit` `old_string`/`new_string`; `patch` parser (single Add → `Write`, Update/Move/Delete/multi-file → `Edit` with `structuredPatch` hunks); failed/denied edit/write/patch → `structured_json: None`.
   - Child-session frames (`_meta["opencode/child-session"]`) skip message/thought/usage updates and map tool calls normally.
   - Permission request on pending/unseen tool emits mapped `ToolUse` + `PermissionRequest` with matching `tool_name` and `input_json`.
   - Text flushed before thought, tool call/result, permission request, and turn end.
   - `last == false` sums usage without emitting `TurnComplete`; `cancelled` → `interrupted: true, is_error: false`; unknown stop reason → `Error` + `is_error: true`.
   - `models.rs` parses `opencode models --standalone`, populates `effort_levels = [Low..Max]` on every model, picks default model per §2.5 (skipping `free`/`contributor`/`preview`), and detects auth errors.
5. Write `tests/fixtures.rs` replaying every recorded fixture's `out` frames through `Translator` and asserting exact key event sequences.
**Acceptance:**
- `grep -ri "$USER\|/home/" crates/opencode/fixtures` finds nothing.
- `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test -p xade-opencode` green.
- No leftover recording sessions in `opencode session list --standalone`.

### P2 — `process.rs`, `provider.rs`, fake tests, and real-`opencode` e2e tests (still not wired)
**Goal:** Complete, verified `OpenCodeProvider` passing both deterministic fake tests and live `opencode` e2e tests.
**Tasks:**
1. Implement `src/process.rs` per §4:
   - Auto OFF `OPENCODE_CONFIG_CONTENT` builder/merger (unit-tested: fresh env, appending to existing JSON, rejecting malformed JSON, top-level + `agents.{build,explore,general}.permissions`, never injecting `allow`). Auto ON leaves env untouched.
   - `initialize` version check (`major < 2` → fail with update message) → lazy `session/new` (or `session/resume`) on first prompt → `Started` → `set_config_option model` → `set_config_option effort` (including `"default"` on resume when current differs) → drain `queued`.
   - Mid-turn steer via `_session/steering` with `steer_epoch` invalidation on `Interrupt` and fallback on `promptRequired`/error; post-Stop prompts queue until cancelled turn resolves; non-empty `queued` on prompt resolution chains without emitting premature `TurnComplete`.
   - Synthetic turn + quiet timer for background subagents (held while permissions pending or tools in-flight; Stop cancels and emits interrupted `TurnComplete`; `SendPrompt` takes over).
   - `Interrupt` before Ready drops queue + emits interrupted `TurnComplete`; mid-turn clears `queued`, bumps `steer_epoch`, answers pending permissions `cancelled`, sends `session/cancel`; late permissions after Stop answered `cancelled` immediately.
   - `SetAutoMode(true)` auto-allows pending permissions.
   - Unknown server requests answered `-32601`.
   - Clean shutdown (close stdin 3 s → SIGTERM 2 s → SIGKILL) + post-reap `killpg(SIGKILL)` + mid-turn unexpected exit emits `Error` + error `TurnComplete` + `Exited`.
2. Implement `src/provider.rs` with inherent methods in P2 (`preflight`, `models`, `default_model`, `effort_levels`, `account_label`, `spawn`, `delete_session`); `impl AgentProvider` is added in P3 when `ProviderId::OpenCode` is introduced:
   - Binary lookup (`$PATH`, `~/.opencode/bin`, `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`) with `find_binary` retry when missing at startup.
   - `preflight()` via `opencode models --standalone` (15 s timeout).
   - `delete_session()` via `opencode session delete --standalone <id>` (id validation + `"Session not found"` tolerance).
3. Write `tests/fake_opencode.py` + `tests/fake.rs` (no network, no quota):
   - Prompt queued during handshake; lazy `session/new` (no prompt = no session).
   - OpenCode v1 `agentInfo.version` rejected at `initialize`.
   - `model` then `effort` set in order; resume resets effort to `"default"` when `Effort::Default`; bad model non-fatal.
   - `tool_allow` (Auto OFF allow/deny, Auto ON auto-allow, `SetAutoMode(true)` allows pending).
   - `interrupt` before Ready; `interrupt` mid-turn clears `queued` & invalidates pending steer; `interrupt` with pending permission; late permission after Stop answered `cancelled`; prompt after Stop queues and runs without premature `TurnComplete`.
   - `steer`: `_session/steering` `injected` yields one `TurnComplete`; `promptRequired` falls back to `session/prompt`; `promptRequired` after `Interrupt` is ignored.
   - Background subagent synthetic turn: frames after `end_turn` stay open while tool/permission active, finish after quiet timer (short timer in test), Stop cancels cleanly, SendPrompt takes over.
   - `resume` + `bad_resume`.
   - Unknown server request answered `-32601`.
   - Unexpected child exit mid-turn emits `Error` + error `TurnComplete` + `Exited`.
   - Shutdown reaps child cleanly.
   - `preflight` (models, auth error, offline error, empty) and `delete_session` (ok, not found, invalid id).
4. Write `tests/e2e.rs` (`#[ignore]`, gated by `XADE_E2E_OPENCODE=1`, sandbox in `mktemp -d` outside `$HOME`, model `opencode-go/glm-5.3-flash`, drop guard deletes every created session via `delete_session`):
   - Simple turn + follow-up turn in same process.
   - Auto OFF: `shell` triggers `PermissionRequest` even with a project `opencode.json` allowing `shell`; allow runs it, deny stops it.
   - Auto ON: a project `opencode.json` `deny` rule stays denied (proving Auto ON never weakens user `deny`).
   - Interrupt mid-stream + follow-up prompt works in same process.
   - Resume in new process recalls context.
   - Shutdown leaves no child under our spawned PID (never kill `opencode` by name).
**Acceptance:**
- `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test -p xade-opencode` green.
- `XADE_E2E_OPENCODE=1 cargo test -p xade-opencode --test e2e -- --ignored --test-threads=1` passes on this machine and leaves no stray sessions.

### P3 — Wire `ProviderId::OpenCode`, `respawn_on_auto_change`, and `OpenCodeProvider` across workspace and desktop UI
**Goal:** OpenCode is a first-class selectable provider alongside Claude, Antigravity, and Codex everywhere in Xade.
**Known sites (fix every exhaustive match the compiler reports):**
1. `crates/core/src/ids.rs`: add `ProviderId::OpenCode` (serializes `"opencode"`).
2. `crates/core/src/provider.rs`: add defaulted method `fn respawn_on_auto_change(&self) -> bool { false }`.
3. `crates/runtime/src/session_actor.rs`: add `pending_auto: Option<bool>`; in `set_auto(on)`, when `self.provider.respawn_on_auto_change()` is true, store `self.pending_auto = Some(on)` and call `self.apply_pending().await` (respawns immediately when `Idle`, or at `TurnComplete` after the running turn finishes). Add runtime unit test covering both idle and mid-turn Auto toggle respawn (and confirming providers with `false` do NOT respawn).
4. `crates/opencode/src/provider.rs`: `impl AgentProvider for OpenCodeProvider` (returning `respawn_on_auto_change() -> true`).
5. `crates/store/src/schema.rs`: `provider_str` / `parse_provider` gain `"opencode"` (no DB migration); add mixed-provider store test.
6. `crates/runtime/src/handoff.rs`: `provider_name` / `parse_provider` gain `ProviderId::OpenCode` → `"opencode"`.
7. `crates/runtime/src/manager.rs`: `SessionOrigin` match arm (`ProviderId::OpenCode` alongside non-importing providers).
8. `crates/runtime/src/multi.rs`: `team_preamble` label `"OpenCode"`.
9. `apps/desktop/Cargo.toml`: add `xade-opencode = { version = "0.6.0", path = "../../crates/opencode" }`.
10. `apps/desktop/src/lib.rs`: register `Arc::new(OpenCodeProvider::new())` in `backend()` after Codex (startup still waits only for Claude; OpenCode preflights in background).
11. `apps/desktop/src/conversation.rs`: `symbol_tint` is `theme::text()`; `company_symbol` and `cli_symbol` return the neutral mark for `ProviderId::OpenCode`. Update `symbol_tests`.
12. `apps/desktop/src/home.rs`: `provider_color` → `theme::text_muted()`.
13. `apps/desktop/src/widgets/delete_modal.rs`: `history` → `"OpenCode history"` + test.
14. `apps/desktop/src/guide.rs`: add `ProviderId::OpenCode` setup row (`name: "OpenCode"`, `install: "curl -fsSL https://opencode.ai/v2/install | bash"`, `sign_in: "opencode auth login"`, `docs: "https://opencode.ai/docs"`).
15. `apps/desktop/src/app.rs`: add `StubProvider::opencode()` and extend multi-provider unit tests (4 providers, hiding `NotInstalled`, picking OpenCode).
16. Audit all hard-coded provider lists (`working.rs`, `home.rs`, `workspace.rs`, `app.rs`) and confirm picker, status bar, Status modal, Setup guide, Fork menu, and Multi-Session work cleanly with 4 providers.
**Acceptance:**
- `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`, and `cargo test -p xade --test e2e` all pass.

### P4 — Docs, wiki, and final verification
**Goal:** Keep wiki and user docs accurate, minimal, and grounded in recorded fixtures.
**Tasks:**
1. Write `wiki-llm/opencode-protocol.md` mirroring `antigravity-protocol.md` / `codex-protocol.md` (facts only, every protocol fact naming its fixture).
2. Update `wiki-llm/index.md` (add `opencode-protocol.md` row and add it to "Adding a provider"), `wiki-llm/architecture.md` (crate list + dependency rule + `respawn_on_auto_change`), `wiki-llm/conventions.md` (dependency rule + credentials line mentions `opencode`), `wiki-llm/testing.md` (recorder + `XADE_E2E_OPENCODE` line).
3. Add `docs/opencode.md` (short user doc mirroring `docs/codex.md`: install/sign-in, OpenCode >= 2.0 requirement, free models vs Go/connected providers, Auto OFF gating — including that user `deny` rules prompt instead of silent block under Auto OFF and MCP tools stay ungated — Auto ON preserving user `deny`, background subagent Stop/close/respawn note including silent-child residual, delete, fork & Multi-Session, no import/quota in v1).
4. Update `docs/provider-protocols.md` (add OpenCode column) and `README.md` (mention OpenCode next to Claude Code, Antigravity, and Codex, linking `docs/opencode.md`).
**Acceptance:**
- All links resolve; no doc restates code/types/copy.
- `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`, `cargo test -p xade --test e2e` all green.
- Re-run `XADE_E2E_OPENCODE=1 cargo test -p xade-opencode --test e2e -- --ignored --test-threads=1` to confirm real-CLI integration is clean.

---

## 6. Corner cases checklist

- [ ] `tool_call` (`status:"pending"`, `{}` or `{"cwd":...}`) never emits `ToolUse`; `tool_call_update` without `status` is non-terminal; `tool_call_update` (`in_progress`, no `name`, real `rawInput`) merges cached `name` and emits mapped `Bash`/`Read`/`Write`/`Edit`.
- [ ] `patch` tool (`gpt-6-luna`): single Add → `Write`; Update/Move/Delete/multi-file → `Edit` with `structuredPatch` hunks (Delete has visible marker hunk).
- [ ] `write` create (`oldText: null`) vs overwrite (`oldText: string`) `structured_json`; failed/denied `edit`/`write`/`patch` → `structured_json: None`, `is_error: true`.
- [ ] Child-session frames (`_meta["opencode/child-session"]`): text/thought/usage skipped; tool calls mapped.
- [ ] Background subagent frames after parent `end_turn` → synthetic turn with quiet timer (held while tools/permissions active), Stop cancels cleanly, SendPrompt takes over.
- [ ] Auto OFF injects `OPENCODE_CONFIG_CONTENT` `ask` rules (top-level + `agents.{build,explore,general}`) overriding project `allow`; appends to existing env JSON; fails spawn on invalid env JSON. Auto ON leaves env untouched so user `deny` stays `deny`.
- [ ] OpenCode major version `< 2` rejected at `initialize`.
- [ ] Permission request with numeric `id` (`0`) and string `id` (`"1"`) both round-trip original JSON type.
- [ ] Prompt queued during `initialize` / `session/new` / `set_config_option` runs in order after config settles.
- [ ] No prompt sent → no `session/new` sent (zero sessions created in OpenCode).
- [ ] Stop before Ready → queue dropped, interrupted `TurnComplete`, no `session/new`.
- [ ] Stop mid-turn → clears `queued`, invalidates in-flight steer (`steer_epoch`), answers pending permissions `cancelled`, then `session/cancel`.
- [ ] Late permission after Stop → answered `cancelled` immediately, never shown, never auto-allowed.
- [ ] Prompt sent after Stop while turn unwinds → queued until cancelled turn finishes, never steers it, chains without premature `TurnComplete`.
- [ ] Mid-turn prompt → `_session/steering`; `outcome:"injected"` yields single `TurnComplete`; `outcome:"promptRequired"` or RPC error (when not invalidated by Stop) falls back to `session/prompt`.
- [ ] `SetAutoMode(true)` in an Auto-OFF-spawned process does NOT auto-allow (protects user `deny` rules until idle respawn); `SessionActor` respawns on idle when Auto changes (`respawn_on_auto_change() == true`).
- [ ] Unknown server request → answered `-32601` immediately.
- [ ] Model change updates valid `effort` values; resume with `Effort::Default` sends `"default"` when current differs; unsupported effort is skipped; bad model is non-fatal `Error`.
- [ ] `session/resume` of unknown id → `Error`, failed turn, clean shutdown.
- [ ] Unexpected child exit mid-turn (or synthetic turn / queued) → flushed text, `Error` with stderr tail, error `TurnComplete`, `Exited`.
- [ ] `preflight` and `delete_session` use `--standalone`; `delete_session` rejects invalid/flag-like ids and tolerates `"Session not found"`.
- [ ] `opencode` not installed → hidden in UI when another provider is available.

---

## 7. Execution protocol (team rules)

- Single shared branch: `feat/opencode-provider`. Never touch `main`, never push unless the user asks.
- File ownership: while `#sonnet-1` implements P`n`, only `#sonnet-1` edits workspace files. During review, `#sonnet-2` is read-only (runs checks/tests if needed without leaving dirty files). After `#sonnet-2` reports, `#neon-1` applies any needed fixes and commits before signaling P`n+1`.
- Every commit includes a blank-line-separated `Co-Authored-By: <Exact Model Name> <noreply@ai-agent.local>` trailer.
- If any verified fact in §2 differs during live fixture recording, update §2 in the same phase and note it in the broadcast.

---

## 8. Out of scope for v1 (follow-ups)

- Importing past OpenCode-native sessions (`session/list` + `session/load` / `opencode session export`).
- Running-sessions detection (`running_sessions()`).
- Quota meter (`quota()`).
- Live model/effort change without respawning the ACP process.
- Exposing OpenCode's `plan` mode (v1 stays on `build`).
- Gating MCP tools (`<server>_<tool>`) under Auto OFF (using `*_*` would also match `external_directory` and break fork snapshots reading `/tmp/xade-forks-<uid>`).
- Keeping background subagents alive across session close or Auto/model/effort respawn (documented); also note that while a background child works silently with no active turn, there is no Stop button and only closing the session kills it.
- Gating custom **subagents** whose own rules `allow` guarded actions: their names are unknown at spawn.
