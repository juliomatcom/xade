# Antigravity

Xade can run sessions on the Antigravity CLI (`agy`) next to Claude Code.

- Install `agy` and sign in by running `agy` once in a terminal. Xade never reads its credentials.
- On Home, pick the provider for a new session in the top bar. The picker shows only when both providers are available; with one, Xade behaves as before.
- Auto OFF gates shell commands and URLs. `agy` does not ask before editing files in the workspace, so edits are not gated.
- There is no quota meter and no cost for Antigravity sessions.
- Antigravity history is not imported; only sessions started in Xade appear.
- The Fork button in a session header continues the conversation on any available provider. The history is copied into a new session and sent once to the new provider with its first prompt.
