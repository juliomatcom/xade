# Codex

Xade can run sessions on the OpenAI Codex CLI (`codex`) next to Claude Code and Antigravity.

- Install `codex` and sign in by running `codex login` once in a terminal (or configure your OpenAI API key for Codex). Xade talks to `codex app-server` over stdio and never reads `~/.codex/auth.json` or any credential.
- On Home, pick the provider for a new session in the top bar. The picker appears whenever two or more providers are available; if `codex` is not installed, Xade hides it automatically.
- Auto OFF (`untrusted` approval policy) gates both shell commands and file edits; file edits open in the side-by-side diff modal for review before applying.
- Auto ON (`never` approval policy) runs commands and edits without asking, while staying inside Codex's `workspace-write` sandbox (no network access, no writes outside the session folder).
- Codex history from terminal sessions is not imported; only sessions started in Xade appear. Deleting a Codex session in Xade also deletes its Codex thread.
- Codex does not show a quota bar in the top bar; context and token meters update after each turn as usual.
- You can fork a session between Codex, Claude Code, and Antigravity from the session header, and include Codex members in a Multi-Session team.
