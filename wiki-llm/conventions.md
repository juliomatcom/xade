# Conventions

## Product principles
- Xade is simple, opinionated and really fast. These lead every decision, for people and agents alike.
- Simple: do one thing well; no setting, mode or screen where a sensible default works.
- Opinionated: choose for the user; add an option only when no single choice is right.
- Really fast: never block a frame, never add startup or interaction latency; a feature that slows the app does not ship.

## Hard invariants
- Never read, copy, log or transmit CLI credentials or `claude auth status` output beyond `loggedIn` and `email`; `email` is for on-screen display only: never log (not at debug), persist, or put it in error reports, Copy text or issue bodies.
- Spawn the vendor binary unmodified; never patch, wrap or proxy it, never expose it over the network.
- No prompt engineering or token billing in this app; only pass user text and flags through.
- Dependency rule: xade-core ← xade-claude, xade-store ← xade-runtime ← xade. Only xade names concrete providers.
- Only the store writer thread writes SQLite; only the session actor creates message WriteOps.
- Persist every user prompt, finalized assistant block and tool result when it completes; never persist deltas.
- Unknown CLI frames or fields are skipped, never fatal.
- Every spawned child is reaped on close, app exit, and parent death.
- Code changes are reviewed only in the diff modal, never inline.

## Code
- Rust stable, edition 2021. `cargo fmt`; `cargo clippy --workspace -- -D warnings` clean.
- Packages are `xade-*` for crates and `xade` for the app (a crate named `core` shadows Rust's `core`); directories stay `crates/<name>` and `apps/desktop`.
- No `unwrap`/`expect` outside tests and startup wiring; errors via `thiserror` in libs.
- No trait without two implementations (FakeProvider counts for AgentProvider).
- Pin exact versions (`=x.y.z`) when adding a crate; never upgrade/swap one without asking. egui-family crates move together.
- `tracing` for logs; never log prompt or message text above `debug`.
- Never swallow an error: log it, or report it to the user (`App::report`); the UI never panics out of a frame.
- UI colors only from `theme.rs` tokens.

## Docs
- Wiki edits follow AGENTS.md "Wiki Upkeep". Protocol facts in claude-protocol.md must be backed by a fixture.

## Definition of done
- Phase acceptance passes; `cargo test --workspace`, clippy and fmt clean.
- New CLI behavior has a fixture and a Translator test.
- Wiki updated only if a flow, contract, invariant or non-obvious rule changed.
