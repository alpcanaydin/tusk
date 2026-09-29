// Screen Studio–style cut of the raw demo recording: the app window on a
// soft gradient stage, rounded and shadowed, with smooth zooms toward the
// action. No captions, no audio.
import React from "react";
import { AbsoluteFill, Easing, interpolate, staticFile, useCurrentFrame, useVideoConfig } from "remotion";
import { Video } from "@remotion/media";
import { Grain } from "./fx";

export const REDDIT_FPS = 60;
export const REDDIT_FRAMES = Math.round(20.03 * REDDIT_FPS);
const SRC = { w: 2880, h: 1724 };

/** A zoom toward (x, y) (0–1 of the recording) held between `from` and `to` seconds. */
type Zoom = { from: number; to: number; z: number; x: number; y: number };
const ZOOMS: Zoom[] = [
  { from: 0.15, to: 1.2, z: 1.35, x: 0.4, y: 0.56 }, // the get-started screen
  { from: 5.05, to: 7.45, z: 1.9, x: 0.66, y: 0.2 }, // editing a cell in place
  { from: 7.95, to: 12.55, z: 1.75, x: 0.3, y: 0.12 }, // SQL + autocomplete
  { from: 14.45, to: 18.35, z: 1.22, x: 0.9, y: 0.62 }, // the AI panel: question at the bottom, steps at the top
];
const RAMP = 0.42; // seconds in and out
const ease = Easing.bezier(0.65, 0, 0.35, 1);

function camera(t: number): { z: number; x: number; y: number } {
  for (const k of ZOOMS) {
    if (t >= k.from - RAMP && t <= k.to + RAMP) {
      const pin = interpolate(t, [k.from - RAMP, k.from], [0, 1], { extrapolateLeft: "clamp", extrapolateRight: "clamp", easing: ease });
      const pout = interpolate(t, [k.to, k.to + RAMP], [1, 0], { extrapolateLeft: "clamp", extrapolateRight: "clamp", easing: ease });
      return { z: 1 + (k.z - 1) * Math.min(pin, pout), x: k.x, y: k.y };
    }
  }
  return { z: 1, x: 0.5, y: 0.5 };
}

const Stage: React.FC = () => {
  const frame = useCurrentFrame();
  const t = frame / REDDIT_FPS;
  return (
    <AbsoluteFill style={{ background: "linear-gradient(135deg, #1a1440 0%, #2b1f6b 38%, #3a2a8a 62%, #16122e 100%)" }}>
      {/* slow-moving color blobs */}
      <AbsoluteFill
        style={{
          background: `radial-gradient(38% 45% at ${22 + Math.sin(t / 3) * 6}% ${28 + Math.cos(t / 4) * 5}%, rgba(167,139,250,0.55), transparent 70%),
            radial-gradient(40% 50% at ${80 + Math.cos(t / 3.5) * 5}% ${78 + Math.sin(t / 2.8) * 6}%, rgba(96,165,250,0.40), transparent 70%),
            radial-gradient(30% 35% at ${70 + Math.sin(t / 5) * 4}% ${18 + Math.cos(t / 4.5) * 4}%, rgba(236,72,153,0.22), transparent 70%)`,
          filter: "blur(30px)",
        }}
      />
    </AbsoluteFill>
  );
};

export const RedditDemo: React.FC = () => {
  const frame = useCurrentFrame();
  const { width, height } = useVideoConfig();
  const t = frame / REDDIT_FPS;
  const cam = camera(t);
  // Window frame on the stage: as large as fits with padding.
  const pad = 70;
  const aspect = SRC.w / SRC.h;
  const fw = Math.min(width - pad * 2, (height - pad * 2) * aspect);
  const fh = fw / aspect;
  const fx = (width - fw) / 2;
  const fy = (height - fh) / 2;
  // Zoom inside the frame, pinned at the focus point, clamped so no edge shows.
  const cw = fw * cam.z;
  const ch = fh * cam.z;
  const left = Math.min(0, Math.max(fw - cw, fw * cam.x - cw * cam.x));
  const top = Math.min(0, Math.max(fh - ch, fh * cam.y - ch * cam.y));
  const intro = interpolate(frame, [0, 18], [0, 1], { extrapolateRight: "clamp", easing: Easing.bezier(0.16, 1, 0.3, 1) });
  return (
    <AbsoluteFill>
      <Stage />
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
          opacity: intro,
          scale: String(0.97 + 0.03 * intro),
        }}
      >
        <div style={{ position: "absolute", left, top, width: cw, height: ch }}>
          <Video src={staticFile("reddit/tusk-reddit-full.mp4")} muted style={{ width: "100%", height: "100%" }} />
        </div>
      </div>
      <Grain />
    </AbsoluteFill>
  );
};
