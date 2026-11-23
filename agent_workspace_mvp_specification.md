# Product & Architecture Specification: Multi-Provider Agent Workspace (MVP)

## 1. Executive Summary & Vision

The **Agent Workspace** is a native, GPU-accelerated Rust desktop application designed to provide a high-productivity, multi-session graphical interface over local AI coding agent CLIs. Rather than re-implementing agent loops, prompt orchestration, or credential management, the application maintains an ultra-thin backend that supervises unmodified vendor CLI binaries through persistent bidirectional streaming pipes.

### Core Goals
* **High-Throughput Developer UX:** Eliminate sluggish webview rendering and single-thread bottlenecks by supporting unlimited, draggable, side-by-side session windows inside a GPU-rendered workspace.
* **Work-in-Progress (WIP) Visibility:** Persist all sessions locally in SQLite with instant, typo-tolerant fuzzy search and paginated browsing organized by working directory.
* **Vendor Agnosticism:** Isolate vendor-specific CLI protocols behind Clean Architecture adapter interfaces, launching with **Anthropic Claude Code** (`claude`) and designed for seamless expansion to **Google Antigravity** (`agy`).
* **Strict Enterprise Compliance:** Operate 100% within Enterprise Terms of Service by spawning unmodified official binaries authenticated via the user's native local CLI session.

---

## 2. Technology Stack & Clean Architecture

### 2.1 Desktop UI Framework
* **Primary Framework:** **Iced** (backed by `wgpu` for native Metal, Vulkan, and DirectX 12 GPU acceleration). Its Elm-inspired Model-View-Update (MVU) architecture enforces unidirectional data flow and strict decoupling between UI state and domain logic.
* **Async Runtime:** **Tokio** for non-blocking child process management, asynchronous `stdin`/`stdout` NDJSON streaming, and background SQLite writes.
* **Persistence Engine:** **SQLite** (in WAL mode) with built-in **FTS5** trigram full-text indexing.

### 2.2 Clean Architecture Layers (Dependency Rule)
Following Robert C. Martin’s **Clean Architecture**, all source code dependencies point strictly inward toward core domain entities. Outer frameworks (GPU UI, SQLite, OS subprocesses) depend on inner abstractions, never the reverse.

| Ring | Layer Name | Responsibilities | Allowed Dependencies |
| :--- | :--- | :--- | :--- |
| **1 (Inner)** | **Entities (Domain)** | Pure domain models (`Session`, `Workspace`, `Message`, `QuotaWindow`, `ProviderCapabilities`, `ApprovalPolicy`) and core business invariants. | None (Rust `std` only). |
| **2** | **Use Cases (Application)** | Application workflows (`CreateSession`, `SendPrompt`, `InterruptTurn`, `ResumeSession`, `SearchSessionsFuzzy`, `ResolveToolApproval`, `UpdateSessionConfig`, `SyncQuotaTelemetry`) and abstract **Port traits**. | Entities ring. |
| **3** | **Interface Adapters** | Concrete implementations of Ports: **Provider Adapters** (`ClaudeCliAdapter`, `AntigravityCliAdapter`), **SQLite Repository Adapter**, and **UI ViewModels / Presenters**. | Use Cases and Entities rings. |
| **4 (Outer)** | **Frameworks & Drivers** | Iced GPU windowing/canvas widgets, `tokio::process` child supervisors, SQLite connection pool, OS file dialogs, and drag-and-drop handlers. | Interface Adapters, Use Cases, and Entities rings. |

---

## 3. Provider Adapter Specification (Thin Backend)

The backend contains no LLM prompt engineering or token billing logic. Each vendor integration is implemented as an adapter satisfying a unified `AgentProviderPort` interface, translating between vendor-specific NDJSON stdout/stdin frames and uniform domain events.

### 3.1 Subprocess Lifecycle & Protocol Normalization
* **Warm Persistent Processes:** Opening or resuming a session spawns a long-lived child CLI process bound to the session's working directory. Keeping `stdin` open preserves in-memory session context and vendor prompt caching across turns.
* **Graceful Interruption vs. Termination:** Triggering "Stop" sends `SIGINT` to abort the active generation turn while keeping the warm process alive. Closing a session window sends `SIGTERM` to clean up child processes cleanly.

### 3.2 Provider Capability Matrix

| Capability | Anthropic (`claude` CLI) | Google Antigravity (`agy` CLI) | Unified Domain Abstraction |
| :--- | :--- | :--- | :--- |
| **Streaming Mode** | Spawns unmodified `claude` with `-p --input-format stream-json --output-format stream-json --verbose --include-partial-messages`. | Spawns unmodified `agy` with `--input-format stream-json --output-format stream-json` (omitting `-p` so `stdin` remains active). | Emits normalized `DomainStreamEvent` items (`TextDelta`, `ThinkingDelta`, `ToolUse`, `ToolResult`, `TurnComplete`, `Error`). |
| **Session Resumption** | Captures `session_id` from the initial `system` frame; resumes prior state via `--resume <session_id>`. | Captures native conversation ID from stream metadata; resumes via CLI conversation flags. | Persists `provider_session_id` in SQLite for crash-safe resumption. |
| **Auto Mode & Permissions** | Routes `control_request` (`can_use_tool`) frames over stdio (`--permission-prompt-tool stdio`). Auto Mode immediately replies `allow`; Manual Mode pauses for user approval. | Maps Auto Mode to CLI execution flags (`--mode accept-edits` / allow rules) or stdio approval responses. | Exposes a per-session `auto_mode: bool` setting in the domain. |
| **Model & Thinking Effort** | Passes `--model` and reasoning/thinking effort flags or session controls. | Passes `--model` (e.g., Gemini 3.5 Pro/Flash) and reasoning effort parameters. | Adapter exposes `supported_models()` and `supported_effort_levels()` to dynamically populate UI selectors. |
| **Usage & Quota Telemetry** | Passively parses inline `rate_limit_event` frames (`five_hour` and `seven_day` utilization percentages and reset timestamps). | Passively parses turn `result` token counts and quota status envelopes. | Updates `QuotaWindow` and per-session `UsageMetrics` reactively without extra polling requests. |

---

## 4. On-Disk Persistence & Fuzzy Search (SQLite)

All session metadata, message histories, and file attachments are stored in a local SQLite database configured with **Write-Ahead Logging (WAL)** so concurrent streaming writes from multiple background sessions never block UI queries.

### 4.1 Logical Data Entities
* **`workspaces`:**
  * `id` (UUID), `root_path` (unique normalized path), `display_name`, `last_opened_at`, `is_pinned`.
* **`sessions`:**
  * `id` (UUID), `workspace_id` (FK), `provider_id` (`claude` | `antigravity`), `provider_session_id` (nullable string), `title`, `preview_snippet`, `model_id`, `thinking_effort`, `auto_mode` (boolean), `context_tokens_used`, `context_window_max`, `total_input_tokens`, `total_output_tokens`, `total_cost_usd`, `created_at`, `updated_at`.
* **`messages`:**
  * `id` (UUID), `session_id` (FK), `sequence_num`, `role` (`user` | `assistant` | `tool_call` | `tool_result` | `system`), `content_text`, `thinking_text`, `tool_name`, `tool_payload_json`, `created_at`.
* **`attachments`:**
  * `id` (UUID), `message_id` (FK), `file_path`, `file_name`, `mime_type`, `byte_size`.

### 4.2 Typo-Tolerant Fuzzy Search Pipeline
1. **FTS5 Trigram Indexing:** A SQLite **FTS5** virtual table using the **`trigram` tokenizer** automatically indexes `sessions.title`, `workspaces.root_path`, and `messages.content_text` via database triggers whenever records are inserted or updated.
2. **Candidate Retrieval & Fuzzy Re-Ranking:**
   * Queries of 3+ characters query the FTS5 trigram index to retrieve candidate sessions quickly on disk, tolerating character insertions, deletions, and transpositions.
   * For queries under 3 characters, a fast `LIKE` prefix/substring fallback is used.
   * Candidate matches are re-ranked in Rust using a fuzzy string scorer so exact matches appear first, followed by close typo matches, with matched excerpts highlighted in the session cards.

---

## 5. User Interface Specification

The application consists of two primary views toggled via a persistent navigation control: the **Home / Main Window** and the **Multi-Session Workspace Canvas**. Background sessions continue streaming seamlessly regardless of which view is active.

### 5.1 Home / Main Window
Designed as an organizational dashboard for reviewing works in progress and monitoring subscription capacity.

* **Top Global Bar:**
  * **Fuzzy Search Box:** Real-time search input filtering the session grid across all historical message text and titles (scoped either globally or to the active working directory).
  * **Subscription Quota Bar:** Dual visual progress indicator displaying **5-hour session utilization (%)** with reset countdown and **7-day weekly utilization (%)** with reset timestamp.
  * **New Session Action:** Button + provider selector (`Claude` / `Antigravity`) to launch a new session in the currently selected working directory.
* **Left Sidebar (Working Directories):**
  * Top **"All Workspaces"** filter showing sessions across every repository.
  * Scrollable list of registered working directories sorted by most recent activity, displaying active session count badges for directories with running CLI processes.
  * **"Add Directory"** button opening the native OS folder picker.
* **Center Canvas (Paginated Session Grid):**
  * Responsive card grid displaying sessions matching the active working directory and search query.
  * **Session Card Contents:** Editable/derived title, provider badge, working directory basename, 2–3 line conversation preview snippet (with highlighted search terms when filtering), relative timestamp, and live status badge (`Idle`, `Streaming`, `Needs Approval`, `Error`).
  * **Pagination Controls:** Footer bar with page size selector and `Previous` / page numbers / `Next` navigation driven by `LIMIT`/`OFFSET` SQLite queries.
  * **Action:** Clicking any card opens or focuses that session inside the **Multi-Session Workspace Canvas**.

### 5.2 Multi-Session Workspace & Session Windows
Designed for high-density parallel work across multiple tasks or repositories simultaneously.

* **Unbounded Draggable Canvas:**
  * Open sessions render as independent, draggable, and resizable floating/tileable window panes inside the GPU-accelerated workspace canvas.
  * **No Concurrency Limit:** Users can open as many simultaneous sessions as fit comfortably on their screen(s), with each pane backed by its own isolated `tokio` subprocess handle.
  * **Window Management:** Drag panes by their header bar to reposition freely, snap/tile panes side-by-side, resize edges/corners, minimize inactive panes to a bottom workspace dock, or close panes cleanly.
* **Inside Each Session Window:**
  1. **Title Header:** Inline-editable session title, working directory path badge, provider indicator, live status dot, minimize/maximize toggles, and close button.
  2. **Scrollable Conversation Stream (Chatbox):**
     * Incremental markdown rendering as streaming text deltas arrive.
     * Collapsible **Thinking / Reasoning** accordion blocks when reasoning output is present.
     * Structured **Tool Execution Cards** summarizing file reads, edits/diffs, and shell commands with execution status.
     * **Inline Permission Banner (when Auto Mode is OFF):** Pauses tool execution on `control_request` and displays `Approve`, `Approve & Enable Auto`, or `Deny` controls.
  3. **Composer & Session Control Bar:**
     * **Prompt Input:** Multi-line text input with keyboard submit (`Enter` / `Cmd+Enter`), `@`-file reference autocomplete scoped to the session's working directory, and a **Stop** button (`SIGINT`) during active generation.
     * **Attach Files (`+`):** Native file picker and drag-and-drop zone to attach local files to the next prompt turn.
     * **Model Selector:** Dropdown populated from the active provider adapter's available models.
     * **Thinking Effort Selector:** Selector (`Off`, `Low`, `Medium`, `High`, `Max`) mapped to provider-specific reasoning flags.
     * **Auto Mode Toggle:** One-click switch (`Auto: ON` / `Auto: OFF`) controlling whether tool calls and file edits are approved automatically for that session.
     * **Context Meter:** Compact indicator showing active context window consumption (`used / max` tokens and percentage) plus attached file count.
     * **Session Usage Meter:** Compact readout showing cumulative session tokens (input, output, cache reads) and estimated cost.

---

## 6. Non-Functional & Compliance Guardrails

1. **Enterprise Terms of Service Compliance:**
   * The application must never inspect, extract, or transmit OAuth tokens or `.credentials.json` files.
   * Authentication relies exclusively on the user's pre-existing local login state inside the official `claude` or `agy` CLI binaries (`claude auth status` pre-flight verification).
   * Vendor binaries must never be patched, wrapped with modified entrypoints, or exposed as a shared multi-user network service.
2. **Crash & Data Integrity:**
   * Every user prompt, finalized assistant message, and tool result is committed transactionally to SQLite immediately upon completion so unexpected shutdowns never lose conversation history.
3. **Zero Orphan Processes:**
   * Closing a session pane or exiting the application gracefully terminates all child CLI processes before exit.