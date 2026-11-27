# Codex

Xade can run sessions on the OpenAI Codex CLI (`codex`) next to Claude Code and Antigravity.

- Install `codex` and sign in by running `codex login` once in a terminal (or configure your OpenAI API key for Codex). Xade talks to `codex app-server` over stdio and never reads `~/.codex/auth.json` or any credential.
- On Home, pick the provider for a new session in the top bar. The picker appears whenever two or more providers are available; if `codex` is not installed, Xade hides it automatically.
- Auto OFF (`untrusted` approval policy) gates both shell commands and file edits; file edits open in the side-by-side diff modal for review before applying.
- Auto ON (`never` approval policy) runs commands and edits without asking, while staying inside Codex's `workspace-write` sandbox (no network access, no writes outside the session folder).
- Past non-Xade Codex sessions (such as `codex-tui` terminal threads) are imported automatically and shown with a Codex origin badge; threads created in Xade are kept under Xade origin. Deleting a Xade-origin Codex session also deletes its Codex thread.
- The top bar shows Codex usage quota for ChatGPT sign-in accounts; API-key accounts have no rate-limit readout. Context and token meters update after each turn as usual.
- You can fork a session between Codex, Claude Code, and Antigravity from the session header, and include Codex members in a Multi-Session team.
