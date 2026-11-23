# Architecture
- Crates: `aw-core` entities, events, `AgentProvider`; `aw-claude` Claude CLI adapter; `aw-store` SQLite writer, reader, search; `aw-runtime` SessionManager and session actors; `desktop` egui UI and composition root.
- Dependency rule: aw-core ← aw-claude, aw-store ← aw-runtime ← desktop; runtime gets providers injected as `Arc<dyn AgentProvider>` and never names aw-claude.
- Providers implement `aw_core::AgentProvider`; `spawn` returns a `SessionHandle` of two unbounded channels (commands in, events out) and must run inside a tokio runtime.
