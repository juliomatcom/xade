# Testing
- Protocol fixtures: `python3 -I crates/claude/scripts/record.py <scenario> <sandbox-dir>`; scenarios are listed in the script. Record in `$(mktemp -d)`: a cwd under the home or scratchpad path leaks the user name through streamed chunks, and the script refuses such fixtures. Output is redacted (home path, user name, emails, account keys).
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
