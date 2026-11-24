# Xade — Antigravity CLI Integration Plan (MVP)

> Goal: Xade runs sessions on **Claude Code** (`claude`) and **Antigravity** (`agy`) side by side. Any session can be **forked to another provider** and continue there with its history.
> Written to be executed phase by phase by Sonnet 5.5 sub-agents (`model: sonnet`). Opus reviews each phase's diff before the next one starts. Wiki prose goes to Haiku (`model: haiku`).
> Rules: `wiki-llm/conventions.md` wins over this plan. If they conflict, stop and ask; do not pick one yourself.
> Iterative: this is v1. Items in §9 are deliberately out of scope.

---

## 0. TL;DR of decisions

| # | Decision | Why |
|---|---|---|
| D1 | Integrate via **ACP**: `agy acp`, JSON-RPC 2.0 over stdio, one process per Xade session. Do **not** use `agy -p --input-format stream-json`. | Headless stream-json has **no approval channel**: permission prompts are soft-denied, and a `control_*` frame on stdin ends the session with exit 2. ACP has `session/request_permission`, `session/cancel`, resume, and model and thinking-level options. All of this was verified live against agy 1.5.16 (§2). |
| D2 | New crate **`xade-antigravity`**, a sibling of `xade-claude`. It depends only on `xade-core`. Only `apps/desktop` names it. | Same dependency rule as today: `xade-core ← xade-claude, xade-antigravity, xade-store ← xade-runtime ← xade`. |
| D3 | **No change to the `AgentProvider` trait, `ProviderEvent`, or `SessionCommand`.** Add one enum variant, `ProviderId::Antigravity`. | Keeps the change non-breaking. Everything ACP-specific is translated inside the adapter. |
| D4 | Model change and effort change keep the runtime's existing path: the actor respawns with `resume`. ACP *can* change them live, but v1 does not use that. | One code path for both providers; zero runtime changes for this. Optimize later (§9). |
| D5 | Auto mode = the adapter answers `session/request_permission` with the allow option itself. The ACP mode stays `default`. | Same semantics as the Claude adapter. No dependence on `auto` mode, which needs the sandbox and is unavailable on business accounts. |
| D6 | The ACP session is created **lazily**, on the first prompt. The process and `initialize` start eagerly. | `session/new` persists a ~400 KB conversation even when no prompt is sent (verified). Opening a window must not litter the user's Antigravity history. |
| D7 | **Fork = copy the Xade messages into a new Xade session for the target provider, plus a one-time transcript hand-off** prepended to that session's first prompt. The hand-off uses the same `context` mechanism as the IDE-context tag. | Works for every provider pair with one code path, survives restarts, and needs no schema change. |
| D8 | No import of Antigravity-native sessions, and no Antigravity quota meter, in v1. | ACP exposes neither quota nor cheap history. Kept out to stay small (§9). |

---

## 1. Constraints (read before any phase)

- **No breaking changes.** Existing Claude sessions, the DB, `settings.json`, shortcuts and behavior must work unchanged. No DB migration is needed: `sessions.provider_id` is free `TEXT`. If a phase seems to need a breaking change, stop and report.
- **Dependency rule:** `xade-antigravity` → `xade-core` only. `xade-runtime` never names a concrete provider. Only `apps/desktop` (`main.rs`) constructs providers.
- **Hard invariants** (`conventions.md`) apply to `agy` exactly as they do to `claude`:
  - Spawn the binary unmodified.
  - Never read `~/.gemini` credentials or the keyring. Never read `settings.json` from `~/.gemini`.
  - Log no prompt text above `debug`.
  - Skip unknown frames; never treat them as fatal.
  - Reap every child.
- **Dependencies:** ask before upgrading or swapping a crate. Adding a crate is fine; pin it `=x.y.z`. Reuse the exact versions `xade-claude` already pins (`serde`, `serde_json`, `tokio`, `libc`, `tracing`, `uuid`, `time`, `tempfile`). No JSON-RPC crate: the framing is small enough to hand-write.
- **Done** means: `cargo fmt`, `cargo clippy --workspace -- -D warnings` and `cargo test --workspace` are clean, the phase's Acceptance passes, and the wiki is updated only where a flow or contract changed.

---

## 2. Verified ACP protocol facts (agy 1.5.16, probed 2026-11-24)

These are facts, not guesses. Phase 1 turns each one into a fixture plus a test. Anything marked **(verify)** was not observed and must be confirmed while recording; if it differs, adjust and note it in the fixture's test.

### 2.1 Process

- Command: `agy acp`, `cwd` = the session's working directory. No other flags. Logs go to `~/.gemini/antigravity-cli/log/acp-*.log`; Xade never reads them.
- Closing stdin makes the process exit 0. This was observed both while idle and while a `session/request_permission` was unanswered.
- Binary lookup order:
  1. `$PATH` (on this machine that resolves to a mise shim);
  2. `~/.gemini/bin/agy` (the default install, verified present);
  3. `~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`.
- Requests from Xade: one JSON object per line, `{"jsonrpc":"2.0","id":<int>,"method":…,"params":…}`. Notifications from Xade carry no `id`.
- Unknown method sent to agy → `{"error":{"code":-32601,"message":"Method not found: …"},"id":…}`.

### 2.2 Handshake

- `initialize` params: `{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}`.
- The `initialize` result has:
  - `protocolVersion: 1`;
  - `agentCapabilities.loadSession: true`, `sessionCapabilities: {resume, close, load, list, additionalDirectories}`;
  - `promptCapabilities: {image:true, audio:false, embeddedContext:true}`;
  - `agentInfo: {name:"Antigravity ACP Server", version}`;
  - `authMethods[]`.
- `session/new` params: `{"cwd":<abs path>,"mcpServers":[]}`. The result has:
  - `sessionId` (a UUID);
  - `models{availableModels[{modelId,name}],currentModelId}`;
  - `modes{currentModeId:"default",availableModes[]}`;
  - `configOptions[]`. Each option is `{id, category, type:"select", currentValue, options[{value,name}]}`, with ids `model`, `thought_level`, `mode`.
- After `session/new` comes an `available_commands_update` notification (large, ignore it) and a `usage_update {used:0,size:1048576}`.
- `session/resume` params: `{"sessionId","cwd","mcpServers":[]}`. Per the docs it reopens the session without replaying history **(verify the result shape and that no `*_chunk` replay arrives)**.
- `session/load` with the same params **replays the whole history** as `user_message_chunk`, `agent_message_chunk`, `agent_thought_chunk` and `tool_call` (`status:"completed"`, with `rawOutput`) before its result. Xade does not use `load` in v1.
- `session/set_config_option` params: `{"sessionId","configId":"model"|"thought_level","value"}`. The result is `{configOptions}`, followed by a `config_option_update` notification. A wrong sessionId → `-32602 "Session not found: "`.
- Model option values are **base ids**: `gemini-4-neon`, `gemini-3.8-flash`, `claude-opus-5-5`, `gpt-oss-120b-medium`, `gemini-pro-orchestrator`, …
- `thought_level` values: `low`, `medium`, `high`, `xhigh` (no `max`). The default `currentValue` is `high`.
- Without a login, ACP requests fail with `-32000 "Authentication required"` (docs) **(verify where: initialize or session/new)**.

### 2.3 A turn

- `session/prompt` params: `{"sessionId","prompt":[{"type":"text","text":…}]}`. The result is `{"stopReason":"end_turn"|"cancelled"|…,"usage":{inputTokens,outputTokens,cachedReadTokens,cachedWriteTokens,thoughtTokens,totalTokens}}`.
- `usage` is **per turn, not cumulative** (observed 17102, then 2210, then 2111 input tokens).
- The turn streams `session/update` notifications. `params.update.sessionUpdate` is one of:
  - `agent_message_chunk {content:{type:"text",text}}`: a text delta.
  - `agent_thought_chunk {content:{type:"text",text}}`: thinking text, which agy does expose.
  - `tool_call {toolCallId,name,args,rawInput,status:"in_progress"|"pending",kind:"read"|"edit"|"execute"|…,title,locations[]}`.
  - `tool_call_update`: the same fields, plus `status:"pending"|"in_progress"|"completed"` and, when done, `rawOutput` and `content[]`. Edit tools carry `content:[{"type":"diff","path","oldText","newText"}]`. Commands carry `rawOutput:{output,exit_code}` and `content:[{"type":"content","content":{"type":"text","text"}}]`.
  - `usage_update {used,size}`: context tokens used and the window size.
  - `session_info_update {title}`: an auto title. Ignored in v1.
  - `available_commands_update`, `config_option_update`, `user_message_chunk`: ignore.
- Tool names seen: `read_file`, `edit_file`, `run_command`. In `default` mode, workspace reads and edits were **not** gated, while `run_command` asked for permission. **Consequence:** with Auto OFF, agy pauses on commands, URLs and out-of-workspace access, not on workspace edits. Document this; do not try to change it.
- **Permission request** (agy → Xade, a JSON-RPC *request* with an id that may be a **string**, e.g. `"1"`):
  - Request: `{"method":"session/request_permission","params":{"sessionId","toolCall":{toolCallId,name,rawInput,…},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"deny","name":"Deny","kind":"reject_once"}]},"id":"1"}`.
  - Reply: `{"jsonrpc":"2.0","id":"1","result":{"outcome":{"outcome":"selected","optionId":"allow"}}}`.
  - It arrives after a `tool_call_update` with `status:"pending"`.
- **Cancel:** the notification `session/cancel {sessionId}`. The outstanding prompt then resolves with `stopReason:"cancelled"` and that turn's usage. The process stays alive and the next prompt works in the same process.
- **Steer** (critical): sending a second `session/prompt` while one is running makes agy resolve the **first** prompt immediately with `stopReason:"end_turn"`, while the turn keeps going (more tool calls and a permission request followed). The **last** prompt's response ends the turn. Rule: *the turn ends when no prompt request is outstanding.*

### 2.4 Not available via ACP

- Quota windows and cost.
- An auth-status command.
- Fork (it exists only as the TUI `/fork`).
- Effort `max`.

---

## 3. Domain mapping (adapter contract)

| ACP | Xade (`ProviderEvent` / behavior) |
|---|---|
| `session/new` or `session/resume` succeeded | `Started{provider_session_id: sessionId, model_id: <session's configured model or the config's currentValue>}`, emitted once per process. |
| `agent_message_chunk` | Append to the text buffer, then `TextDelta(text)`. |
| Text buffer non-empty, then any of: `agent_thought_chunk`, `tool_call`, a permission request, turn end (any stopReason, including cancelled, or an error) | **Flush** first: `AssistantText{text: buffer}`, then clear the buffer. Finalized history must equal what the user saw streaming. |
| `agent_thought_chunk` | `thought_chars += text.chars().count()`, then `ThinkingProgress{estimated_tokens: thought_chars / 4}`. Reset at turn end. Thinking text is never stored (same as Claude today). |
| `tool_call` (first sight of a `toolCallId`) | `ToolUse{tool_use_id: toolCallId, name, input_json: rawInput (fall back to args, then "{}")}`. Remember the id. |
| `tool_call_update` with an id never seen | Synthesize the `ToolUse` first, then handle the update as below. |
| `tool_call_update` with `status` ∈ {`pending`, `in_progress`} | Nothing. |
| `tool_call_update` with `status` = `completed`, or any other terminal status (`failed`, `cancelled`, unknown non-`pending`/`in_progress`) | `ToolResult{tool_use_id, output, is_error: status != "completed", structured_json: Some(serde_json of {"content": content}) when content is present}`. Emit once per id; ignore later updates for it. |
| ToolResult `output` | `rawOutput.output` if it is a string; else the joined `text` of `content[].content` blocks; else, for `diff` blocks, `"<path>: edited"` lines; else `""`. Do not dump `rawOutput` JSON. |
| `usage_update{used,size}` | Store as the current context (`context_tokens_used = used`, `context_window_max = size`). No event. |
| `session/request_permission`, Auto ON | Reply at once with the allow option (§4.4). No event. |
| `session/request_permission`, Auto OFF | `PermissionRequest{request_id: <id as a string key>, tool_use_id: toolCall.toolCallId, tool_name: toolCall.name, input_json: toolCall.rawInput}`. Remember `key → original JSON id value + options`. |
| `session/prompt` result, while other prompts are still outstanding | Add its usage to the turn's usage; no event. |
| `session/prompt` result for the last outstanding prompt | Flush text, then `TurnComplete{usage, is_error, interrupted}`. `interrupted = (stopReason=="cancelled") \|\| interrupt_flag`. `is_error = !interrupted && stopReason ∉ {end_turn}`; for a non-`end_turn` error, also emit `Error("Antigravity stopped: <stopReason>")`. |
| `TurnComplete.usage` | `input_tokens = Σ inputTokens`, `output_tokens = Σ outputTokens`, `cache_read_tokens = Σ cachedReadTokens`, `cost_usd = 0.0`, plus the context figures from the last `usage_update`. |
| JSON-RPC error on `session/prompt` | Remove it from the outstanding set, then `Error(message)`. If the set is now empty: `TurnComplete{is_error:true, interrupted:false}`. |
| Any request **from agy** with a method other than `session/request_permission` | Reply `{"error":{"code":-32601,"message":"Method not found"}}`. Never leave it unanswered: agy would block. |
| A notification whose `sessionId` differs from ours; any unknown `sessionUpdate`; an unparsable line | Skip it (`tracing::debug!`). |
| Child exit | `Exited{code}`, always the last event. |

Effort mapping: `Effort::Default` → don't set `thought_level`. `Low|Medium|High|XHigh` → `low|medium|high|xhigh`. `Max` → not offered; if a session has it, treat it as Default.

---

## 4. Adapter design (`crates/antigravity`)

Mirror `crates/claude` file by file wherever possible; read it first. The files:

```
crates/antigravity/
  Cargo.toml            # package xade-antigravity, version = workspace app version, lints.workspace = true
  src/lib.rs            # pub use provider::AntigravityProvider;
  src/rpc.rs            # JSON-RPC line types: Incoming {Request, Notification, Response}, OutFrame builders
  src/translate.rs      # pure, sync: Translator state machine, ACP JSON → Vec<ProviderEvent>
  src/process.rs        # spawn, stdin writer, stdout/stderr readers, command loop, shutdown
  src/models.rs         # parse `agy models` output → Vec<ModelOption>
  src/provider.rs       # AntigravityProvider: binary resolution, preflight, models, spawn, delete_session
  fixtures/*.ndjson     # recorded sessions (same {"dir","frame"} line format as Claude's)
  scripts/record.py     # recorder (mirror of crates/claude/scripts/record.py, incl. redaction + refusal rules)
  tests/fixtures.rs     # replays fixtures through Translator
  examples/chat.rs      # manual REPL, mirror of the claude example
  examples/parent_death.rs
```

### 4.1 `translate.rs`: `Translator` (pure, no I/O, fully unit-tested)

State:
- `text_buf: String`, `thought_chars: usize`
- `seen_tools: HashSet<String>`, `finished_tools: HashSet<String>`
- `context: (u64 used, u64 size)`, `turn_usage: UsageTotals`

API, so the I/O layer stays dumb:
- `fn on_update(&mut self, update: &Value) -> Vec<ProviderEvent>`
- `fn on_prompt_result(&mut self, result: &Value, last: bool, interrupted: bool) -> Vec<ProviderEvent>`
- `fn on_prompt_error(&mut self, msg: &str, last: bool) -> Vec<ProviderEvent>`
- `fn flush_text(&mut self) -> Vec<ProviderEvent>`

The permission request is classified in `process.rs` but must call `flush_text()` before emitting.

### 4.2 `process.rs`: the session task

One tokio task owns the child, `stdin`, and a `select!` over: the command channel, parsed stdout lines, and child exit. State:
- `next_id: u64` (our request ids start at 1).
- `pending: HashMap<u64, Pending>`, where `Pending = Initialize | NewOrResume | SetConfig | Prompt`.
- `outstanding_prompts: BTreeSet<u64>`.
- `queued: VecDeque<String>`: prompts received before the session is ready.
- `phase: Handshaking | NeedSession | CreatingSession | Ready | Closing`.
- `acp_session_id: Option<String>`.
- `auto: bool`, `interrupt_flag: bool`.
- `permissions: HashMap<String, (Value id, Vec<Option>)>`.
- `translator`.
- `stderr_tail` (copy Claude's bounded tail, used in error messages).

Lifecycle:
1. **Spawn.** `Command::new(binary).arg("acp").current_dir(cwd)` with stdin, stdout and stderr piped, `kill_on_drop(true)`, and its own process group (copy Claude's `process_group(0)` / `killpg` code). Leave the environment as is: no vars added or removed. Send `initialize` at once; phase = Handshaking.
2. **`initialize` result.** If `protocolVersion != 1` → `Error("Unsupported Antigravity ACP version")`, then go to shutdown. Else phase = NeedSession, or, if `resume` is set and a prompt is queued, go straight to step 3.
3. **First `SendPrompt`** (or at once, if prompts were queued during the handshake): send `session/resume{sessionId: cfg.resume}` if `cfg.resume` is set, else `session/new`. Phase = CreatingSession. Queue the prompt.
4. **new/resume result.**
   - Set `acp_session_id`.
   - Emit `Started`.
   - If `cfg.model_id` is non-empty and differs from the `model` option's `currentValue` → `set_config_option model`.
   - If the effort maps to a level the `thought_level` options contain → `set_config_option thought_level`.
   - **Await these responses before prompting**, since order matters: a model change can reset `thought_level`, so set the model first.
   - A set_config error is **non-fatal**: emit `Error("Antigravity rejected model <x>: <msg>")` and continue on the CLI default.
   - Phase = Ready. Drain `queued` as `session/prompt`s.
5. **new/resume error.**
   - `-32000` or a message containing "auth" → `Error("Antigravity is not signed in. Run `agy` in a terminal once to sign in.")`.
   - On resume, any other error → `Error("Could not resume the Antigravity conversation: <msg>")`.
   - Then flush the queue as a failed turn (`TurnComplete{is_error:true}`) and **shut down**, so the actor reports `Exited` with that message (the actor's `last_error` path).
6. **`SendPrompt` when Ready.** Send `session/prompt` at once, even mid-turn (that is the steer, §2.3), and insert the id into `outstanding_prompts`. Clear `interrupt_flag` only when the set was empty.
7. **`Interrupt`.**
   - If phase < Ready and prompts are queued: drop them and emit `TurnComplete{interrupted:true}` (otherwise the UI stays "Streaming" forever).
   - Else, if prompts are outstanding: set `interrupt_flag`, answer every pending permission with `{"outcome":{"outcome":"cancelled"}}` (the ACP rule: the client must answer pending requests on cancel), clear `permissions`, and send the notification `session/cancel{sessionId}`.
   - Else: no-op.
8. **`RespondPermission{request_id, allow}`.** Look up the key; if it is missing, ignore it (it was already answered or cancelled).
   - allow → the first option with kind `allow_once`, else `allow_always`, else any kind starting with `allow`.
   - deny → `reject_once`, else `reject_always`, else any `reject*`, else outcome `cancelled`.
   - Reply with the **original JSON id value** (a string stays a string).
9. **`SetAutoMode(on)`.** Store the flag. When turning on with permissions pending, auto-allow them now. (This differs from Claude, which leaves them for the user. It is harmless and avoids a stuck banner; note it in the wiki.)
10. **`Shutdown`** and parent death:
    - Same sequence as Claude: close stdin, wait 3 s, SIGTERM the group, wait 2 s, SIGKILL. Emit `Exited` last.
    - Don't send `session/close`: it is not needed, and a dead pipe would make it fail.
    - Parent death: the child gets stdin EOF and exits. Prove it with `examples/parent_death.rs`.
11. **Unexpected child exit.** Emit `Error` with the stderr tail if the exit was non-zero, then `Exited{code}`.

Writes to stdin go through one writer (a mutex or channel). Read lines with no length cap (tool outputs can be large); invalid UTF-8 or JSON lines are skipped.

### 4.3 `provider.rs`: `AntigravityProvider: AgentProvider`

- `id()` → `ProviderId::Antigravity`; `display_name()` → `"Antigravity"`.
- `preflight()` (blocking; the app calls it at startup and on Retry):
  - No binary → `NotInstalled`.
  - Else run `agy models` with a **15 s timeout**, stdin null, capturing stdout and stderr.
  - On exit 0 and ≥1 model parsed → replace the catalog, `Ok`.
  - On non-zero exit, or output that mentions "auth", "sign in" or "log in" (case-insensitive, either stream) → `NotLoggedIn`.
  - On timeout → `Protocol("agy models timed out")`.
  - Never log stdout above `debug`.
- `models()` / `effort_levels()` / `default_model()`:
  - The catalog comes from `models.rs`. The fallback catalog (before preflight) is empty, with default `""`.
  - `default_model()` = the first catalog id, else `""`.
  - `effort_levels()` = `[Low, Medium, High, XHigh]`.
- `account_label()` → `None`: there is no auth-status command, and Xade never reads the keyring.
- `spawn(cfg)` → `process::spawn_session(binary, cfg)`; `NotInstalled` if there is no binary.
- `import_sessions()` and `running_sessions()`: the trait defaults (empty) for v1.
- `delete_session(id)`:
  - Validate the id against `[A-Za-z0-9_-]{1,128}`.
  - Remove `~/.gemini/antigravity-cli/conversations/<id>.db` together with its `-wal`/`-shm` siblings, and the directory `~/.gemini/antigravity-cli/brain/<id>`. Both paths were verified to be per-conversation.
  - Never follow symlinks. A missing file is `Ok`.
  - Resolve `~` from `$HOME`; no `$HOME` → `Ok` (nothing to delete).

### 4.4 `models.rs`: parsing `agy models`

Each line is `<slug>\t<label>`. Ignore blank lines and lines without a tab (for example `Fetching available models...`). Group slugs to match the ACP `model` config values:
- Strip a trailing `-(low|medium|high|xhigh|max)` from the slug to get its base.
- If **≥2 slugs** share a base, that base is one `ModelOption`:
  - `id` = the base;
  - `label` = the label with a trailing ` (Low|Medium|High|XHigh|Max)` removed;
  - `effort_levels` = the levels seen, in the order Low < Medium < High < XHigh < Max.
- Otherwise keep the slug as is: `id` = slug, `label` = label, `effort_levels` = `[]`.
- Keep first-seen order.

Test this against the verified output (`gemini-4-neon-{high,xhigh,medium,low}` → `gemini-4-neon` with 4 levels; `gpt-oss-120b-medium` alone stays as is; `gemini-pro-orchestrator` has no suffix). It must equal the ACP `configOptions.model` values listed in §2.2.

---

## 5. Phases

Order: **P1 → P2 → P3 → P4 → P5.** P1 and P2 touch only the new crate. P3 wires the provider in. P4 is fork. P5 covers docs and acceptance. Each phase ends with fmt, clippy and test clean, and an Opus review.

### P1 — Recorder, fixtures, Translator (new crate, not wired) · Sonnet

**Goal:** prove the protocol in §2 with recorded fixtures and a pure translator.

**Tasks:**
1. Create `crates/antigravity` (§4 layout) and add it to the workspace `members`. Only `lib.rs`, `rpc.rs` and `translate.rs` get real code now.
2. Write `scripts/record.py`, mirroring Claude's recorder:
   - It runs `agy acp` in a `mktemp -d` sandbox, sends the scripted requests, and auto-answers permissions per scenario.
   - It writes `{"dir":"in"|"out"|"meta","frame":…}` lines.
   - It redacts the home path, user name and emails, and refuses sandboxes under `$HOME`.
   - It drops `available_commands_update` frames (huge, and they contain the user's skill list, which is private).
   - **At the end, it deletes the conversation it created** (`conversations/<id>.db*`, `brain/<id>`), so recording never litters the user's history.
3. Record these scenarios. Use model `gemini-3.8-flash` via `set_config_option` to keep quota use small:
   - `simple_turn`
   - `two_turns`
   - `thinking`
   - `tool_allow`: a `run_command` request allowed.
   - `tool_deny`: the same, denied; record what agy does after the deny.
   - `edit`: an `edit_file` with a diff.
   - `interrupt`: cancel mid-stream, then the next prompt in the same process.
   - `interrupt_pending_permission`: cancel while a request is pending; answer `cancelled`.
   - `steer`: a second prompt mid-turn; record both prompt responses.
   - `resume`: a new process, `session/resume`; confirm there is no replay and that the context is kept.
   - `set_config`: model + thought_level.
   - `bad_model`: `set_config_option` with an unknown model.
   - `bad_resume`: resume of a random UUID.
   - `unknown_agent_request`, if one can be triggered; else a unit test only.
4. Implement `Translator` per §3 and §4.1. Write `tests/fixtures.rs`: replay each fixture's `out` frames and assert the exact `ProviderEvent` sequence. The cases that matter most:
   - text flushed before `ToolUse` and at turn end;
   - a `ToolUse` synthesized for an unseen `tool_call_update`;
   - exactly one `ToolResult` per id;
   - steer: no `TurnComplete` on the first response, usage summed;
   - cancel: `interrupted:true` and `is_error:false`;
   - `stopReason` other than `end_turn` → `is_error`;
   - unknown `sessionUpdate` skipped;
   - a string vs numeric JSON-RPC id.
5. Write `wiki-llm/antigravity-protocol.md` (via Haiku, with the facts supplied). It mirrors `claude-protocol.md`: spawn, handshake, stdin, stdout, frame→event, lifecycle. Each fact must be backed by a fixture. Add a row to `wiki-llm/index.md`.

**Acceptance:**
- All fixtures are committed and redacted; `grep -ri "$USER\|/home/" crates/antigravity/fixtures` finds nothing.
- Translator tests pass.
- The Antigravity history has no leftover recording conversations.
- Any §2 **(verify)** result that differed is reflected in the code and the wiki page.

### P2 — Process + provider (crate complete, still not wired) · Sonnet

**Goal:** a working `AntigravityProvider` that `examples/chat.rs` can drive end to end.

**Tasks:**
1. Write `rpc.rs` framing and `process.rs` (§4.2), including every lifecycle step (1–11). Unit-test the pure parts:
   - permission option choice;
   - id round-trip (string and number);
   - effort → thought_level mapping;
   - Max → none.
2. Write `models.rs` (§4.4) with tests, and `provider.rs` (§4.3) with tests for:
   - binary resolution order (inject the dirs, as Claude's tests do);
   - `delete_session` id validation, the `-wal`/`-shm`/`brain` removal, and symlink safety (use `tempfile` with a fake `$HOME`-style root passed in; don't touch the real `~/.gemini` in tests).
3. Write an in-crate integration test with a **fake `agy`**: a small script in `tests/` that speaks scripted ACP from a fixture, so `process.rs` is tested without the network. Mirror whatever `crates/claude/tests` does for its process tests. If Claude has no such harness, write a minimal one: a Python or shell stub that echoes the fixture's `out` frames in response to `in` frames, chosen by the `AGY_FAKE_FIXTURE` env var. The stub lives in the test dir and is never shipped. Cover:
   - prompts queued during the handshake;
   - interrupt before Ready;
   - an auth error on `session/new` → `Error` then `Exited`;
   - a shutdown sequence that ends with `Exited`.
4. Write `examples/chat.rs` (a REPL: `/stop`, `/auto on|off`, `y`/`n` for permissions) and `examples/parent_death.rs` (the child is gone within 2 s after `kill -9` of the parent).

5. **Real-`agy` e2e tests** in `crates/antigravity/tests/e2e.rs`. Each is `#[ignore]` and also skips unless `XADE_E2E_AGY=1`, so CI and `cargo test` never spend quota. Each test:
   - runs in a `mktemp -d` git repo;
   - uses `gemini-3.8-flash`;
   - deletes its conversation afterwards through `delete_session`, even on failure (a drop guard).

   Drive `AntigravityProvider::spawn` and assert on `ProviderEvent`s. Cases:
   - a simple turn;
   - two turns in one process;
   - Auto OFF: a command gives `PermissionRequest`; allow gives `ToolResult`; deny gives no command output;
   - Auto ON: no `PermissionRequest`;
   - interrupt, then the next prompt works, with the same pid;
   - steer: exactly one `TurnComplete`;
   - resume in a new process keeps context (ask the model to recall a word);
   - a bad model is non-fatal;
   - no prompt sent → no conversation file created;
   - Shutdown → `Exited` and no `agy acp` child left (`pgrep -P`).

**Acceptance:**
- `XADE_E2E_AGY=1 cargo test -p xade-antigravity --test e2e -- --ignored --test-threads=1` passes on this machine (`agy` is installed and signed in).
- `cargo run -p xade-antigravity --example chat -- <dir>` completes the following, and each must work:
  - a conversation;
  - an allowed and a denied command;
  - `/stop` mid-stream, then the next prompt with the same pid;
  - a mid-turn steer.
- `parent_death` passes.
- No conversation `.db` is created when the REPL is started and quit without a prompt (D6).

### P3 — Multi-provider wiring in the app · Sonnet

**Goal:** the user picks Claude or Antigravity for a new session. Both run side by side. Nothing changes for Claude-only users.

**Files:**
- `crates/core/src/ids.rs`
- `crates/store/src/{schema.rs,reader.rs}` + tests
- `apps/desktop/src/{main.rs,app.rs,home.rs,status_bar.rs,conversation.rs,session_view.rs,widgets/delete_modal.rs}`
- `apps/desktop/Cargo.toml`
- `assets/google-symbol.{svg,png}`

**Tasks:**
1. **Core:** add `ProviderId::Antigravity`; it serializes as `"antigravity"`. Fix every exhaustive `match` the compiler flags.
2. **Store:**
   - `provider_str` / `parse_provider` gain `"antigravity"`.
   - `last_model_choice(provider: ProviderId)` filters `AND provider_id = ?`. Update the one caller (`App::new_session`). This matters: without the filter, a Claude session would inherit a Gemini model id.
   - Add a store test with mixed providers.
3. **Composition root (`main.rs`):**
   - Build `vec![ClaudeProvider::new(), AntigravityProvider::new()]` and run both preflights **in parallel** (two `std::thread::spawn`s, joined) so startup latency does not grow.
   - Pass a `HashMap<ProviderId, Option<String>>` of preflight errors instead of the single `Option<String>`.
   - Log each failure at warn level, without account data.
4. **Per-provider availability in the UI.** Replace `HomeState.preflight_error: Option<String>` with `preflight: HashMap<ProviderId, Result<(), String>>` and the helpers `App::available(p) -> bool` and `App::any_available()`. Then audit every current `preflight_error` use (about 12; `grep -n preflight_error apps/desktop/src`) and give each a clear meaning:
   - Opening or creating a session is blocked only if **that session's provider** is unavailable. The message names the provider.
   - Import starts if Claude is available (it is the only importer today; use "any provider available" and let providers with no importer return empty).
   - A provider whose preflight is `NotInstalled` is **hidden** (status bar, new-session picker, fork targets) as long as another provider is available. Agy is optional; Claude-only users must see exactly today's UI.
   - The Home error banner lists each installed-but-failing provider with its own Retry. Retry re-runs only that provider's preflight (`UiMsg::Preflight(ProviderId, Result)`).
   - When the last-open session belongs to a hidden provider, restoring its window shows it with the error status (do not silently drop windows from `windows.txt`).
5. **New-session provider picker** (`home.rs::provider_dropdown`):
   - Make it real. It lists the available providers; the selection lives in `App` (memory only).
   - It defaults to the provider of the latest Xade session (add `reader.last_provider()`, or reuse `last_model_choice` across providers if it returns the provider too; pick the smaller change).
   - Opinionated: no new setting.
   - `new_session()` uses the picked provider. `last_model_choice(picked)` falls back to `provider.default_model()`. If the stored effort is not supported by the model (`effort_choices`), use `Effort::Default`.
   - Hide the dropdown entirely when only one provider is available (Claude-only users see no change).
6. **Provider-neutral labels:**
   - The Home card's hard-coded `"Claude"` (`home.rs` ~770) → `provider.display_name()`. Claude keeps `theme::claude()`. Antigravity uses the existing `theme::text_muted()`. **No new theme key** for Antigravity in v1.
   - The status bar is already per provider; only make the dot color use that provider's own preflight result.
   - The Home banner text `"Claude is not available"` → `"{name} is not available"`.
   - `delete_modal.rs` text: "its Claude Code transcript" → "its {provider} history".
   - Provider symbol: today `conversation::claude_symbol` is drawn as the assistant avatar (`conversation.rs`) and in the session header (`session_view.rs` ~99).
     - Generalize it to `provider_symbol(ctx, ProviderId)`: Claude keeps `claude-symbol.png`, Antigravity uses the **Google "G" logo**.
     - Add `assets/google-symbol.svg`, taken from Wikimedia Commons (note its source and license next to it, as for the Claude symbol), and pre-render it to `assets/google-symbol.png` with `resvg -w 80 -h 80`.
     - Cache one texture per provider.
     - Extend the existing decode test to both symbols.
     - Update the `ui.md` avatar line.
7. **Behavior notes that need no code, only checks:**
   - Tool cards render agy tools generically by name.
   - The diff modal and code-change approval stay Claude-tool-name based (`diff.rs::is_code_change`). For agy, edits are not gated in `default` mode anyway, and its permission requests use the banner.
   - The turn's changes card comes from `turn_files.rs` (git working tree), which already works for any provider in a git repo. Outside git, agy turns show no changes card in v1 (§9).
   - The cost readout shows `$0.00` for agy. Hide the cost part of `usage_text` when `cost_usd == 0.0 && provider != Claude`. Better, provider-neutral: hide it when the session's total cost is 0 and its output tokens are > 0.
8. **Tests:** update the test fixtures that construct `Session` (they will not compile without the new match arms). Add:
   - a headless layout test of the Home view with two providers;
   - a unit test that `NotInstalled` hides a provider;
   - a test that `new_session` passes the picked provider.

**Acceptance (manual, Linux and macOS):**
- With `agy` installed and signed in, start one Claude and one Antigravity session in the same directory. Both stream side by side.
- The agy session: model and thinking pickers show the grouped models (e.g. "Gemini 4 Neon" with Low…Extra); changing them respawns and continues the conversation.
- The agy session with Auto OFF: a shell command shows the banner, and Approve, Approve & Enable Auto and Deny each work. Auto ON: no banner. Stop works, and the next prompt reuses the same pid.
- Quit and relaunch: the agy session is found by search, reopens, and resumes on a new prompt (the model remembers earlier context).
- Delete an agy session from Home: its `.db` and `brain/` dir are gone.
- Rename `agy` away (or set `PATH` without it): the UI is identical to today's Claude-only UI.
- After quitting, `pgrep -f "agy acp"` finds nothing. After `kill -9` of Xade, every agy child is gone within 2 s.

### P4 — Fork a session to another provider · Opus designs (this section), Sonnet implements

**Goal:** from any idle session, "Fork to Antigravity", "Fork to Claude" (any available provider, including the same one) creates a new session with the same history that continues seamlessly on the target.

**Flow** (all new code goes in `xade-runtime` and `xade`; providers are untouched):
1. **UI:**
   - Add a header button in the session window (`session_view.rs`) using a new `icons::FORK` glyph; assert it in the existing icons test. It opens a small menu with one entry per available provider: `"Fork to <display_name>"`.
   - The button is disabled, with a tooltip, unless the session status is `Idle` or `Closed`/dormant and the session has ≥1 message. A fork mid-turn would copy a half-finished turn.
   - Clicking an entry sends `ManagerCmd::Fork { session_id, provider }`.
2. **Manager (`ManagerCmd::Fork`):**
   1. `reader.load_session(source)` gives the row and its messages. If the source is live, first `flush(&writer)` so the latest messages are on disk.
   2. Build the new `Session`:
      - new id; same `workspace_id`; `provider_id = target`; `provider_session_id = None`; `origin = Xade`;
      - `title = "Fork: " + source.title`, cut to 60 chars by the `derive_title` rule; if the title is empty, derive it from the first user message;
      - `model_id`/`effort` = `reader.last_model_choice(target)`, else `target.default_model()`/`Default`;
      - effort reset to `Default` if the target model doesn't list it;
      - `auto_mode = source.auto_mode`; usage zeroed; `sent_model_id = ""`.
   3. Copy the messages into new `Message` rows: new uuids, the same `seq` order starting at 1, the same role, text, tool fields and `is_error`, `created_at` kept.
   4. Write the session and the messages **in one transaction**: a new `WriteOp::InsertSessionWithMessages { session, messages }`. Search rows come from the existing triggers. A crash mid-fork must not leave a half copy.
   5. `TouchWorkspace` and `flush`, then emit `AppEventKind::SessionForked { from, session_id: new_id }`.
   6. On any failure: `AppEventKind::Error("Could not fork the session: …")`. Nothing is written.
3. **UI on `SessionForked`:**
   - Call the normal `open_session(new_id)`, which goes through `ManagerCmd::Open` → a dormant session with full history shown, so no new open path.
   - Focus the new window. Refresh the Home cards.
4. **Hand-off (actor):**
   - When the manager builds a `Dormant` in `open()`, it computes `handoff: Option<String>` = `Some(transcript(&messages))` **iff** `session.provider_session_id.is_none() && !messages.is_empty()`.
   - `wake()` passes it to `Actor::new`.
   - On the **first `Send`**, the actor prepends it the way it prepends `context`: `format!("{handoff}\n{context?}\n{text}")`. Then it sets `handoff = None`. The stored and shown user message stays the user's text only.
   - This survives restarts with no new column: a fork that has not sent yet still has a `None` provider id and copied messages, so the next open recomputes the hand-off. Once the provider reports `Started`, the provider id is stored and the hand-off is never sent again.
   - Side effect, acceptable and correct: a normal session whose first turn crashed before `Started` will, on reopen, re-send its earlier messages as context.
5. **`transcript(messages)`:** a pure function in `xade-runtime`, unit-tested.
   - Format:
     ```
     <previous_conversation>
     This conversation started in another agent session in this same directory. Its history follows; continue from where it ends.

     [user] …
     [assistant] …
     [tool_call <name>] <input_json, max 1000 chars>
     [tool_result <is_error?>] <output, max 2000 chars>
     </previous_conversation>
     ```
   - Skip `System` messages and `tool_payload_json` diffs.
   - Truncation:
     - truncate per item with a `…[truncated]` marker;
     - whole-transcript cap of **150 000 chars**: drop the **oldest** items first and insert `[… N earlier messages omitted]`;
     - always keep the first user message (the task statement) and the most recent items.
   - Never cut inside a UTF-8 char.
   - Never include `queued`/`steered` delivery tags (they are UI metadata).
6. **Corner cases to test:**
   - fork of a fork (the hand-off contains the copied history once, not nested twice);
   - a fork whose source is a `ClaudeCode`-origin import (allowed; the result is `Xade` origin);
   - fork to the same provider (works, via the hand-off; native `--fork-session` is §9);
   - fork while the target provider is unavailable (not offered);
   - a source with only a user message and no reply;
   - a very long transcript (cap and order);
   - closing the fork window before sending, then reopening (the hand-off is still sent);
   - deleting the source does not affect the fork (it holds copies);
   - interrupt on the first turn of a fork (the hand-off was sent; `Started` stored the provider id, so it is not re-sent; acceptable).

**Acceptance:**
- Fork a Claude session that edited files to Antigravity. Ask "what did we just change and why?" in the fork. The answer reflects the source history.
- The fork shows the full copied history, and the source is unchanged.
- Fork it back to Claude: it works the same way.
- Relaunch between the fork and the first send: the hand-off is still applied.

### P5 — Docs, wiki, acceptance · Haiku (prose) + Sonnet (checks)

0. **End-to-end run on this machine (`agy` and `claude` are both installed and signed in).** Opus runs this; it is the gate for calling the MVP done.
   - **Crate e2e:** the P2 suite: `XADE_E2E_AGY=1 cargo test -p xade-antigravity --test e2e -- --ignored --test-threads=1`.
   - **Runtime e2e:** `crates/runtime/tests/e2e_providers.rs`, `#[ignore]`, gated by the same env var. It needs a dev-dependency on both adapter crates, which is allowed for tests only; the dependency rule covers library code. It starts `SessionManager` with the real `ClaudeProvider` and `AntigravityProvider` over a temp store and covers:
     - Create on each provider → Send → `TurnComplete`;
     - Close → Open (dormant) → Send resumes with context;
     - `SetModel` respawn on agy;
     - **Fork Claude→Antigravity and Antigravity→Claude**: the fork answers a recall question ("what word did I ask you to remember?") from the source history, and the hand-off is sent once only;
     - DeleteSession removes the agy conversation file;
     - `ShutdownAll` leaves no `claude -p` / `agy acp` children.

     It cleans up every provider conversation it created.
   - **App smoke test:** launch the real app via the `run` skill against a temp `XADE_DATA_DIR`. Take screenshots of Home with both providers, an agy session streaming next to a Claude one, and a forked window.
   - Report results with output. A failure blocks P5 docs until it is fixed or the user accepts it.

1. **Wiki** (facts only, one line each; see the AGENTS.md "Wiki Upkeep" rules):
   - `architecture.md`: the dependency rule line, providers listed, the fork flow and the hand-off rule.
   - `conventions.md`: the dependency-rule invariant gains `xade-antigravity`.   - `store.md`: `provider_id` values; `last_model_choice` is per provider; `InsertSessionWithMessages`.
   - `ui.md`: the provider picker, provider hiding, and fork.
   - `testing.md`: the agy recorder, plus manual acceptance lines 11–14 below.
   - `index.md`: an `antigravity-protocol.md` row; "Adding a provider" points at both protocol pages.
2. **User docs:** `docs/*.md` gets a short "Antigravity" section: install and sign in with `agy` once; Auto OFF gates commands but not workspace edits; no quota meter.
3. **Manual acceptance:** add these to the testing.md list:
   11. Claude and Antigravity sessions stream side by side; their pickers show each provider's own models.
   12. Antigravity: Auto OFF gates a shell command; Stop keeps the pid; relaunch + resume keeps context.
   13. Fork Claude→Antigravity and back; the fork answers from the copied history.
   14. Without `agy` installed, the UI matches the Claude-only build.

---

## 6. Corner cases checklist (every one needs a test or a manual line)

**Protocol / adapter:**
- [ ] The JSON-RPC id from agy is a string (`"1"`): round-trip it unchanged.
- [ ] Steer: an early `end_turn` for a superseded prompt must not end the turn (§2.3).
- [ ] Interrupt with a pending permission: answer `cancelled`, then `session/cancel`.
- [ ] Interrupt before the session is ready: synthesize `TurnComplete{interrupted}`.
- [ ] Text arriving before a tool call or a thought is flushed as its own `AssistantText`; nothing is lost on cancel.
- [ ] A `tool_call_update` without a prior `tool_call` → `ToolUse` synthesized.
- [ ] A tool `status` other than completed is terminal → `is_error`.
- [ ] An unknown request from agy (elicitation, `fs/*`, `terminal/*`, future methods) → reply `-32601` at once.
- [ ] Notifications for another `sessionId` are ignored.
- [ ] The model id is gone after a CLI update → `set_config_option` error is non-fatal and the session still works.
- [ ] Resume of a deleted conversation → a clear error status, not a hang.
- [ ] Not signed in → a clear message telling the user to run `agy` once; preflight shows the provider in red with Retry.
- [ ] `agy models` hangs (network) → 15 s timeout; startup is not blocked longer than today (parallel preflight).
- [ ] Huge tool output lines: no line-length cap; the UI's existing truncation applies.
- [ ] `agy` auto-updates between sessions (version change): nothing is cached on disk; models refresh on Retry or a restart.
- [ ] The CLI starts a turn on its own (`/schedule`, `/goal`): the actor goes Streaming on output, but no `TurnComplete` comes. Known limitation; log once at debug. The user can press Stop.
- [ ] The mise shim as the binary: process-group kill reaches the real binary (verify with `pgrep` after Close).
- [ ] `$HOME` unset: binary lookup and `delete_session` degrade without panicking.

**App / store:**
- [ ] `last_model_choice` is per provider (no Gemini model sent to `claude`).
- [ ] An existing DB with only `claude` rows opens unchanged; no migration.
- [ ] Hidden provider + an existing session of that provider in Home: the card shows; opening it shows that provider's error.
- [ ] Effort `Max` on a Claude session forked to agy → `Default`.
- [ ] Two agy sessions in the same directory are independent (separate processes and conversations).

---

## 7. Execution protocol for sub-agents

- One phase per dispatch, `model: sonnet`. The prompt includes:
  - this file's §1–§4, plus the phase section;
  - `wiki-llm/index.md`, `wiki-llm/conventions.md`, and the wiki pages the phase touches;
  - for P1/P2, `wiki-llm/claude-protocol.md` and `crates/claude/src/*` as the pattern to mirror.
- A sub-agent that hits a fact contradicting §2 records the fixture, follows the observed behavior, and reports the difference in its summary. It never guesses silently.
- A sub-agent that needs a breaking change, a dependency upgrade, or a change to a hard invariant stops and reports. It does not proceed.
- Recording uses real `agy` quota: keep prompts tiny, use `gemini-3.8-flash`, and record each scenario once.
- Opus reviews each phase's diff against this plan and `conventions.md` before the next phase starts.

---

## 8. Decisions taken by the user

1. Fork hand-off text is allowed: the old "no prompt engineering" rule was removed from `conventions.md` (2026-11-25).
2. No Antigravity theme color in v1. Its symbol is the Google logo, wherever the Claude symbol is shown today.

---

## 9. Out of scope / next iterations

- Import of Antigravity-native sessions (ACP `session/list` + `session/load` replay maps cleanly onto `ImportedSession`), plus a `SessionOrigin::Antigravity` badge and filter.
- Live model and effort switching via `set_config_option` (no respawn), behind an optional provider capability.
- Antigravity quota: the TUI `/usage` panel has no machine-readable equivalent today.
- Diff modal and approval for agy edit tools: map ACP `content[{type:"diff"}]` to a provider-neutral diff payload instead of Claude tool names. Also a changes card outside git.
- Native same-provider fork (Claude `--resume <id> --fork-session`), skipping the hand-off.
- Show agy's thinking text (it is exposed) once the core grows a thinking-text event.
- Use `session_info_update.title` as the auto title.
- Running-elsewhere detection for agy sessions (`running_sessions`).
