// Motion pieces layered on top of the kit: 3D window, cursor, spotlight,
// keycaps, kinetic type, reveal masks, light leaks, film grain.
import React from "react";
import {
  AbsoluteFill,
  Easing,
  Img,
  interpolate,
  random,
  Sequence,
  spring,
  staticFile,
  useCurrentFrame,
  useVideoConfig,
} from "remotion";
import { Audio } from "@remotion/media";
import { ACCENT, MONO, SANS } from "./kit";

export const EASE_OUT = Easing.bezier(0.16, 1, 0.3, 1);
const EASE_IO = Easing.bezier(0.65, 0, 0.35, 1);
const clamp = { extrapolateLeft: "clamp", extrapolateRight: "clamp" } as const;

/** Screenshot space: the main shots were measured at 1568 × 938. */
export const V = { w: 1568, h: 938 };
/** A rect in screenshot units → CSS percentages inside the window. */
export const pct = (x: number, y: number, w: number, h: number, base = V) => ({
  left: `${(x / base.w) * 100}%`,
  top: `${(y / base.h) * 100}%`,
  width: `${(w / base.w) * 100}%`,
  height: `${(h / base.h) * 100}%`,
});

/** Camera key: at frame `f`, zoom `z` (plus optional tilt in degrees). */
export type Cam = { f: number; z: number; rx?: number; ry?: number };
const track = (frame: number, keys: Cam[], pick: (k: Cam) => number) =>
  keys.length === 1
    ? pick(keys[0])
    : interpolate(frame, keys.map((k) => k.f), keys.map(pick), { ...clamp, easing: Easing.bezier(0.2, 0.9, 0.25, 1) });

/**
 * The app window on stage. The camera only pushes in toward `origin`
 * (0–1 in the shot): that point stays put on screen while everything grows
 * around it, so a zoom never doubles as a pan. Zoom resizes the element
 * itself instead of CSS-scaling it, so Chrome rasterizes the screenshot at
 * its final size and text stays sharp. `overlay` lives inside the window.
 */
export const Window3D: React.FC<{
  src?: string;
  children?: React.ReactNode;
  overlay?: React.ReactNode;
  aspect: number;
  width?: number;
  origin?: { x: number; y: number };
  keys: Cam[];
  enterFrom?: "below" | "depth" | "none";
  glow?: boolean;
}> = ({ src, children, overlay, aspect, width = 1560, origin = { x: 0.5, y: 0.5 }, keys, enterFrom = "below", glow = true }) => {
  const frame = useCurrentFrame();
  const { fps, width: W, height: H } = useVideoConfig();
  const h = width / aspect;
  const z = track(frame, keys, (k) => k.z);
  const rx = track(frame, keys, (k) => k.rx ?? 0);
  const ry = track(frame, keys, (k) => k.ry ?? 0);
  const p = enterFrom === "none" ? 1 : spring({ frame, fps, config: { damping: 200 }, durationInFrames: 44 });
  // The origin point's screen position at zoom 1 stays fixed as z changes.
  const px = (W - width) / 2 + origin.x * width;
  const py = (H - h) / 2 + origin.y * h;
  const left = px - origin.x * width * z;
  const top = py - origin.y * h * z;
  const enterRx = enterFrom === "below" ? (1 - p) * 30 : 0;
  const enterY = enterFrom === "below" ? (1 - p) * 360 : 0;
  const enterZ = enterFrom === "depth" ? (1 - p) * -700 : 0;
  const tilted = rx + enterRx !== 0 || ry !== 0 || enterZ !== 0;
  return (
    <AbsoluteFill style={{ perspective: tilted ? 2200 : undefined }}>
      <div
        style={{
          position: "absolute",
          left,
          top,
          width: width * z,
          height: h * z,
          transform: tilted ? `translate3d(0, ${enterY}px, ${enterZ}px) rotateX(${rx + enterRx}deg) rotateY(${ry}deg)` : undefined,
          transformOrigin: `${origin.x * 100}% ${origin.y * 100}%`,
          opacity: Math.min(1, p * 1.6),
          borderRadius: 22 * z,
          overflow: "hidden",
          background: "#000",
          boxShadow: glow
            ? `0 50px 140px rgba(0,0,0,0.7), 0 0 0 1px rgba(255,255,255,0.10), 0 0 120px rgba(139,124,246,${0.18 * p})`
            : "0 50px 140px rgba(0,0,0,0.7), 0 0 0 1px rgba(255,255,255,0.10)",
        }}
      >
        {src ? <Img src={staticFile(src)} style={{ width: "100%", height: "100%", display: "block" }} /> : children}
        {overlay ? <div style={{ position: "absolute", inset: 0 }}>{overlay}</div> : null}
        <Glint />
      </div>
    </AbsoluteFill>
  );
};

/** A soft diagonal light sweep across glass, once, early in the shot. */
const Glint: React.FC = () => {
  const frame = useCurrentFrame();
  const x = interpolate(frame, [8, 58], [-60, 160], clamp);
  return (
    <div
      style={{
        position: "absolute",
        inset: 0,
        pointerEvents: "none",
        background: `linear-gradient(115deg, transparent ${x - 18}%, rgba(255,255,255,0.10) ${x}%, transparent ${x + 18}%)`,
        mixBlendMode: "screen",
      }}
    />
  );
};

/** A macOS arrow cursor gliding through points (screenshot units) with click pulses. */
export const Cursor: React.FC<{
  path: { f: number; x: number; y: number; click?: boolean }[];
  base?: typeof V;
  size?: number;
}> = ({ path, base = V, size = 26 }) => {
  const frame = useCurrentFrame();
  const x = interpolate(frame, path.map((p) => p.f), path.map((p) => p.x), { ...clamp, easing: EASE_IO });
  const y = interpolate(frame, path.map((p) => p.f), path.map((p) => p.y), { ...clamp, easing: EASE_IO });
  const o = interpolate(frame, [path[0].f, path[0].f + 8], [0, 1], clamp);
  return (
    <>
      {path
        .filter((p) => p.click)
        .map((p) => {
          const t = frame - p.f;
          if (t < 0 || t > 24) return null;
          const r = interpolate(t, [0, 24], [4, 38], { easing: EASE_OUT });
          return (
            <div
              key={p.f}
              style={{
                position: "absolute",
                left: `${(p.x / base.w) * 100}%`,
                top: `${(p.y / base.h) * 100}%`,
                width: r * 2,
                height: r * 2,
                translate: "-50% -50%",
                borderRadius: "50%",
                border: `3px solid ${ACCENT}`,
                opacity: interpolate(t, [0, 24], [0.9, 0]),
              }}
            />
          );
        })}
      <svg
        width={size}
        height={size * 1.5}
        viewBox="0 0 20 30"
        style={{
          position: "absolute",
          left: `${(x / base.w) * 100}%`,
          top: `${(y / base.h) * 100}%`,
          opacity: o,
          scale: String(
            path.some((p) => p.click && frame >= p.f && frame < p.f + 6) ? 0.85 : 1,
          ),
          transformOrigin: "0 0",
          filter: "drop-shadow(0 3px 6px rgba(0,0,0,0.6))",
        }}
      >
        <path d="M1 1 L1 23 L6.5 17.5 L10 26 L13.5 24.5 L10 16 L17.5 16 Z" fill="white" stroke="black" strokeWidth="1.4" strokeLinejoin="round" />
      </svg>
      {path
        .filter((p) => p.click)
        .map((p) => (
          <Sequence key={`s${p.f}`} from={p.f} layout="none">
            <Audio src={staticFile("sfx/click.wav")} volume={0.35} />
          </Sequence>
        ))}
    </>
  );
};

/** Dim everything except a rounded rect, with a glowing accent ring. */
export const Spotlight: React.FC<{ x: number; y: number; w: number; h: number; from: number; to?: number; base?: typeof V; pad?: number }> = ({
  x,
  y,
  w,
  h,
  from,
  to = 99999,
  base = V,
  pad = 6,
}) => {
  const frame = useCurrentFrame();
  const o = interpolate(frame, [from, from + 18, to - 14, to], [0, 1, 1, 0], clamp);
  return (
    <div
      style={{
        position: "absolute",
        ...pct(x - pad, y - pad, w + pad * 2, h + pad * 2, base),
        borderRadius: 12,
        opacity: o,
        boxShadow: `0 0 0 3000px rgba(4,3,12,0.62), 0 0 0 2px ${ACCENT}, 0 0 40px rgba(139,124,246,0.6)`,
      }}
    />
  );
};

/** A black cover that slides away (reveals what's under it) top→bottom or left→right. */
export const Reveal: React.FC<{ x: number; y: number; w: number; h: number; from: number; dur?: number; dir?: "down" | "right"; color?: string; base?: typeof V }> = ({
  x,
  y,
  w,
  h,
  from,
  dur = 24,
  dir = "down",
  color = "#000",
  base = V,
}) => {
  const frame = useCurrentFrame();
  const p = interpolate(frame, [from, from + dur], [0, 100], { ...clamp, easing: EASE_OUT });
  return (
    <div
      style={{
        position: "absolute",
        ...pct(x, y, w, h, base),
        background: color,
        clipPath: dir === "down" ? `inset(${p}% 0 0 0)` : `inset(0 0 0 ${p}%)`,
      }}
    />
  );
};

/** Big keycaps that press down one after another. */
export const Keycaps: React.FC<{ keys: string[]; at: number; size?: number; gap?: number }> = ({ keys, at, size = 120, gap = 7 }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  return (
    <div style={{ display: "flex", gap: size * 0.18 }}>
      {keys.map((k, i) => {
        const f = frame - at - i * gap;
        const inP = spring({ frame: f, fps, config: { damping: 12, stiffness: 180 } });
        const press = interpolate(f, [12, 16, 26], [0, 1, 0], clamp);
        return (
          <div
            key={k + i}
            style={{
              width: size,
              height: size,
              borderRadius: size * 0.2,
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              fontFamily: SANS,
              fontWeight: 600,
              fontSize: size * 0.46,
              color: "white",
              background: "linear-gradient(180deg, #2a2840 0%, #1a1929 100%)",
              boxShadow: `inset 0 ${-8 + press * 6}px 0 rgba(0,0,0,0.45), inset 0 1px 0 rgba(255,255,255,0.18), 0 ${18 - press * 12}px 40px rgba(0,0,0,0.55), 0 0 0 1px rgba(139,124,246,${0.35 + press * 0.5}), 0 0 ${press * 50}px rgba(139,124,246,0.7)`,
              translate: `0 ${(1 - inP) * 60 + press * 6}px`,
              opacity: Math.min(1, inP * 1.5),
            }}
          >
            {k}
            <Sequence from={at + i * gap + 12} layout="none" durationInFrames={30}>
              <Audio src={staticFile("sfx/click.wav")} volume={0.25} />
            </Sequence>
          </div>
        );
      })}
    </div>
  );
};

/** Words that rise in from a blur, staggered. `accent` indexes get the gradient. */
export const Kinetic: React.FC<{
  text: string;
  at: number;
  size?: number;
  accent?: number[];
  stagger?: number;
  out?: number;
  weight?: number;
  align?: "left" | "center";
}> = ({ text, at, size = 96, accent = [], stagger = 5, out, weight = 800, align = "left" }) => {
  const frame = useCurrentFrame();
  const outP = out === undefined ? 0 : interpolate(frame, [out, out + 14], [0, 1], clamp);
  return (
    <div
      style={{
        fontFamily: SANS,
        fontSize: size,
        fontWeight: weight,
        letterSpacing: -size * 0.035,
        lineHeight: 1.18,
        color: "white",
        textAlign: align,
        // keeps headlines legible where they sit over busy UI
        textShadow: "0 4px 24px rgba(0,0,0,0.85), 0 1px 3px rgba(0,0,0,0.9)",
        opacity: 1 - outP,
        filter: `blur(${outP * 12}px)`,
      }}
    >
      {text.split(" ").map((w, i) => {
        const p = interpolate(frame, [at + i * stagger, at + i * stagger + 22], [0, 1], { ...clamp, easing: EASE_OUT });
        return (
          <span
            key={i}
            style={{
              display: "inline-block",
              marginRight: size * 0.24,
              opacity: p,
              translate: `0 ${(1 - p) * size * 0.5}px`,
              filter: `blur(${(1 - p) * 14}px)`,
              ...(accent.includes(i)
                ? {
                    backgroundImage: "linear-gradient(90deg, #c4b5fd 0%, #8b7cf6 45%, #60a5fa 100%)",
                    WebkitBackgroundClip: "text",
                    color: "transparent",
                    // a text-shadow would show through the transparent fill; draw it behind instead
                    textShadow: "none",
                    filter: `blur(${(1 - p) * 14}px) drop-shadow(0 4px 18px rgba(0,0,0,0.85))`,
                    // room for descenders inside the clipped background
                    paddingBottom: "0.16em",
                    marginBottom: "-0.16em",
                  }
                : {}),
            }}
          >
            {w}
          </span>
        );
      })}
    </div>
  );
};

/** One slot that flips through words (engine names), each with its color. */
export const RotatingWord: React.FC<{ words: { t: string; c: string }[]; at: number; every: number; size: number }> = ({ words, at, every, size }) => {
  const frame = useCurrentFrame();
  const f = Math.max(0, frame - at);
  const i = Math.min(words.length - 1, Math.floor(f / every));
  const local = f - i * every;
  const inP = interpolate(local, [0, 14], [0, 1], { ...clamp, easing: EASE_OUT });
  const outP = i === words.length - 1 ? 0 : interpolate(local, [every - 10, every], [0, 1], clamp);
  const w = words[i];
  return (
    <div style={{ height: size * 1.4, paddingTop: size * 0.05, overflow: "hidden", whiteSpace: "nowrap", fontFamily: SANS, fontWeight: 800, fontSize: size, letterSpacing: -size * 0.035 }}>
      <div
        style={{
          color: w.c,
          translate: `0 ${(1 - inP) * size * 0.9 - outP * size * 0.9}px`,
          filter: `blur(${(1 - inP) * 10 + outP * 10}px)`,
          textShadow: `0 0 60px ${w.c}66`,
        }}
      >
        {w.t}
      </div>
    </div>
  );
};

/** Letters of a word dropping in from a blur (logo sting). */
export const Letters: React.FC<{ text: string; at: number; size: number }> = ({ text, at, size }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  return (
    <div style={{ display: "flex", fontFamily: SANS, fontWeight: 800, fontSize: size, letterSpacing: -size * 0.03, color: "white" }}>
      {text.split("").map((ch, i) => {
        const s = spring({ frame: frame - at - i * 4, fps, config: { damping: 14, stiffness: 140 } });
        return (
          <span key={i} style={{ display: "inline-block", opacity: Math.min(1, s * 1.4), translate: `0 ${(1 - s) * 50}px`, filter: `blur(${(1 - Math.min(1, s)) * 18}px)`, scale: String(0.7 + 0.3 * s) }}>
            {ch}
          </span>
        );
      })}
    </div>
  );
};

/** Monospace typewriter with a blinking caret. */
export const Typewriter: React.FC<{ text: string; at: number; cps?: number; size: number; color?: string }> = ({ text, at, cps = 38, size, color = "rgba(255,255,255,0.72)" }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const n = Math.max(0, Math.floor(((frame - at) / fps) * cps));
  const caret = frame >= at && Math.floor(frame / 16) % 2 === 0;
  return (
    <div style={{ fontFamily: MONO, fontSize: size, color }}>
      {text.slice(0, n)}
      <span style={{ opacity: caret ? 1 : 0, color: ACCENT }}>▍</span>
    </div>
  );
};

/** A violet light flash that blooms across the cut and retracts (CSS, screen-blended). */
export const LightLeak: React.FC<{ seed?: number }> = ({ seed = 3 }) => {
  const frame = useCurrentFrame();
  const { durationInFrames } = useVideoConfig();
  const t = frame / Math.max(1, durationInFrames - 1);
  const a = Math.sin(Math.PI * t); // 0 → 1 → 0
  const dir = seed % 2 === 0 ? 1 : -1;
  const x1 = 50 + dir * (t * 70 - 35);
  const x2 = 50 - dir * (t * 50 - 25);
  return (
    <AbsoluteFill
      style={{
        mixBlendMode: "screen",
        opacity: a,
        background: `radial-gradient(45% 70% at ${x1}% 40%, rgba(167,139,250,0.95) 0%, rgba(139,92,246,0.45) 40%, transparent 70%),
          radial-gradient(35% 55% at ${x2}% 70%, rgba(236,72,153,0.55) 0%, rgba(96,165,250,0.25) 45%, transparent 70%),
          linear-gradient(${100 + dir * 20}deg, transparent 20%, rgba(255,255,255,${0.35 * a}) 50%, transparent 80%)`,
        filter: "blur(8px)",
      }}
    />
  );
};

/** Animated film grain so flat dark gradients don't band. */
export const Grain: React.FC = () => {
  const frame = useCurrentFrame();
  const seed = Math.floor(frame / 2);
  return (
    <AbsoluteFill style={{ pointerEvents: "none", opacity: 0.06, mixBlendMode: "overlay" }}>
      <svg width="100%" height="100%">
        <filter id={`g${seed}`}>
          <feTurbulence type="fractalNoise" baseFrequency="0.9" numOctaves="2" seed={seed} stitchTiles="stitch" />
        </filter>
        <rect width="100%" height="100%" filter={`url(#g${seed})`} />
      </svg>
    </AbsoluteFill>
  );
};

/** Perspective floor grid drifting toward the camera. */
export const FloorGrid: React.FC<{ opacity?: number }> = ({ opacity = 0.5 }) => {
  const frame = useCurrentFrame();
  return (
    <AbsoluteFill style={{ perspective: 900, overflow: "hidden", opacity }}>
      <div
        style={{
          position: "absolute",
          left: "-50%",
          width: "200%",
          top: "52%",
          height: "120%",
          transform: "rotateX(72deg)",
          transformOrigin: "50% 0",
          backgroundImage:
            "linear-gradient(rgba(139,124,246,0.35) 1px, transparent 1px), linear-gradient(90deg, rgba(139,124,246,0.35) 1px, transparent 1px)",
          backgroundSize: "80px 80px",
          backgroundPosition: `0 ${(frame * 1.6) % 80}px`,
          maskImage: "linear-gradient(to bottom, transparent 0%, black 30%, black 60%, transparent 100%)",
        }}
      />
    </AbsoluteFill>
  );
};

/** Deterministic pseudo-random helper for staggered layouts. */
export const rnd = (s: string) => random(s);
