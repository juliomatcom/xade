# Wiki index
Start here. Open only the page your task needs; the code is the detail.

| Task touches | Read | Then look in |
|---|---|---|
| Any change (rules, done criteria) | conventions.md | — |
| Crate boundaries, data flow, session lifecycle | architecture.md | Cargo.toml, crates/runtime/src |
| Claude CLI flags, frames, interrupt, permissions, shutdown | claude-protocol.md | crates/claude/src, crates/claude/fixtures |
| Antigravity (`agy acp`) frames, handshake, turn, permissions | antigravity-protocol.md | crates/antigravity/src, crates/antigravity/fixtures |
| Codex (`codex app-server`) frames, handshake, turn, approvals | codex-protocol.md | crates/codex/src, crates/codex/fixtures |
| Persistence, schema, search | store.md | crates/store/src |
| Home, session windows, diff modal, status bar | ui.md | apps/desktop/src, design/workspace-mock.png |
| Tests, e2e, fixtures, manual checks | testing.md | apps/desktop/tests/e2e, crates/*/tests, crates/claude/scripts |
| Adding a provider | architecture.md, claude-protocol.md, antigravity-protocol.md, codex-protocol.md | crates/core/src/provider.rs |
