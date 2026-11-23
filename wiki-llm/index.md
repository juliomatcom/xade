# Wiki index
Start here. Open only the page your task needs; the code is the detail.

| Task touches | Read | Then look in |
|---|---|---|
| Any change (rules, done criteria) | conventions.md | — |
| Crate boundaries, data flow, session lifecycle | architecture.md | Cargo.toml, crates/runtime/src |
| Claude CLI flags, frames, interrupt, permissions, shutdown | claude-protocol.md | crates/claude/src, crates/claude/fixtures |
| Persistence, schema, search | store.md | crates/store/src |
| Home, session windows, diff modal, status bar | ui.md | apps/desktop/src, design/workspace-mock.png |
| Tests, fixtures, manual checks | testing.md | crates/*/tests, crates/claude/scripts |
| Adding a provider | architecture.md, claude-protocol.md | crates/core/src/provider.rs |
