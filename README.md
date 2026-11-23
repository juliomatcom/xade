<p align="center"><img src="assets/xade-icon.png" alt="Xade icon" width="128"></p>

# Xade

A fast native desktop app for running several AI coding agent sessions side by side.

Xade is short for X-provider Agentic Development Environment (pronounced "zade").

**It uses the agent clients you already have configured.** Xade drives your installed `claude` CLI as is: your login, settings, `CLAUDE.md` files, MCP servers, skills and permission rules all apply. There is nothing to set up again, no API keys to paste, and your existing Claude Code sessions can be imported and resumed.

<!-- Screenshot / demo video coming soon -->

## What it does

- Run multiple Claude Code sessions in side-by-side windows.
- Sessions are saved locally and searchable. Typos are fine in titles.
- Review code changes in a side-by-side diff before they are applied, and open any file in VS Code.
- Turn auto-approve on or off per session. With it on, changes still show up for review afterwards.
- Pick the model and effort level, and see context and cost meters.
- The git branch and changed-file count stay visible in the bottom bar.

## How it works

Xade runs the official `claude` CLI that you already have installed and logged in. It talks to the CLI over its JSON streaming mode and never reads or touches your credentials. Sessions are stored in a local SQLite file. Other agent CLIs can be added later.

## Status

Early MVP. macOS first. Claude Code is the only provider today.

## Requirements

- Rust (stable)
- Claude Code CLI, installed and logged in (check with `claude auth status`)
- git
- VS Code with the `code` command (optional, for opening files from the diff view)

## Build and run

```sh
cargo run --release -p xade
```

To install it as a regular macOS app (so you can open it from Launchpad and keep it in the Dock):

```sh
./scripts/bundle-macos.sh            # builds and installs /Applications/Xade.app
```

Run the script again after pulling changes to update the app.

Xade stores its data in the `xade/` folder inside your OS data directory. To use another location, set `XADE_DATA_DIR`.

## Development

```sh
cargo test --workspace
```

Project notes for contributors and AI agents live in `wiki-llm/`. Start at `wiki-llm/index.md`.

## License

MIT. See [LICENSE](LICENSE).

Copyright © 2026 Julio César Martín
