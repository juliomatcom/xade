# OpenCode

Xade can run sessions on OpenCode (`opencode`) next to Claude Code, Antigravity and Codex.

- Needs OpenCode 2.0 or newer. Install with `curl -fsSL https://opencode.ai/v2/install | bash`. Connect providers or sign in with `opencode auth login`. Free models work without signing in; connected providers and an OpenCode Go subscription add their models. Xade talks to `opencode acp` and never reads OpenCode's credentials.
- The picker on Home offers OpenCode when installed; if it is not installed, it is hidden.
- Models are listed as `provider/model`. The effort picker shows all levels; a level the model does not support is not applied.
- Auto off: Xade makes OpenCode ask before shell commands, file changes, web fetches and searches, code execution and image generation. File changes open in the diff viewer for review. Denying a request ends the turn.
- With Auto off, a command you set to `deny` in your OpenCode config prompts you instead of being blocked; approving the prompt runs it, so deny the prompt to keep it blocked. Xade turns this on by setting the `OPENCODE_CONFIG_CONTENT` environment variable for that session. If you already set it, Xade adds to it; it must be plain JSON (no comments), or the session does not start. The session also uses OpenCode's `build` agent.
- Not asked about even with Auto off: MCP tools, custom subagents whose own rules allow an action, and reading files (your OpenCode rules still apply to reading).
- Auto on: your OpenCode config applies unchanged, so `deny` stays blocked; anything it would ask about is approved.
- Changing Auto restarts the session's OpenCode process once the session is idle, resuming the same conversation. During a running turn, turning Auto on keeps asking until the turn ends; turning Auto off keeps running without asking until the turn ends.
- Background subagents: their report appears as a new turn. A restart (changing Auto, model or effort while the session looks idle) stops a subagent still working silently. Stop ends a visible turn and its subagents; a silent subagent with no turn showing cannot be stopped with Stop: close the session.
- With Auto on, the review of an overwritten file shows the whole new content, because OpenCode does not send the old text. Patch reviews show changed lines without real line numbers.
- Deleting a session started in Xade also deletes the OpenCode session.
- You can fork between OpenCode and the other providers, and add OpenCode members to a Multi-Session team.
- Not in this version: importing OpenCode sessions made outside Xade, and a live usage quota readout (clicking OpenCode's top-bar meter opens `https://opencode.ai/console` in your browser). No cost figure is shown.
