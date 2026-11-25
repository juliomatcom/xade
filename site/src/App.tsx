import { useState } from "react";
import LazyLogo from "./LazyLogo";
import LazyRain from "./LazyRain";
import Carousel from "./Carousel";
import { faq } from "./faq";

// Legal name shown in the footer.
const OWNER = "Julio César Martín";
const REPO = "https://github.com/juliomatcom/xade";
const RELEASES = `${REPO}/releases/latest`;
const INSTALL = `curl -fsSL https://raw.githubusercontent.com/juliomatcom/xade/main/scripts/install.sh | sh`;

// Set to a hosted demo (e.g. "/demo.mp4") to replace the screenshot with a video.
const VIDEO_SRC = "";

const pillars = [
  ["All your subscriptions, one place", "Bring the plans you already pay for. Xade drives the agent CLIs you have installed and logged in, so there are no API keys to paste and nothing to set up again."],
  ["Hand work between providers", "Fork a session to the other provider when you hit a limit or want a second opinion. The new agent continues from a summary of the work, without leaving the workspace."],
  ["Cloud or local models", "One interface for hosted agents and models running on your own machine. Pick the right brain for each job."],
];

const features = [
  ["Side by side", "Run multiple agent sessions in side-by-side windows."],
  ["Your setup, as is", "Your login, CLAUDE.md files, MCP servers, skills and permission rules all apply."],
  ["Review before it lands", "Every change shows up in a side-by-side diff. Turn auto-approve on or off per session."],
  ["Searchable sessions", "Saved locally in SQLite and searchable. Typos are fine. Import and resume existing sessions."],
  ["Model, effort, context", "Pick the model and effort level, and watch the context and token meters."],
  ["Git in view", "The branch and changed-file count stay visible in the bottom bar."],
];

const downloads = [
  ["macOS", "Apple Silicon", "Xade-macos-arm64.zip"],
  ["Linux", "x86_64", "Xade-linux-x86_64.tar.gz"],
  ["Linux", "aarch64", "Xade-linux-aarch64.tar.gz"],
];

function GitHubIcon({ className = "size-5" }: { className?: string }) {
  return (
    <svg viewBox="0 0 16 16" className={className} fill="currentColor" aria-hidden="true">
      <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27s1.36.09 2 .27c1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8z" />
    </svg>
  );
}

function AppleIcon() {
  return (
    <svg viewBox="0 0 24 24" className="size-6" fill="currentColor" aria-hidden="true">
      <path d="M16.4 12.6c0-2.3 1.9-3.4 2-3.5-1.1-1.6-2.8-1.8-3.4-1.8-1.4-.15-2.8.85-3.5.85-.73 0-1.85-.83-3.04-.8-1.56.02-3 .91-3.8 2.3-1.63 2.8-.42 7 1.16 9.3.78 1.12 1.7 2.38 2.9 2.33 1.17-.05 1.6-.75 3-.75s1.8.75 3.03.72c1.25-.02 2.05-1.13 2.8-2.27.9-1.3 1.27-2.56 1.29-2.63-.03-.01-2.47-.95-2.5-3.76zM14.1 5.8c.64-.78 1.07-1.85.95-2.93-.92.04-2.04.61-2.7 1.39-.59.69-1.1 1.8-.97 2.85 1.03.08 2.08-.52 2.72-1.31z" />
    </svg>
  );
}

function LinuxIcon() {
  return (
    <svg viewBox="0 0 24 24" className="size-6" fill="currentColor" aria-hidden="true">
      <path d="M12 2c-2 0-3.2 1.6-3.2 4 0 1.2-.5 2.2-1.2 3.4C6.5 11 5 13 5 15.5c0 1 .3 1.8.9 2.4-.5.6-1.6 1.1-1.6 2 0 1.2 1.7 1.6 3 1.9 1 .2 1.6.9 2.5.9 1 0 1.6-.4 2.2-.4s1.2.4 2.2.4c.9 0 1.5-.7 2.5-.9 1.3-.3 3-.7 3-1.9 0-.9-1.1-1.4-1.6-2 .6-.6.9-1.4.9-2.4 0-2.5-1.5-4.5-2.6-6.1-.7-1.2-1.2-2.2-1.2-3.4 0-2.4-1.2-4-3.2-4zm-1.4 4.2c.4 0 .7.5.7 1.1s-.3 1.1-.7 1.1-.7-.5-.7-1.1.3-1.1.7-1.1zm2.8 0c.4 0 .7.5.7 1.1s-.3 1.1-.7 1.1-.7-.5-.7-1.1.3-1.1.7-1.1zM12 8.4c.8 0 1.6.5 1.6 1s-.8 1-1.6 1-1.6-.5-1.6-1 .8-1 1.6-1z" />
    </svg>
  );
}

function Copy({ text }: { text: string }) {
  const [done, setDone] = useState(false);
  return (
    <button
      onClick={() => {
        navigator.clipboard?.writeText(text).then(() => {
          setDone(true);
          setTimeout(() => setDone(false), 1500);
        });
      }}
      className="shrink-0 rounded-md border border-edge px-2.5 py-1 font-mono text-xs text-slate-400 transition hover:border-jade hover:text-jade"
    >
      {done ? "copied" : "copy"}
    </button>
  );
}

export default function App() {
  return (
    <div className="relative overflow-x-clip">
      <LazyRain />
      <div className="pointer-events-none absolute inset-x-0 top-0 h-[900px] bg-[radial-gradient(60%_50%_at_50%_20%,rgb(16_185_129/0.18),transparent)]" />

      <header className="sticky top-0 z-50 border-b border-edge/60 bg-ink/70 backdrop-blur-md">
        <div className="mx-auto flex max-w-6xl items-center justify-between px-6 py-4">
        <a href="/" className="flex items-center gap-2.5 text-lg font-semibold text-white">
          <img src="/favicon.svg" alt="Xade logo" className="size-7" /> Xade
        </a>
        <nav aria-label="Main" className="flex items-center gap-5 text-sm font-medium text-white">
          <a href="#why" className="hidden transition hover:text-jade sm:inline">Why Xade</a>
          <a href="#install" className="transition hover:text-jade">Install</a>
          <a href="#download" className="transition hover:text-jade">Download</a>
          <a href={REPO} rel="noopener" className="flex items-center gap-2 rounded-lg border border-edge px-3 py-1.5 text-white transition hover:border-slate-500">
            <GitHubIcon className="size-4" /> GitHub
          </a>
        </nav>
        </div>
      </header>

      <main className="relative mx-auto max-w-6xl px-6">
        <section className="pb-20 pt-10 text-center md:pt-16">
          <p className="mb-5 font-mono text-base font-medium uppercase tracking-[0.2em] text-jade sm:text-xl">X-provider Agentic Development Environment</p>
          <h1 className="mx-auto max-w-4xl text-4xl font-semibold leading-[1.08] tracking-tight text-white sm:text-6xl">
            Every AI coding agent you pay for,{" "}
            <span className="bg-gradient-to-r from-emerald-300 to-jade-deep bg-clip-text text-transparent">in one fast workspace.</span>
          </h1>
          <p className="mx-auto mt-6 max-w-2xl text-lg text-slate-400">
            Xade is a native desktop app that runs your AI coding agents side by side. Use the subscriptions you already have, hand tasks between providers, and review every change before it lands. Native and built in Rust, so it stays fast however many sessions you run. Simple, opinionated, never boring.
          </p>
          <div className="mt-8 flex flex-wrap justify-center gap-3">
            <a href="#download" className="rounded-lg bg-jade px-6 py-3 text-sm font-semibold text-ink transition hover:bg-emerald-300">Download Xade</a>
            <a href={REPO} rel="noopener" className="flex items-center gap-2 rounded-lg border border-edge px-6 py-3 text-sm font-semibold text-white transition hover:border-slate-500">
              <GitHubIcon /> Star on GitHub
            </a>
          </div>
          <p className="mt-4 text-sm text-slate-500">Free and open source · Powered by Rust · macOS and Linux · Pronounced "zade"</p>

          <figure className="mx-auto mt-14 max-w-6xl">
            {VIDEO_SRC ? (
              <video src={VIDEO_SRC} poster="/app.webp" autoPlay muted loop playsInline className="w-full rounded-2xl shadow-[0_30px_120px_-20px_rgb(16_185_129/0.3)]" aria-label="Xade running Claude, Codex and Google sessions side by side" />
            ) : (
              <Carousel />
            )}
          </figure>
        </section>

        <section aria-labelledby="speed" className="pb-24">
          <div className="grid items-center gap-8 rounded-3xl border border-edge bg-gradient-to-br from-slab to-ink p-8 md:grid-cols-2 md:p-12">
            <div>
              <p className="font-mono text-sm uppercase tracking-widest text-jade">Powered by Rust</p>
              <h2 id="speed" className="mt-3 text-3xl font-semibold tracking-tight text-white sm:text-4xl">Super fast, because it is native</h2>
              <p className="mt-3 leading-relaxed text-slate-400">
                No browser engine and no web runtime. Xade is a single native Rust binary that renders on the GPU, so windows open instantly and long sessions stay smooth while several agents stream at once.
              </p>
            </div>
            <dl className="grid grid-cols-3 gap-4 text-center">
              {[
                ["100%", "Rust"],
                ["0", "Electron"],
                ["GPU", "rendered"],
              ].map(([v, l]) => (
                <div key={l} className="rounded-xl border border-edge bg-ink/60 p-5">
                  <dt className="text-sm text-slate-500">{l}</dt>
                  <dd className="mt-1 text-3xl font-semibold text-jade">{v}</dd>
                </div>
              ))}
            </dl>
          </div>
        </section>

        <section id="why" className="scroll-mt-24 pb-24">
          <h2 className="mx-auto max-w-3xl text-center text-3xl font-semibold tracking-tight text-white sm:text-4xl">One place for every agent, subscription and model</h2>
          <p className="mx-auto mt-3 max-w-2xl text-center text-slate-400">
            Today Xade drives Claude Code and Antigravity, and sessions can be forked between them. It is built to take more providers, so the workspace you learn now is the one you keep.
          </p>
          <div className="mt-12 grid gap-px overflow-hidden rounded-2xl border border-edge bg-edge md:grid-cols-3">
            {pillars.map(([t, d]) => (
              <article key={t} className="bg-ink p-8">
                <h3 className="text-xl font-semibold text-white">{t}</h3>
                <p className="mt-3 leading-relaxed text-slate-400">{d}</p>
              </article>
            ))}
          </div>
          <p className="mt-3 text-center text-xs uppercase tracking-widest text-slate-600">More providers and local models are on the roadmap</p>
        </section>

        <section className="pb-24" aria-labelledby="features">
          <h2 id="features" className="text-3xl font-semibold tracking-tight text-white">Everything you need, nothing you don't</h2>
          <p className="mt-3 max-w-2xl text-slate-400">
            Simple, opinionated and really fast. Xade never reads or touches your credentials; it runs the official CLIs you already have.
          </p>
          <div className="mt-10 grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {features.map(([t, d]) => (
              <article key={t} className="rounded-xl border border-edge bg-slab/60 p-6 transition hover:border-jade-deep">
                <h3 className="font-semibold text-white">{t}</h3>
                <p className="mt-2 text-sm leading-relaxed text-slate-400">{d}</p>
              </article>
            ))}
          </div>
        </section>

        <section id="install" className="scroll-mt-24 pb-24">
          <h2 className="text-3xl font-semibold tracking-tight text-white">Install</h2>
          <p className="mt-3 text-slate-400">macOS (Apple Silicon) and Linux (x86_64, aarch64). Run it again to update.</p>
          <div className="mt-6 flex items-center gap-3 overflow-x-auto rounded-xl border border-edge bg-slab p-4">
            <code className="flex-1 whitespace-nowrap font-mono text-sm text-emerald-200">$ {INSTALL}</code>
            <Copy text={INSTALL} />
          </div>
          <p className="mt-4 text-sm text-slate-500">
            Requires the Claude Code CLI, installed and logged in, and/or the Antigravity CLI <code className="font-mono">agy</code>, signed in, plus git. VS Code with the <code className="font-mono">code</code> command is optional. Linux needs a Vulkan-capable GPU driver.
          </p>
        </section>

        <section id="download" className="scroll-mt-24 pb-24">
          <h2 className="text-3xl font-semibold tracking-tight text-white">Download</h2>
          <p className="mt-3 text-slate-400">Builds are published on every push to main.</p>
          <div className="mt-6 grid gap-4 sm:grid-cols-3">
            {downloads.map(([os, arch, file]) => (
              <a key={file} href={`${REPO}/releases/latest/download/${file}`} className="group rounded-xl border border-edge bg-slab/60 p-6 transition hover:border-jade">
                <div className="flex items-center gap-3 text-white">
                  {os === "macOS" ? <AppleIcon /> : <LinuxIcon />}
                  <span className="text-lg font-semibold">{os}</span>
                </div>
                <div className="mt-1 text-sm text-slate-400">{arch}</div>
                <div className="mt-4 font-mono text-xs text-slate-500 group-hover:text-jade">{file} ↓</div>
              </a>
            ))}
          </div>
          <div className="mt-6 flex flex-wrap items-center gap-4 text-sm text-slate-500">
            <a href={RELEASES} className="flex items-center gap-2 rounded-lg border border-edge px-4 py-2 font-medium text-white transition hover:border-slate-500">
              <GitHubIcon className="size-4" /> All releases
            </a>
            <span>The macOS build is unsigned: right-click Xade.app, then Open the first time.</span>
          </div>
        </section>

        <section id="faq" className="scroll-mt-24 pb-24" aria-labelledby="faq-title">
          <h2 id="faq-title" className="text-3xl font-semibold tracking-tight text-white">Frequently asked questions</h2>
          <div className="mt-8 divide-y divide-edge rounded-2xl border border-edge">
            {faq.map(([q, a]) => (
              <details key={q} className="group p-6" open>
                <summary className="cursor-pointer list-none text-lg font-medium text-white">
                  <h3 className="inline">{q}</h3>
                </summary>
                <p className="mt-2 leading-relaxed text-slate-400">{a}</p>
              </details>
            ))}
          </div>
          <script
            type="application/ld+json"
            dangerouslySetInnerHTML={{
              __html: JSON.stringify({
                "@context": "https://schema.org",
                "@type": "FAQPage",
                mainEntity: faq.map(([q, a]) => ({ "@type": "Question", name: q, acceptedAnswer: { "@type": "Answer", text: a } })),
              }),
            }}
          />
        </section>

        <section className="pb-24" aria-label="Built from jade">
          <div className="grid items-center gap-6 overflow-hidden rounded-3xl border border-edge bg-gradient-to-br from-slab to-ink p-8 md:grid-cols-2 md:p-12">
            <div>
              <h2 className="text-3xl font-semibold tracking-tight text-white">Open source, built in the open</h2>
              <p className="mt-3 text-slate-400">Xade is MIT licensed and written in Rust. Read the code, file an issue, or send a pull request.</p>
              <div className="mt-6 flex flex-wrap gap-3">
                <a href={REPO} rel="noopener" className="flex items-center gap-2 rounded-lg bg-jade px-5 py-2.5 text-sm font-semibold text-ink transition hover:bg-emerald-300">
                  <GitHubIcon /> Star on GitHub
                </a>
                <a href={`${REPO}/issues`} className="rounded-lg border border-edge px-5 py-2.5 text-sm font-semibold text-white transition hover:border-slate-500">Report an issue</a>
              </div>
            </div>
            <div className="mx-auto aspect-square w-full max-w-xs">
              <LazyLogo />
            </div>
          </div>
        </section>
      </main>

      <footer className="relative border-t border-edge bg-ink/80">
        <div className="mx-auto max-w-6xl px-6 py-14">
          <div className="grid gap-10 sm:grid-cols-2 lg:grid-cols-4">
            <div className="lg:col-span-2">
              <a href="/" className="flex items-center gap-2.5 text-lg font-semibold text-white">
                <img src="/favicon.svg" alt="" className="size-7" /> Xade
              </a>
              <p className="mt-4 max-w-sm text-sm leading-relaxed text-slate-400">
                One fast, native workspace for all your AI coding agents. Free and open source.
              </p>
              <a href={REPO} rel="noopener" className="mt-5 inline-flex items-center gap-2 rounded-lg border border-edge px-4 py-2 text-sm font-medium text-white transition hover:border-slate-500">
                <GitHubIcon className="size-4" /> Star on GitHub
              </a>
            </div>
            <nav aria-label="Product">
              <h2 className="text-sm font-semibold text-white">Product</h2>
              <ul className="mt-4 space-y-3 text-sm text-slate-400">
                <li><a href="#why" className="hover:text-white">Why Xade</a></li>
                <li><a href="#install" className="hover:text-white">Install</a></li>
                <li><a href="#download" className="hover:text-white">Download</a></li>
                <li><a href="#faq" className="hover:text-white">FAQ</a></li>
              </ul>
            </nav>
            <nav aria-label="Project">
              <h2 className="text-sm font-semibold text-white">Project</h2>
              <ul className="mt-4 space-y-3 text-sm text-slate-400">
                <li><a href={REPO} rel="noopener" className="hover:text-white">GitHub</a></li>
                <li><a href={RELEASES} rel="noopener" className="hover:text-white">Releases</a></li>
                <li><a href={`${REPO}/issues`} rel="noopener" className="hover:text-white">Report an issue</a></li>
                <li><a href={`${REPO}/blob/main/LICENSE`} rel="noopener" className="hover:text-white">MIT License</a></li>
              </ul>
            </nav>
          </div>
          <div className="mt-12 space-y-2 border-t border-edge pt-8 text-xs leading-relaxed text-slate-500">
            <p>© {new Date().getFullYear()} {OWNER}. All rights reserved.</p>
            <p>
              The Xade source code is released under the <a href={`${REPO}/blob/main/LICENSE`} rel="noopener" className="underline hover:text-white">MIT License</a>. Xade™ and the Xade logo are trademarks of {OWNER}; the license does not grant rights to the name or logo.
            </p>
          </div>
        </div>
      </footer>
    </div>
  );
}
