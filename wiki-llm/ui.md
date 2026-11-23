# UI
- Home and Workspace views share the top bar, status bar and sidebar panels; only the central panel differs (`apps/desktop/src/app.rs`). Design reference: `design/workspace-mock.png`.
- All UI state lives in `App` (per-session `SessionVm`, home state, git map), never in a view, so background sessions keep updating.
- Manager events, query results and git results reach the UI through one std mpsc channel; every sender calls `request_repaint()`, and `App::logic` drains it each frame. Never block or `.await` in a frame.
- Reads (home list, search, workspaces) run as tasks on the tokio runtime and return tagged with a generation; stale results are dropped.
- Git status is `git status --porcelain=v2` of the active directory (focused session cwd in Workspace view, else the selected or most recent workspace); one in-flight run per directory.
- Git refreshes on active-directory change, every 5 s while the window is focused, and after any `ToolResult` or `TurnComplete`.
