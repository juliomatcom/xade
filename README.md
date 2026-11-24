<p align="center"><img src="assets/xade-icon.png" alt="Xade icon" width="128"></p>

# Xade

A fast native desktop app for running several AI coding agent sessions side by side.

Xade is short for X-provider Agentic Development Environment (pronounced "zade").

**It uses the agent clients you already have configured.** Xade drives your installed `claude` CLI as is: your login, settings, `CLAUDE.md` files, MCP servers, skills and permission rules all apply. There is nothing to set up again, no API keys to paste, and your existing Claude Code sessions can be imported and resumed.

<!-- Screenshot / demo video coming soon -->

**Simple. Opinionated. Really fast.** Xade does one thing well, makes the choices for you, and never trades speed for features. These three rules lead every change to the project.

## What it does

- Run multiple Claude Code sessions in side-by-side windows.
- Sessions are saved locally and searchable. Typos are fine in titles.
- Review code changes in a side-by-side diff before they are applied, and open any file in VS Code.
- Turn auto-approve on or off per session. With it on, changes still show up for review afterwards.
- Pick the model and effort level, and see context and cost meters.
- The git branch and changed-file count stay visible in the bottom bar.
- Auto mode and the model choice carry over to new sessions.

## How it works

Xade runs the official `claude` CLI that you already have installed and logged in. It talks to the CLI over its JSON streaming mode and never reads or touches your credentials. Sessions are stored in a local SQLite file. Other agent CLIs can be added later.

## Status

Early MVP. macOS and Linux (x86_64, aarch64). Claude Code is the only provider today.

## Install

### Command line

```sh
curl -fsSL https://raw.githubusercontent.com/juliomatcom/xade/main/scripts/install.sh | sh
```

This installs the latest build: `Xade.app` into `/Applications` on macOS (Apple Silicon), or into `~/.local/share/xade` with a desktop entry on Linux (x86_64 or aarch64). It also installs the `xade` command into `~/.local/bin`; add that folder to your PATH if the script says so. Run the same command again to update.

```sh
xade --help
xade --version
xade <path>    # opens that folder as a workspace in a new window; `xade .` for the current one
```

### Downloads

Builds are published on every push to main at https://github.com/juliomatcom/xade/releases/latest: `Xade-macos-arm64.zip` (unsigned: right-click Xade.app > Open the first time) and `Xade-linux-x86_64.tar.gz` / `Xade-linux-aarch64.tar.gz` (unpack, run `./xade`).

Linux needs a Vulkan-capable GPU driver and, for file dialogs, xdg-desktop-portal.

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

## Settings

Settings live in `settings.json` in the data folder; open it with Cmd+, (Ctrl+, on Linux) and changes apply live. Keys include `font_family`, `font_size`, `send_while_running`, `show_fps` (frame rate at the right of the status bar, default false) and `shortcuts`, which maps every keyboard shortcut, e.g. `"new_session": "cmd+t"`; chords look like `"cmd+k cmd+m"`, and `cmd` is Ctrl off macOS.

## Development

```sh
cargo test --workspace
```

Project notes for contributors and AI agents live in `wiki-llm/`. Start at `wiki-llm/index.md`.

## License

MIT. See [LICENSE](LICENSE).

Copyright © 2026 Julio César Martín
