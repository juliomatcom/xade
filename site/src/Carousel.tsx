import { useEffect, useState } from "react";

const slides = [
  { src: "/app.webp", label: "Workspace", alt: "Xade Workspace view: Claude, Codex and Google coding agent sessions side by side, each with a diff summary" },
  { src: "/app-home.webp", label: "Home", alt: "Xade Home view: a grid of searchable sessions from every provider, with status and prompt-cache timers" },
];

// Auto-advancing, pausable screenshot carousel. Every slide stays in the DOM for crawlers.
export default function Carousel() {
  const [i, setI] = useState(0);
  const [paused, setPaused] = useState(false);

  useEffect(() => {
    if (paused || matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    const id = setInterval(() => setI((n) => (n + 1) % slides.length), 6000);
    return () => clearInterval(id);
  }, [paused]);

  const go = (n: number) => setI((n + slides.length) % slides.length);

  return (
    <div
      role="region"
      aria-roledescription="carousel"
      aria-label="Xade screenshots"
      className="group relative"
      onMouseEnter={() => setPaused(true)}
      onMouseLeave={() => setPaused(false)}
      onFocus={() => setPaused(true)}
      onBlur={() => setPaused(false)}
    >
      <div className="relative w-full drop-shadow-[0_30px_80px_rgba(16,185,129,0.25)]" style={{ aspectRatio: "2960 / 1460" }}>
        {slides.map((s, n) => (
          <img
            key={s.src}
            src={s.src}
            alt={s.alt}
            width={2960}
            height={1460}
            loading={n === 0 ? "eager" : "lazy"}
            fetchPriority={n === 0 ? "high" : "auto"}
            aria-hidden={n !== i}
            className={`absolute inset-0 size-full transition-opacity ${n === i ? "opacity-100 delay-300 duration-500" : "opacity-0 duration-300"}`}
          />
        ))}
      </div>

      <button
        onClick={() => go(i - 1)}
        aria-label="Previous screenshot"
        className="absolute left-3 top-1/2 flex size-10 -translate-y-1/2 items-center justify-center rounded-full border border-edge bg-ink/70 text-white opacity-0 backdrop-blur transition hover:border-jade focus-visible:opacity-100 group-hover:opacity-100"
      >
        ‹
      </button>
      <button
        onClick={() => go(i + 1)}
        aria-label="Next screenshot"
        className="absolute right-3 top-1/2 flex size-10 -translate-y-1/2 items-center justify-center rounded-full border border-edge bg-ink/70 text-white opacity-0 backdrop-blur transition hover:border-jade focus-visible:opacity-100 group-hover:opacity-100"
      >
        ›
      </button>

      <div className="mt-5 flex items-center justify-center gap-2" role="tablist" aria-label="Choose screenshot">
        {slides.map((s, n) => (
          <button
            key={s.label}
            role="tab"
            aria-selected={n === i}
            onClick={() => setI(n)}
            className={`rounded-full border px-4 py-1.5 text-sm transition ${n === i ? "border-jade bg-jade/15 text-white" : "border-edge text-slate-400 hover:text-white"}`}
          >
            {s.label}
          </button>
        ))}
      </div>
    </div>
  );
}
