# Architecture
- Crates: `xade-core` entities, events, `AgentProvider`; `xade-claude` Claude CLI adapter; `xade-store` SQLite writer, reader, search; `xade-runtime` SessionManager and session actors; `xade` egui UI and composition root.
- Dependency rule: xade-core ← xade-claude, xade-store ← xade-runtime ← xade; runtime gets providers injected as `Arc<dyn AgentProvider>` and never names xade-claude.
- Providers implement `xade_core::AgentProvider`; `spawn` returns a `SessionHandle` of two unbounded channels (commands in, events out) and must run inside a tokio runtime.
- Data flow: UI → `ManagerCmd` → SessionManager → session actor → provider; the actor emits `AppEvent`s to the UI and `WriteOp`s to the store writer, and the UI reads the store through `StoreReader`.
- The session actor is the only producer of message `WriteOp`s; the UI never writes messages.
- `XADE_DATA_DIR` overrides the data directory (the DB lives at `$XADE_DATA_DIR/workspace.db`).
- Import: `ManagerCmd::Import` runs each provider's `import_sessions` via `spawn_blocking`, then the manager skips Xade-origin, live and not-newer sessions, sends `WriteOp::ImportSession` for the rest and emits `ImportDone`.
