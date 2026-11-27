# Testing
- Protocol fixtures: `python3 -I crates/claude/scripts/record.py <scenario> <sandbox-dir>`; scenarios are listed in the script. Record in `$(mktemp -d)`: a cwd under the home or scratchpad path leaks the user name through streamed chunks, and the script refuses such fixtures. Output is redacted (home path, user name, emails, account keys).
- Antigravity fixtures: `python3 -I crates/antigravity/scripts/record.py <scenario> <sandbox-dir>` (same sandbox and redaction rules); real-`agy` provider tests: `XADE_E2E_AGY=1 cargo test -p xade-antigravity --test e2e -- --ignored --test-threads=1`.
- Codex fixtures: `python3 -I crates/codex/scripts/record.py <scenario> <sandbox-dir>` (sandbox outside `$HOME`, deletes created threads; costs API tokens); real-`codex` provider tests: `XADE_E2E_CODEX=1 cargo test -p xade-codex --test e2e -- --ignored --test-threads=1`.
- Real-CLI runtime tests (both `claude` and `agy` signed in; spends a little quota): `XADE_E2E_AGY=1 cargo test -p xade-runtime --test e2e_providers -- --ignored --test-threads=1`; it deletes every provider conversation it creates.
- E2E tests are only the ones that start the real app and act in it like a person: `apps/desktop/tests/e2e/`, run with `cargo test -p xade --test e2e`. They use the standard `egui_kittest` harness (key presses, typed text and clicks on widgets found by label) over the same startup as `xade <folder>` (`xade::backend_with`), on a throwaway data folder under `target/` (imports skip temp dirs) with `HOME` and `PATH` isolated so no real `claude`, `agy` or history is touched. The model is mocked at the `AgentProvider` boundary by `FakeProvider` (`tests/e2e/fake_provider.rs`: echoes `Echo: <prompt>`, serves a canned history); everything above it is real. Core flows only: one test per flow, one file per area, each proven by breaking the feature and watching it fail. Every other kind of test lives beside the code it tests (`#[cfg(test)]` in the module, `crates/*/tests`). Rows or controls drawn without an accessible name cannot be found by a test: name them (`widget_info`) rather than clicking by position.
- Covered by `tests/e2e/`: open a workspace and a file in it, preview and search a markdown file, open an image file, load a session from history and continue it, send a prompt and get the reply, create a multi-session and route targeted/broadcast messages with close/reopen persistence. Still to cover: stop a running prompt; relaunch and resume; the permission banner and diff modal; fork.
- Demo media (site, README, `docs/images`) is rendered from the real app by `scripts/record-demo.sh` over `apps/desktop/tests/demo/` (ignored test, scripted models, made-up data, needs a GPU or Mesa); re-run it after a visible change and never publish captures of a real session.
- Manual acceptance (macOS), one line per §1 success criterion:
  1. Add two directories; start a session in each from Home; both stream side by side.
  2. Replies render markdown (code, table, list) while streaming and after; "Thinking… ~N tokens" shows; tool calls are cards.
  3. Auto OFF: an edit pauses with a banner; Approve, Approve & Auto and Deny work. Auto ON: no banner.
  4. Stop ends the turn; the next prompt works in the same process (same pid).
  5. Context and usage meters update after each turn.
  6. Quit, relaunch, search an old reply word: the session opens with history and resumes on a new prompt.
  7. A typo in a title (`refactr`) still finds the session.
  8. After quitting, `pgrep -f "claude -p"` is empty; after `kill -9` of the app, children exit within 2 s.
  9. Auto OFF: Edit/Write opens the diff modal (Accept applies, Cancel denies); Auto ON: "Review changes" opens it read-only; "Open in VS Code" jumps to the first changed line.
  10. The status bar shows branch and dirty state; the modified count rises within about 1 s of a tool result.
  11. Claude and Antigravity sessions stream side by side; their pickers show each provider's own models.
  12. Antigravity: Auto OFF gates a shell command; Stop keeps the pid; relaunch + resume keeps context.
  13. Fork Claude→Antigravity and back; the fork answers from the copied history.
  14. Without `agy` installed, the UI matches the Claude-only build.
