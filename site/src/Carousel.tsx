import { useEffect, useRef, useState } from "react";

type Slide = { label: string; src: string; alt: string; video?: boolean };

const slides: Slide[] = [
  { label: "Multi-Session", src: "/app.webp", video: true, alt: "Xade demo: a Multi-Session of #opus-1 on Claude Code, #neon-1 on Antigravity and #gpt-sol-1 on Codex plans, codes and tests a feature in one window while a solo session fixes a slow query beside it" },
  { label: "Home", src: "/app-home.webp", alt: "Xade Home view: a grid of searchable sessions, including a Multi-Session card with its member handles, with status and prompt-cache timers" },
];

const WIDTH = 2960;
const HEIGHT = 1620;
const STILL_MS = 6000;

const reducedMotion = () => matchMedia("(prefers-reduced-motion: reduce)").matches;

// The demo video plays through, then the screenshots advance. Every slide stays in the DOM for crawlers.
export default function Carousel() {
  const [i, setI] = useState(0);
  const [hovered, setHovered] = useState(false);
  const [playing, setPlaying] = useState(false);
  const video = useRef<HTMLVideoElement>(null);
  const stopped = useRef(false); // the visitor pressed pause

  const go = (n: number) => setI((n + slides.length) % slides.length);

  // The video plays only while its slide shows; it never starts by itself under reduced motion.
  useEffect(() => {
    const v = video.current;
    if (!v) return;
    if (!slides[i].video) {
      v.pause();
      v.currentTime = 0;
    } else if (!stopped.current && !reducedMotion()) {
      v.muted = true;
      v.play().catch(() => {});
    }
  }, [i]);

  useEffect(() => {
    if (slides[i].video || hovered || reducedMotion()) return;
    const id = setTimeout(() => go(i + 1), STILL_MS);
    return () => clearTimeout(id);
  }, [i, hovered]);

  const toggle = () => {
    const v = video.current;
    if (!v) return;
    stopped.current = !v.paused;
    if (v.paused) v.play().catch(() => {});
    else v.pause();
  };

  const ended = () => {
    if (hovered) video.current?.play().catch(() => {});
    else go(i + 1);
  };

  const layer = (n: number) =>
    `absolute inset-0 size-full transition-opacity ${n === i ? "opacity-100 delay-300 duration-500" : "pointer-events-none opacity-0 duration-300"}`;

  return (
    <div
      role="region"
      aria-roledescription="carousel"
      aria-label="Xade demo and screenshots"
      className="group relative"
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
      onFocus={() => setHovered(true)}
      onBlur={() => setHovered(false)}
    >
      <div className="relative w-full overflow-hidden rounded-xl border border-edge shadow-[0_30px_120px_-20px_rgb(16_185_129/0.3)]" style={{ aspectRatio: `${WIDTH} / ${HEIGHT}` }}>
        {slides.map((s, n) =>
          s.video ? (
            <video
              key={s.label}
              ref={video}
              poster={s.src}
              width={WIDTH}
              height={HEIGHT}
              muted
              playsInline
              preload="metadata"
              aria-label={s.alt}
              aria-hidden={n !== i}
              onPlay={() => setPlaying(true)}
              onPause={() => setPlaying(false)}
              onEnded={ended}
              className={layer(n)}
            >
              <source src="/multi-session.webm" type="video/webm" />
              <source src="/multi-session.mp4" type="video/mp4" />
              <img src={s.src} alt={s.alt} />
            </video>
          ) : (
            <img key={s.label} src={s.src} alt={s.alt} width={WIDTH} height={HEIGHT} loading="lazy" aria-hidden={n !== i} className={layer(n)} />
          ),
        )}
      </div>

      <button
        onClick={() => go(i - 1)}
        aria-label="Previous slide"
        className="absolute left-3 top-1/2 flex size-10 -translate-y-1/2 items-center justify-center rounded-full border border-edge bg-ink/70 text-white opacity-0 backdrop-blur transition hover:border-jade focus-visible:opacity-100 group-hover:opacity-100"
      >
        ‹
      </button>
      <button
        onClick={() => go(i + 1)}
        aria-label="Next slide"
        className="absolute right-3 top-1/2 flex size-10 -translate-y-1/2 items-center justify-center rounded-full border border-edge bg-ink/70 text-white opacity-0 backdrop-blur transition hover:border-jade focus-visible:opacity-100 group-hover:opacity-100"
      >
        ›
      </button>

      <div className="mt-5 flex items-center justify-center gap-2">
        <div className="flex items-center gap-2" role="tablist" aria-label="Choose slide">
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
        {slides[i].video && (
          <button onClick={toggle} className="rounded-full border border-edge px-4 py-1.5 text-sm text-slate-400 transition hover:border-jade hover:text-white">
            {playing ? "Pause" : "Play"}
          </button>
        )}
      </div>
    </div>
  );
}
