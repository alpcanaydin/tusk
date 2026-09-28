// Shared building blocks for the Tusk launch video: backdrop, an app
// window with a keyframed camera, captions that sit next to the action.
import React from "react";
import {
  AbsoluteFill,
  Easing,
  Img,
  interpolate,
  spring,
  staticFile,
  useCurrentFrame,
  useVideoConfig,
} from "remotion";
import { loadFont as loadInter } from "@remotion/google-fonts/Inter";
import { loadFont as loadMono } from "@remotion/google-fonts/JetBrainsMono";

export const { fontFamily: SANS } = loadInter("normal", { weights: ["400", "600", "700", "800"], subsets: ["latin"] });
export const { fontFamily: MONO } = loadMono("normal", { weights: ["500"], subsets: ["latin"] });

export const ACCENT = "#8B7CF6"; // Tusk's violet accent
const EASE = Easing.bezier(0.16, 1, 0.3, 1);

export const Backdrop: React.FC = () => {
  const frame = useCurrentFrame();
  return (
    <AbsoluteFill style={{ background: "radial-gradient(120% 90% at 50% 0%, #1b1640 0%, #0c0b16 55%, #07070b 100%)" }}>
      <AbsoluteFill
        style={{
          background: `radial-gradient(40% 35% at ${30 + Math.sin(frame / 90) * 6}% ${75 + Math.cos(frame / 110) * 4}%, rgba(139,124,246,0.16), transparent 70%)`,
        }}
      />
    </AbsoluteFill>
  );
};

/** One camera keyframe: at frame `f`, zoom `z` centred on the shot point (x, y) in 0–1. */
export type Key = { f: number; z: number; x: number; y: number };

const track = (frame: number, keys: Key[], pick: (k: Key) => number) =>
  keys.length === 1
    ? pick(keys[0])
    : interpolate(frame, keys.map((k) => k.f), keys.map(pick), {
        extrapolateLeft: "clamp",
        extrapolateRight: "clamp",
        easing: Easing.bezier(0.65, 0, 0.35, 1),
      });

/**
 * A screenshot (or clip) presented as a floating macOS window. The camera
 * zooms toward (x, y); the window never leaves its frame, so a zoom reads as
 * "look here" instead of a jump cut.
 */
export const AppWindow: React.FC<{
  src?: string;
  children?: React.ReactNode;
  aspect: number; // width / height of the shot
  width?: number; // on-canvas width at zoom 1
  keys: Key[];
  enter?: boolean;
}> = ({ src, children, aspect, width = 1560, keys, enter = true }) => {
  const frame = useCurrentFrame();
  const { fps, width: W, height: H } = useVideoConfig();
  const h = width / aspect;
  const z = track(frame, keys, (k) => k.z);
  const fx = track(frame, keys, (k) => k.x);
  const fy = track(frame, keys, (k) => k.y);
  const inP = enter ? spring({ frame, fps, config: { damping: 200 }, durationInFrames: 30 }) : 1;
  // Move the focus point to the canvas centre, but never pull the window's
  // edge past the canvas edge at the current zoom.
  const maxX = Math.max(0, (width * z - W) / 2);
  const maxY = Math.max(0, (h * z - H) / 2 + 40);
  const tx = Math.max(-maxX, Math.min(maxX, (0.5 - fx) * width * z));
  const ty = Math.max(-maxY, Math.min(maxY, (0.5 - fy) * h * z));
  return (
    <AbsoluteFill style={{ alignItems: "center", justifyContent: "center" }}>
      <div
        style={{
          width,
          height: h,
          translate: `${tx}px ${ty + (1 - inP) * 60}px`,
          scale: String(z * (0.96 + 0.04 * inP)),
          opacity: inP,
          borderRadius: 22,
          overflow: "hidden",
          boxShadow: "0 40px 120px rgba(0,0,0,0.65), 0 0 0 1px rgba(255,255,255,0.09)",
          background: "#000",
        }}
      >
        {src ? <Img src={staticFile(src)} style={{ width: "100%", height: "100%", display: "block" }} /> : children}
      </div>
    </AbsoluteFill>
  );
};

/** A short caption pill placed near the action; fades in at `from`, out at `to`. */
export const Caption: React.FC<{
  text: React.ReactNode;
  from: number;
  to: number;
  x: number; // canvas px of the pill's anchor
  y: number;
  align?: "left" | "center" | "right";
  sub?: string;
}> = ({ text, from, to, x, y, align = "left", sub }) => {
  const frame = useCurrentFrame();
  const o = interpolate(frame, [from, from + 14, to - 12, to], [0, 1, 1, 0], {
    extrapolateLeft: "clamp",
    extrapolateRight: "clamp",
  });
  const dy = interpolate(frame, [from, from + 18], [16, 0], { extrapolateLeft: "clamp", extrapolateRight: "clamp", easing: EASE });
  const shift = align === "center" ? "-50%" : align === "right" ? "-100%" : "0%";
  return (
    <div
      style={{
        position: "absolute",
        left: x,
        top: y,
        translate: `${shift} ${dy}px`,
        opacity: o,
        padding: "18px 30px",
        borderRadius: 18,
        background: "rgba(12,11,22,0.82)",
        backdropFilter: "blur(14px)",
        boxShadow: "0 20px 60px rgba(0,0,0,0.5), 0 0 0 1px rgba(139,124,246,0.35)",
        fontFamily: SANS,
        color: "white",
        whiteSpace: "nowrap",
      }}
    >
      <div style={{ fontSize: 50, fontWeight: 700, letterSpacing: -0.5 }}>{text}</div>
      {sub ? <div style={{ fontSize: 30, fontWeight: 400, opacity: 0.72, marginTop: 6 }}>{sub}</div> : null}
    </div>
  );
};

export const Kbd: React.FC<{ children: React.ReactNode }> = ({ children }) => (
  <span
    style={{
      display: "inline-block",
      fontFamily: SANS,
      fontWeight: 600,
      fontSize: 42,
      padding: "2px 16px",
      margin: "0 4px",
      borderRadius: 10,
      background: "rgba(255,255,255,0.1)",
      boxShadow: "inset 0 -3px 0 rgba(255,255,255,0.12), 0 0 0 1px rgba(255,255,255,0.18)",
    }}
  >
    {children}
  </span>
);

export const Logo: React.FC<{ size: number; delay?: number }> = ({ size, delay = 0 }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const s = spring({ frame: frame - delay, fps, config: { damping: 14, stiffness: 120 } });
  return (
    <Img
      src={staticFile("logo.png")}
      style={{ width: size, height: size, scale: String(0.6 + 0.4 * s), opacity: Math.min(1, s * 1.4), filter: "drop-shadow(0 30px 60px rgba(90,80,200,0.45))" }}
    />
  );
};
