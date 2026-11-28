export const faq: [string, string][] = [
  ["What is Xade?", "Xade is a fast native desktop app, written in Rust, for running several AI coding agent sessions side by side. It is short for X-provider Agentic Development Environment and is pronounced \"zade\"."],
  ["Is Xade free? Do I need a subscription?", "Yes. Xade is 100% free and open source under the MIT License. There is no Xade subscription, account, or paywall; it simply runs the official agent CLIs you already have installed."],
  ["Which AI coding agents does Xade support?", "Today Xade drives Claude Code, Codex, Antigravity and OpenCode, and a session can be forked from one to another. Local models are coming soon."],
  ["Does my code stay on my machine?", "Xade has no telemetry and saves your session history on your machine. The agents you run still send prompts and code to their model providers, under your own account with each one."],
  ["What is a Multi-Session?", "A Multi-Session is a team of models in one window, working in the same folder. You message all of them or one by its #handle, and they pass plans and results to each other with broadcasts. The members are chosen when you start it, and Auto is on for each unless you turn it off."],
  ["Do the models in a Multi-Session see everything each other writes?", "No. A model's normal reply is shown only to you. Teammates receive only what it marks as a broadcast, and those are highlighted in the Team feed. After 12 broadcast rounds in a row, delivery pauses until you resume it or send a message."],
  ["Do I need API keys?", "No. Xade runs the agent CLIs you already have installed and logged in, so your existing CLI login, settings, CLAUDE.md files, MCP servers and skills all apply."],
  ["What platforms does Xade run on?", "macOS (Apple Silicon) and Linux (x86_64 and aarch64). Linux needs a Vulkan-capable GPU driver."],
  ["Why is Xade fast?", "It is a single native Rust binary rendered on the GPU, with no Electron or web runtime."],
  ["Can I review the changes an agent makes?", "Yes. Every change appears in a side-by-side diff before it is applied. You can turn auto-approve on or off per session, and changes still show up for review afterwards."],
];
