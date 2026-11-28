# Conventions

## Product principles
- Xade is simple, opinionated and really fast. These lead every decision, for people and agents alike.
- Simple: do one thing well; no setting, mode or screen where a sensible default works.
- Opinionated: choose for the user; add an option only when no single choice is right.
- Really fast: never block a frame, never add startup or interaction latency; a feature that slows the app does not ship.

## Hard invariants
- Never read, copy, log or transmit CLI credentials (Claude's, `agy`'s, `codex`'s) or auth output (`claude auth status`, `codex` `account/read`) beyond login state and display-only account label (`email` / `"API key"`): never log (not at debug), persist, or put it in error reports, Copy text or issue bodies.
- Spawn the vendor binary unmodified; never patch, wrap or proxy it, never expose it over the network.
- No token billing in this app.
- Dependency rule: xade-core ← xade-claude, xade-antigravity, xade-codex, xade-store ← xade-runtime ← xade. Only xade names concrete providers.
- Only the store writer thread writes SQLite; only the session actor creates message WriteOps.
- Persist every user prompt, finalized assistant block and tool result when it completes; never persist deltas.
- Unknown CLI frames or fields are skipped, never fatal.
- Every spawned child is reaped on close, app exit, and parent death.
- Code changes are reviewed only in the diff modal, never inline.

## Code
- Rust pinned in `rust-toolchain.toml`, edition 2021. `cargo fmt`; `cargo clippy --workspace -- -D warnings` clean.
- Packages are `xade-*` for crates and `xade` for the app (a crate named `core` shadows Rust's `core`); directories stay `crates/<name>` and `apps/desktop`.
- No `unwrap`/`expect` outside tests and startup wiring; errors via `thiserror` in libs.
- No trait without two implementations (FakeProvider counts for AgentProvider).
- Pin every version exactly (crates `=x.y.z`, npm packages, toolchains, CI actions and runners, tools): no ranges, `latest`, `stable` or floating tags.
- Version updates are periodic and manual; never upgrade/swap a package without asking (CI builds with `--locked`). egui-family crates move together.
- `tracing` for logs; never log prompt or message text above `debug`.
- Never swallow an error: log it, or report it to the user (`App::report`); the UI never panics out of a frame.
- UI colors only from `theme.rs` tokens.
- UI is square: windows, buttons, frames and painted rects have no corner radius (`CornerRadius::ZERO`, painter rounding `0.0`); never add rounding.

## Docs
- Wiki edits follow AGENTS.md "Wiki Upkeep". Protocol facts in claude-protocol.md must be backed by a fixture.
- User-facing docs live in `docs/*.md`; settings, shortcuts and theme keys go in `docs/settings.md` (update it when one changes), never as a list in `README.md`.

## Definition of done
- Phase acceptance passes; `cargo test --workspace`, clippy and fmt clean.
- The e2e tests (`cargo test -p xade --test e2e`, see testing.md) must pass before every commit and in CI (the `e2e` job, which gates every build and release); a failing flow blocks the commit, and a change to a core flow updates its test in the same commit.
- New CLI behavior has a fixture and a Translator test.
- Wiki updated only if a flow, contract, invariant or non-obvious rule changed.
