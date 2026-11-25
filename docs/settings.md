# Settings

Xade reads `settings.json` from its data folder (`<data dir>/xade/`, or `XADE_DATA_DIR`). Open it with Cmd+, (Ctrl+, off macOS) or the gear at the top right; changes apply live. A key you leave out keeps its default, an unknown key is ignored, and a bad value shows a warning in the status bar and keeps every default. Right-click the gear to restore the defaults (the old file is kept as `settings.json.bak`).

## General keys

| Key | Default | Meaning |
|---|---|---|
| `font_family` | `"JetBrains Mono"` | The bundled font, an installed family name, or `null` for egui's default font. |
| `font_size` | `13.5` | Text size, clamped to 9-28. |
| `send_while_running` | `"queue"` | What Cmd+Enter does while a turn runs: `"queue"` sends it when the turn ends, `"steer"` sends it at once. Cmd+Shift+Enter does the other one. |
| `show_fps` | `false` | Show the frame rate at the right of the status bar. |
| `show_whitespace` | `true` | Mark every space (dot) and tab (dash) in file windows. Colored by `theme.whitespace`. |
| `input_break_all` | `true` | The session text box breaks long words anywhere (like CSS `word-break: break-all`), saving space; `false` wraps only at word boundaries. |
| `restore_windows` | `true` | Reopen the session and file windows left open when Xade starts without a path. |
| `fork_handoff_prompt` | `"prompts/fork-handoff.md"` | File with the prompt a fork asks the source provider to write its state hand-off with. A relative path is relative to the data folder; absolute paths work too. Xade creates the default file on first run and never overwrites it, so edit it freely. A missing, empty or unreadable file shows a status-bar warning and the bundled prompt is used. |
| `shortcuts` | see below | Keyboard shortcuts by action name. |
| `theme` | see below | Colors by key. |

## Shortcuts

The `shortcuts` object maps an action name to its keys: `"cmd+t"`, or two keys for a chord, `"cmd+k cmd+m"`. `cmd` is Ctrl off macOS; modifiers are `cmd`, `ctrl`, `alt` and `shift`. Two actions cannot share keys, and a single key cannot also start a chord. Esc closing a modal is fixed.

**Sessions**

| Key | Default | Action |
|---|---|---|
| `new_session` | `cmd+t` | New session in the selected workspace |
| `open_picker` | `cmd+shift+p` | Open session picker |
| `close_window` | `cmd+w` | Close the focused window |

**Files**

| Key | Default | Action |
|---|---|---|
| `new_file` | `cmd+n` | New file |
| `open_file` | `cmd+o` | Open file |
| `save` | `cmd+s` | Save the focused file |
| `save_as` | `cmd+shift+s` | Save the focused file as |
| `quick_open` | `cmd+p` | Open file by name or path |

**Chat**

| Key | Default | Action |
|---|---|---|
| `send` | `cmd+enter` | Send prompt |
| `send_other` | `cmd+shift+enter` | Send the other way while running (queue / steer) |
| `stop` | `cmd+period` | Stop the current turn |
| `toggle_auto` | `cmd+shift+a` | Toggle Auto mode (focused session) |

**Layout**

| Key | Default | Action |
|---|---|---|
| `next_window` | `ctrl+tab` | Next window |
| `prev_window` | `ctrl+shift+tab` | Previous window |
| `maximize` | `cmd+k cmd+m` | Maximize / restore the focused window |
| `retile` | `cmd+k cmd+t` | Re-tile windows |
| `toggle_sidebar` | `cmd+b` | Toggle the sidebar |
| `focus_window_1` … `focus_window_9` | `cmd+1` … `cmd+9` | Focus window 1-9 |

**General**

| Key | Default | Action |
|---|---|---|
| `find` | `cmd+f` | Find in the focused window |
| `toggle_view` | `cmd+shift+h` | Home / Workspace |
| `shortcuts` | `cmd+k cmd+s` | Keyboard shortcuts |
| `open_settings` | `cmd+comma` | Open settings (settings.json) |

## Theme

The `theme` object gives colors as `#rrggbb` (or `#rrggbbaa`) hex by key. Every key is optional.

| Key | Default | Used for |
|---|---|---|
| `bg_app` | `#0f1216` | Top bar and the app background. |
| `bg_panel` | `#161a20` | Main panel behind the Home grid. |
| `bg_window` | `#141313` | Session window frame and body. |
| `bg_file` | `#0f0e0e` | File (editor) window frame and body: a bit darker than `bg_window`. |
| `find_match` | `#cdf3e035` | Words matched by Cmd+F other than the current one (in a file, or inside the current chat message). Translucent, drawn over the text. |
| `find_current` | `#34d399` | The current Cmd+F word, in files and chats: a jade fill with its text redrawn in black. |
| `find_block` | `#cdf3e02a` | Cmd+F in a chat: the tint behind the current message (other matching messages are not tinted). |
| `whitespace` | `#3a3838` | Markers for spaces and tabs in file windows (when `show_whitespace` is on): barely there. |
| `bg_raised` | `#1f1e1e` | Cards, message bubbles and other raised surfaces. |
| `bg_terminal` | `#0d0c0c` | Tool and command cards and code blocks in the chat: darker than the window around them. |
| `bg_hover` | `#2a2828` | Hover fill, one step above `bg_raised`. |
| `border` | `#2e2c2c` | Thin borders around panels, cards and windows. |
| `border_strong` | `#4a4848` | Border of the message input: clearly visible against `bg_hover`. |
| `border_light` | `#bfb9b9` | Whitish outline of the user's message bubbles. |
| `text` | `#d6dae1` | Main text. |
| `text_muted` | `#8a93a1` | Secondary text: hints, labels and timestamps. |
| `accent` | `#059669` | Brand jade; primary buttons, links. |
| `accent_hover` | `#10b981` | Hover and active fills of accent widgets. |
| `bg_header_focus` | `#0b4a39` | Header strip of the focused window and the background of selected text: a darker take on `accent`. |
| `bg_sidebar` | `#161a20` | Left sidebar fill. |
| `bg_status` | `#0c1714` | Bottom status bar fill. |
| `ok` | `#34d399` | Success: check marks, the Idle status. |
| `ok_bg` | `#123b2f` | Muted fill behind OK-colored text (e.g. Auto: ON), avoiding a glowing solid mint. |
| `cache_fresh` | `#4fae8e` | Prompt-cache "N min left" text: a muted jade readable on card fills. |
| `warn` | `#d29922` | Needs approval, steering and files with uncommitted changes. |
| `danger` | `#f85149` | Errors, failed tool calls and destructive buttons. |
| `diff_del_bg` | `#4b1d22` | Removed lines in the diff view. |
| `diff_add_bg` | `#1a3d2a` | Added lines in the diff view. |
| `claude` | `#d97757` | The Claude origin badge and avatar tint. |
