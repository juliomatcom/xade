# Architecture
- Crates: `aw-core` entities, events, `AgentProvider`; `aw-claude` Claude CLI adapter; `aw-store` SQLite writer, reader, search; `aw-runtime` SessionManager and session actors; `desktop` egui UI and composition root.
- Dependency rule: aw-core ← aw-claude, aw-store ← aw-runtime ← desktop; runtime gets providers injected as `Arc<dyn AgentProvider>` and never names aw-claude.
- Providers implement `aw_core::AgentProvider`; `spawn` returns a `SessionHandle` of two unbounded channels (commands in, events out) and must run inside a tokio runtime.
- Data flow: UI → `ManagerCmd` → SessionManager → session actor → provider; the actor emits `AppEvent`s to the UI and `WriteOp`s to the store writer, and the UI reads the store through `StoreReader`.
- The session actor is the only producer of message `WriteOp`s; the UI never writes messages.
- `AW_DATA_DIR` overrides the data directory (the DB lives at `$AW_DATA_DIR/workspace.db`).
