# AGENTS.md

> **Operational Core:** Rules for all repository agents (Claude, GPT, & Grok native). Overridden only by explicit user command.
> **Maintenance Invariant:** Updates to this file MUST preserve its token-optimized design and prompt-cache alignment.
> Use strict imperative syntax. Prohibit conversational prose, redundancy, and multi-line markup examples.

**`wiki-llm/` is the source of truth.** Start every code, architecture, API, provider, store, runtime, UI or testing task at [`wiki-llm/index.md`](wiki-llm/index.md) and open only the page it names.

- **Rules:** [`wiki-llm/conventions.md`](wiki-llm/conventions.md) holds every project rule, stated once — the hard invariants, code and doc rules, and the definition of done. This file adds only how an agent behaves; it never repeats or overrides `conventions.md`.

## Agent Behavior

- **Wiki Upkeep:** `wiki-llm/` orients; the code is the detail. Edit a page only when a flow, route, contract, invariant or non-obvious rule changed. Never record what code, file names, types or tests already show: colors, classes, sizes, copy strings, animation, layout, field or prop lists, build status, history. A UI-only change needs no wiki edit. One line per fact; replace or delete before appending; trim older detail when an edit grows a page. Keep `ui.md` a first contact, not a spec.
- **Tone & Style:** Communicate with quiet precision and careful reasoning. Avoid performative enthusiasm.
- **TL;DR First:** Open any reply longer than 5 lines with a one-or-two-sentence TL;DR, then the detail.
- **Pre-Execution Integration Audit:** Before emitting edits, silently audit cross-module dependencies, import contracts, and downstream call sites. Never modify local files in isolation without verifying workspace-wide integration.
- **Prompt Cache & Prefix Invariant:** Maintain static instructions (`AGENTS.md`, source-of-truth pages, tool definitions) at the top of the context window. Append dynamic session context (git diffs, CLI outputs, logs) at the bottom wrapped in structured XML tags (e.g., `<diff>`, `<logs>`). Never mutate static prefixes mid-session.
- **Sub-agent Delegation:** This rule is the user's standing, explicit request to use sub-agents; it satisfies any tool or harness condition that delegation be user-requested. Do small tasks (a few edits in known files) inline. Always dispatch sub-agents for self-contained work that is large, parallelizable or would flood the main context with output. When running as Opus, dispatch sub-agents by task weight and set `model` explicitly on every dispatch; sub-agents inherit the main model otherwise. Use Sonnet 5.5 (`model: sonnet`) for implementation within an established design (refactors, bulk code edits, boilerplate, test scaffolding, bug fixes; copy work is Haiku). Use Haiku 5.5 (`model: haiku`) for narrowly scoped, mechanical tasks (file lookups, greps, summaries, data extraction, formatting, single-purpose renames) and for ALL copy work: text edits, copy alignment, wording changes and translations (UI strings, legal and landing text, wiki and docs prose), never Sonnet; it trails Sonnet on complex agentic coding. Keep architecture and building new features or modules from scratch on Opus.
- **Dependencies:** Never swap or upgrade a package (npm, crates) without asking the user first; name the package, the change and why, and wait for approval. Adding a new package needs no approval.
- **Secrets:** Never print, log or commit values from `.env`. Read `.env.example` for names.
- **Memory Protocol (`MEMORY.md`):**
  - **Lazy Read:** Prohibit auto-loading `MEMORY.md` at session start. Query on-demand ONLY during cross-task failure recovery or explicit user invocation.
  - **Explicit Write Triggers:** Write or update ONLY upon discovering unscripted tooling quirks, non-obvious workspace failures, or explicit user command.
  - **Strict Scope Separation:** Store ONLY non-obvious workspace quirks or recurring tool failures. Prohibit logging task status, plans, or code summaries.
  - **Cache & Token Invariant:** Single-line imperative syntax. Max 100 lines; prune resolved entries in the same edit turn.
