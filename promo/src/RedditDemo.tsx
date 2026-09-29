// Screen Studio–style cut of the raw demo recording: the app window on a
// stage (gradient or photo), rounded and shadowed, with a big cursor. No
// zoom, no captions, no audio.
import React from "react";
import { AbsoluteFill, Img, staticFile, useCurrentFrame, useVideoConfig } from "remotion";
import { Video } from "@remotion/media";
import { Grain } from "./fx";

export const REDDIT_FPS = 60;
export const REDDIT_FRAMES = 1200;
const SRC = { w: 2880, h: 1724 };

// The recording hides the real cursor; its logged path (cursor-track.json,
// one point per output frame, in recording pixels) drives a big cursor.
import track from "./cursor-track.json";

const BigCursor: React.FC = () => {
  const frame = useCurrentFrame();
  const p = track.frames[Math.min(frame, track.frames.length - 1)];
  const t = frame / REDDIT_FPS;
  const size = 64; // px on the 1920 output (about 2.5x a normal cursor)
  const pulses = track.clicks
    .map((c) => t - c)
    .filter((d) => d >= 0 && d < 0.45)
    .map((d, i) => {
      const k = d / 0.45;
      return (
        <div
          key={i}
          style={{
            position: "absolute",
            left: `${(p[0] / track.w) * 100}%`,
            top: `${(p[1] / track.h) * 100}%`,
            width: 90 * (0.3 + 0.7 * k),
            height: 90 * (0.3 + 0.7 * k),
            translate: "-50% -50%",
            borderRadius: "50%",
            border: "4px solid rgba(255,255,255,0.9)",
            opacity: 1 - k,
          }}
        />
      );
    });
  const pressed = track.clicks.some((c) => t - c >= 0 && t - c < 0.12);
  return (
    <>
      {pulses}
      <svg
        width={size * 0.72}
        height={size}
        viewBox="0 0 20 28"
        style={{
          position: "absolute",
          left: `${(p[0] / track.w) * 100}%`,
          top: `${(p[1] / track.h) * 100}%`,
          scale: pressed ? "0.85" : "1",
          transformOrigin: "0 0",
          filter: "drop-shadow(0 4px 8px rgba(0,0,0,0.55))",
        }}
      >
        <path d="M1.5 1.5 L1.5 22 L6.6 17 L10 25 L13.4 23.6 L10.1 15.8 L17 15.8 Z" fill="black" stroke="white" strokeWidth="1.6" strokeLinejoin="round" />
      </svg>
    </>
  );
};

/** A built-in style, or "photo:<file in public/reddit/bg>". */
export type Bg = "aurora" | "silk" | "grid" | "mesh" | `photo:${string}`;

const clampOpts = { extrapolateLeft: "clamp", extrapolateRight: "clamp" } as const;

/** A: flowing light bands across a night sky. */
const Aurora: React.FC<{ t: number }> = ({ t }) => (
  <AbsoluteFill style={{ background: "#05060f" }}>
    <AbsoluteFill
      style={{
        background: `conic-gradient(from ${200 + t * 6}deg at 50% 120%, #0b1030 0deg, #6d28d9 40deg, #22d3ee 80deg, #0b1030 120deg, #db2777 170deg, #0b1030 220deg, #6d28d9 300deg, #0b1030 360deg)`,
        filter: "blur(90px) saturate(1.3)",
        opacity: 0.85,
        scale: "1.4",
      }}
    />
    <AbsoluteFill
      style={{
        background: `radial-gradient(60% 40% at ${50 + Math.sin(t / 2.5) * 12}% ${30 + Math.cos(t / 3) * 6}%, rgba(34,211,238,0.35), transparent 70%),
          radial-gradient(50% 35% at ${30 + Math.cos(t / 2) * 10}% ${70 + Math.sin(t / 3.2) * 6}%, rgba(168,85,247,0.45), transparent 70%)`,
        filter: "blur(40px)",
      }}
    />
    {/* stars */}
    <AbsoluteFill style={{ backgroundImage: "radial-gradient(1px 1px at 20% 30%, #fff8, transparent), radial-gradient(1px 1px at 70% 20%, #fff6, transparent), radial-gradient(1.5px 1.5px at 85% 60%, #fff5, transparent), radial-gradient(1px 1px at 40% 80%, #fff5, transparent), radial-gradient(1px 1px at 10% 70%, #fff6, transparent)", backgroundSize: "600px 400px" }} />
  </AbsoluteFill>
);

/** B: liquid silk folds (turbulence-displaced gradient). */
const Silk: React.FC<{ t: number }> = ({ t }) => (
  <AbsoluteFill style={{ background: "#120a2a", overflow: "hidden" }}>
    <svg width="100%" height="100%" viewBox="0 0 1920 1080" preserveAspectRatio="xMidYMid slice">
      <defs>
        <linearGradient id="silkg" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0%" stopColor="#1e1b4b" />
          <stop offset="30%" stopColor="#7c3aed" />
          <stop offset="50%" stopColor="#f0abfc" />
          <stop offset="68%" stopColor="#6366f1" />
          <stop offset="100%" stopColor="#0f172a" />
        </linearGradient>
        <filter id="silkf" x="-20%" y="-20%" width="140%" height="140%">
          <feTurbulence type="fractalNoise" baseFrequency="0.0016 0.0045" numOctaves="2" seed="7" />
          <feDisplacementMap in="SourceGraphic" scale="520" xChannelSelector="R" yChannelSelector="G" />
          <feGaussianBlur stdDeviation="6" />
        </filter>
      </defs>
      <g filter="url(#silkf)">
        <rect x={-400 + Math.sin(t / 4) * 120} y={-300 + Math.cos(t / 5) * 80} width="2720" height="1680" fill="url(#silkg)" transform={`rotate(${-18 + Math.sin(t / 6) * 4} 960 540)`} />
      </g>
    </svg>
    <AbsoluteFill style={{ background: "radial-gradient(80% 70% at 50% 50%, transparent 40%, rgba(5,3,15,0.55) 100%)" }} />
  </AbsoluteFill>
);

/** C: dot grid with a spotlight behind the window. */
const GridSpot: React.FC<{ t: number }> = ({ t }) => (
  <AbsoluteFill style={{ background: "#07070c" }}>
    <AbsoluteFill
      style={{
        backgroundImage: "radial-gradient(rgba(255,255,255,0.14) 1.2px, transparent 1.4px)",
        backgroundSize: "28px 28px",
        backgroundPosition: `${(t * 6) % 28}px ${(t * 3) % 28}px`,
        maskImage: "radial-gradient(70% 70% at 50% 50%, black 30%, transparent 100%)",
      }}
    />
    <AbsoluteFill style={{ background: "radial-gradient(45% 55% at 50% 50%, rgba(124,58,237,0.55), rgba(59,130,246,0.18) 45%, transparent 75%)", filter: "blur(20px)" }} />
    <AbsoluteFill style={{ background: `linear-gradient(${110 + Math.sin(t / 3) * 8}deg, transparent 35%, rgba(255,255,255,0.05) 50%, transparent 65%)` }} />
  </AbsoluteFill>
);

/** D: saturated mesh gradient. */
const Mesh: React.FC<{ t: number }> = ({ t }) => (
  <AbsoluteFill style={{ background: "#4c1d95" }}>
    <AbsoluteFill
      style={{
        background: `radial-gradient(45% 55% at ${15 + Math.sin(t / 3) * 6}% ${20 + Math.cos(t / 4) * 6}%, #f472b6 0%, transparent 60%),
          radial-gradient(50% 60% at ${85 + Math.cos(t / 3.4) * 6}% ${15 + Math.sin(t / 3.8) * 6}%, #38bdf8 0%, transparent 60%),
          radial-gradient(55% 60% at ${80 + Math.sin(t / 2.9) * 5}% ${88 + Math.cos(t / 3.1) * 5}%, #818cf8 0%, transparent 60%),
          radial-gradient(50% 55% at ${18 + Math.cos(t / 3.6) * 6}% ${85 + Math.sin(t / 2.7) * 5}%, #a855f7 0%, transparent 60%),
          radial-gradient(40% 40% at 50% 50%, #312e81 0%, transparent 70%)`,
        filter: "blur(40px) saturate(1.15)",
      }}
    />
  </AbsoluteFill>
);

/** E: a photo, slowly pushing in, darkened at the edges so the window pops. */
const Photo: React.FC<{ t: number; file: string }> = ({ t, file }) => (
  <AbsoluteFill style={{ background: "#000", overflow: "hidden" }}>
    <Img
      src={staticFile(`reddit/bg/${file}`)}
      style={{ width: "100%", height: "100%", objectFit: "cover", scale: String(1.04 + t * 0.004), filter: "saturate(1.08) brightness(0.92)" }}
    />
    <AbsoluteFill style={{ background: "radial-gradient(75% 70% at 50% 50%, transparent 45%, rgba(0,0,0,0.45) 100%)" }} />
  </AbsoluteFill>
);

const Stage: React.FC<{ bg: Bg }> = ({ bg }) => {
  const frame = useCurrentFrame();
  const t = frame / REDDIT_FPS;
  if (bg.startsWith("photo:")) return <Photo t={t} file={bg.slice(6)} />;
  if (bg === "silk") return <Silk t={t} />;
  if (bg === "grid") return <GridSpot t={t} />;
  if (bg === "mesh") return <Mesh t={t} />;
  return <Aurora t={t} />;
};
void clampOpts;

export const RedditDemo: React.FC<{ bg: Bg }> = ({ bg }) => {
  const { width, height } = useVideoConfig();
  // Window frame on the stage: as large as fits with padding.
  const pad = 48;
  const aspect = SRC.w / SRC.h;
  const fw = Math.min(width - pad * 2, (height - pad * 2) * aspect);
  const fh = fw / aspect;
  const fx = (width - fw) / 2;
  const fy = (height - fh) / 2;
  return (
    <AbsoluteFill>
      <Stage bg={bg} />
      <div
        style={{
          position: "absolute",
          left: fx,
          top: fy,
          width: fw,
          height: fh,
          borderRadius: 18,
          overflow: "hidden",
          background: "#000",
          boxShadow: "0 40px 120px rgba(8,4,30,0.65), 0 12px 40px rgba(0,0,0,0.45), 0 0 0 1px rgba(255,255,255,0.12)",
        }}
      >
        <Video src={staticFile("reddit/tusk-reddit-full.mp4")} muted style={{ width: "100%", height: "100%", display: "block" }} />
        <BigCursor />
      </div>
      <Grain />
    </AbsoluteFill>
  );
};
