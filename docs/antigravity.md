# Antigravity

Xade can run sessions on the Antigravity CLI (`agy`) next to Claude Code.

- Install `agy` and sign in by running `agy` once in a terminal. Xade never reads its credentials.
- On Home, pick the provider for a new session in the top bar. The picker shows only when both providers are available; with one, Xade behaves as before.
- Auto OFF gates shell commands and URLs. `agy` does not ask before editing files in the workspace, so edits are not gated.
- There is no quota meter and no cost for Antigravity sessions.
- Antigravity history is not imported; only sessions started in Xade appear.
- The Fork button in a session header continues the conversation on any available provider. The history is copied into a new session, and the source provider first writes a short state summary of it (a few seconds to a minute, shown as "Preparing fork…"); the new provider gets that summary, or the full transcript if the summary failed, with its first prompt. Both are plain files in a private per-user folder in the system temp folder (`xade-forks-<uid>`) and are removed when the session is deleted; if that folder is not safe to use, the full transcript goes inline instead. The summary prompt is the `fork_handoff_prompt` setting.
