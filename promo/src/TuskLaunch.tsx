import React, { createContext, useContext } from "react";
import { AbsoluteFill, Easing, Img, interpolate, Sequence, spring, staticFile, useCurrentFrame, useVideoConfig } from "remotion";
import { Audio, Video } from "@remotion/media";
import { linearTiming, TransitionSeries } from "@remotion/transitions";
import { fade } from "@remotion/transitions/fade";
import { slide } from "@remotion/transitions/slide";
import { pushCut } from "@remotion/transitions/push-cut";
import grid from "./grid.json";
import { ACCENT, Backdrop, MONO, SANS } from "./kit";
import { Cursor, Grain, Keycaps, Kinetic, Letters, LightLeak, Reveal, RotatingWord, Spotlight, Window3D } from "./fx";

export const FPS = 60;
const SHOT = 2880 / 1724; // full-resolution window captures
const clamp = { extrapolateLeft: "clamp", extrapolateRight: "clamp" } as const;

// ---- Music grid ----
// grid.json holds the track's bar lines in video seconds (music starts at
// `grid.offset` s into the file). Every cut sits on a downbeat, every voice
// line starts one beat after its cut, and in-scene beats (clicks, key presses,
// reveals) land on beats. The window is a steady stretch of the track (no
// build, breakdown or drop), so the music sits under the voice as a bed.
const BARS = grid.bars;
/** Video frame of beat `n` counted from the start of bar `bar` (n may be fractional). */
const beatFrame = (bar: number, n: number) => {
  const i = bar + Math.floor(n / 4);
  const r = n / 4 - Math.floor(n / 4);
  return Math.round((BARS[i] + r * (BARS[i + 1] - BARS[i])) * FPS);
};
export const BEAT = ((BARS[20] - BARS[16]) / 16) * FPS; // frames per beat (≈29)

// Scenes see beats relative to their own first frame.
const BeatCtx = createContext<(n: number) => number>(() => 0);
const useBeat = () => useContext(BeatCtx);
/** Keycaps press ~16 frames after they start entering: aim the press at a beat. */
const pressAt = (f: number) => f - 16;

// Only CSS transitions: the WebGL ones render the next scene through HTML-in-canvas,
// which drops text that has CSS filters (our blur-in headlines).
type CutKind = "none" | "fade" | "slide" | "rise" | "push" | "leak";
type Scene = { id: string; beats: number; C: React.FC; vo: { id: string; beat: number }[]; cutIn: CutKind };
const SCENES: Scene[] = [
  // Hook: the problem on screen from frame 0; the product lands by ~4 s.
  { id: "hook", beats: 7, C: () => <Hook />, vo: [{ id: "00-hook", beat: 0.15 }], cutIn: "none" },
  { id: "reveal", beats: 10, C: () => <Intro />, vo: [{ id: "01-reveal", beat: 0.5 }], cutIn: "push" },
  { id: "engines", beats: 11, C: () => <Engines />, vo: [{ id: "02-connect", beat: 1 }], cutIn: "fade" },
  { id: "grid", beats: 10, C: () => <Hero />, vo: [{ id: "03-grid", beat: 1 }], cutIn: "leak" },
  { id: "edit", beats: 12, C: () => <Edit />, vo: [{ id: "04-edit", beat: 1 }], cutIn: "fade" },
  { id: "sql", beats: 12, C: () => <Sql />, vo: [{ id: "05-sql", beat: 1 }], cutIn: "slide" },
  { id: "ai", beats: 16, C: () => <Ai />, vo: [{ id: "06-ai", beat: 1 }], cutIn: "rise" },
  {
    id: "bento",
    beats: 13,
    C: () => <Bento />,
    vo: [
      { id: "07-palette", beat: 1 },
      { id: "08-themes", beat: 6 },
    ],
    cutIn: "push",
  },
  { id: "outro", beats: 11, C: () => <Outro />, vo: [{ id: "09-outro", beat: 1 }], cutIn: "leak" },
];
const OVERLAP: Record<CutKind, number> = { none: 0, fade: 20, slide: 22, rise: 24, push: 24, leak: 0 };

// Scenes are as long as their voice line plus a breath, rounded up to whole
// beats, so a cut can land on any beat (not only a bar line).
const startBeat = SCENES.map((_, i) => SCENES.slice(0, i).reduce((n, x) => n + x.beats, 0));
const END_BEAT = SCENES.reduce((n, x) => n + x.beats, 0);
// Each transition is centred on its beat: scene i starts half its
// incoming overlap before the cut and runs until the next one ends.
const starts = SCENES.map((s, i) => beatFrame(0, startBeat[i]) - Math.round(OVERLAP[s.cutIn] / 2));
export const TOTAL = beatFrame(0, END_BEAT);
const durations = SCENES.map((s, i) => (i + 1 < SCENES.length ? starts[i + 1] + OVERLAP[SCENES[i + 1].cutIn] : TOTAL) - starts[i]);

// Presentations have different prop types; the series only needs "some presentation".
// eslint-disable-next-line @typescript-eslint/no-explicit-any
const presentation = (k: CutKind): any => {
  switch (k) {
    case "slide":
      return slide({ direction: "from-right" });
    case "rise":
      return slide({ direction: "from-bottom" });
    case "push":
      return pushCut({ flashColor: "#8b7cf6", flashOpacity: 0.35 });
    default:
      return fade();
  }
};

export const TuskLaunch: React.FC<{ music: string | null }> = ({ music }) => {
  const { durationInFrames } = useVideoConfig();
  return (
    <AbsoluteFill style={{ fontFamily: SANS, background: "#07070b" }}>
      <Backdrop />
      <TransitionSeries>
        {SCENES.flatMap((s, i) => {
          const beat = (n: number) => beatFrame(0, startBeat[i] + n) - starts[i];
          const els: React.ReactNode[] = [];
          if (s.cutIn === "leak")
            els.push(
              <TransitionSeries.Overlay key={`o${s.id}`} durationInFrames={40}>
                <LightLeak seed={i} />
              </TransitionSeries.Overlay>,
            );
          else if (s.cutIn !== "none")
            els.push(
              <TransitionSeries.Transition key={`t${s.id}`} presentation={presentation(s.cutIn)} timing={linearTiming({ durationInFrames: OVERLAP[s.cutIn] })} />,
            );
          els.push(
            <TransitionSeries.Sequence key={s.id} name={s.id} durationInFrames={durations[i]}>
              <BeatCtx.Provider value={beat}>
                <s.C />
              </BeatCtx.Provider>
              {s.vo.map((l) => (
                <Sequence key={l.id} from={beat(l.beat)} layout="none">
                  <Audio src={staticFile(`vo/${l.id}.mp3`)} />
                </Sequence>
              ))}
            </TransitionSeries.Sequence>,
          );
          return els;
        })}
      </TransitionSeries>
      <Vignette />
      <Grain />
      {music ? (
        <Audio
          src={staticFile(music)}
          trimBefore={Math.round(grid.offset * FPS)}
          volume={(f) => interpolate(f, [0, 12, durationInFrames - Math.round(BEAT * 6), durationInFrames], [0, 0.2, 0.2, 0], clamp)}
        />
      ) : null}
    </AbsoluteFill>
  );
};

const Vignette: React.FC = () => (
  <AbsoluteFill style={{ pointerEvents: "none", background: "radial-gradient(120% 100% at 50% 50%, transparent 60%, rgba(0,0,0,0.55) 100%)" }} />
);

/** Text-only scenes keep moving with a slow push (windows push via their camera). */
const Drift: React.FC<{ children: React.ReactNode; amount?: number }> = ({ children, amount = 0.04 }) => {
  const frame = useCurrentFrame();
  const { durationInFrames } = useVideoConfig();
  return <AbsoluteFill style={{ scale: String(interpolate(frame, [0, durationInFrames], [1, 1 + amount])) }}>{children}</AbsoluteFill>;
};

const Headline: React.FC<{ children: React.ReactNode; x: number; y: number; w?: number; scrim?: boolean }> = ({ children, x, y, w = 760, scrim }) => (
  <div style={{ position: "absolute", left: x, top: y, width: w }}>
    {scrim ? (
      // a soft dark pool behind text that sits on top of busy UI
      <div style={{ position: "absolute", inset: "-90px -160px", background: "radial-gradient(closest-side, rgba(6,6,12,0.88) 45%, rgba(6,6,12,0.55) 70%, transparent 100%)" }} />
    ) : null}
    <div style={{ position: "relative" }}>{children}</div>
  </div>
);

/** Two framings of one moment joined by a quick soft cut on a beat (no pans). */
const TwoShots: React.FC<{ at: number; a: React.ReactNode; b: React.ReactNode }> = ({ at, a, b }) => {
  const frame = useCurrentFrame();
  const t = interpolate(frame, [at - 6, at + 6], [0, 1], clamp);
  return (
    <AbsoluteFill>
      {t < 1 ? <AbsoluteFill style={{ opacity: 1 - t }}>{a}</AbsoluteFill> : null}
      {frame >= at - 6 ? (
        <AbsoluteFill style={{ opacity: t }}>
          <Sequence from={at - 6} layout="none">
            {b}
          </Sequence>
        </AbsoluteFill>
      ) : null}
    </AbsoluteFill>
  );
};

// ---- 1. Hook: the paywall. A generic paid client, trial over, $99/yr ----
/** A generic, greyed-out database client (not any real product). */
const GenericClient: React.FC = () => {
  const frame = useCurrentFrame();
  return (
    <div style={{ position: "absolute", inset: 0, background: "#24242b", filter: "saturate(0.5)" }}>
      <div style={{ height: 44, display: "flex", alignItems: "center", gap: 9, padding: "0 18px", background: "#2e2e36", borderBottom: "1px solid #3a3a44" }}>
        {[0, 1, 2].map((k) => (
          <div key={k} style={{ width: 13, height: 13, borderRadius: 7, background: "#44444d" }} />
        ))}
        <div style={{ marginLeft: 16, fontFamily: SANS, fontSize: 18, color: "#9a9aa6" }}>Database Client — Trial</div>
      </div>
      <div style={{ display: "flex", height: "100%" }}>
        <div style={{ width: 240, borderRight: "1px solid #34343c", padding: 18, display: "flex", flexDirection: "column", gap: 14 }}>
          {[0.8, 0.6, 0.7, 0.5, 0.65, 0.55].map((w, k) => (
            <div key={k} style={{ height: 14, width: `${w * 100}%`, borderRadius: 4, background: "rgba(255,255,255,0.10)" }} />
          ))}
        </div>
        <div style={{ flex: 1, padding: 18, display: "flex", flexDirection: "column", gap: 12 }}>
          {Array.from({ length: 12 }).map((_, k) => (
            <div key={k} style={{ height: 22, borderRadius: 4, background: `rgba(255,255,255,${0.06 + 0.02 * Math.sin((frame + k * 7) / 10)})` }} />
          ))}
        </div>
      </div>
    </div>
  );
};

/** The trial-ended upgrade sheet. */
const Paywall: React.FC<{ at: number; leaveAt: number }> = ({ at, leaveAt }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const p = spring({ frame: frame - at, fps, config: { damping: 200 }, durationInFrames: 18 });
  const leave = interpolate(frame, [leaveAt, leaveAt + 16], [0, 1], { ...clamp, easing: Easing.bezier(0.5, 0, 1, 0.5) });
  return (
    <div
      style={{
        position: "absolute",
        left: "50%",
        top: "50%",
        width: 620,
        translate: `-50% ${-50 + (1 - p) * 8 + leave * 160}%`,
        rotate: `${leave * 14}deg`,
        opacity: p * (1 - leave),
        scale: String(0.92 + 0.08 * p),
        borderRadius: 22,
        padding: "34px 40px 30px",
        background: "#2f2f37",
        boxShadow: "0 40px 120px rgba(0,0,0,0.7), 0 0 0 1px rgba(255,255,255,0.12)",
        fontFamily: SANS,
        color: "white",
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        gap: 14,
      }}
    >
      <div style={{ fontSize: 46 }}>🔒</div>
      <div style={{ fontSize: 34, fontWeight: 700 }}>Your free trial has ended</div>
      <div style={{ fontSize: 22, color: "#b4b4c0", textAlign: "center", lineHeight: 1.4 }}>Upgrade to keep your connections, tabs and exports.</div>
      <div style={{ marginTop: 6, fontSize: 56, fontWeight: 800, letterSpacing: -1 }}>
        $99<span style={{ fontSize: 26, fontWeight: 600, color: "#b4b4c0" }}> / year</span>
      </div>
      <div style={{ display: "flex", gap: 14, marginTop: 8 }}>
        <div style={{ padding: "12px 26px", borderRadius: 12, fontSize: 21, color: "#c8c8d2", background: "#3a3a44" }}>Maybe later</div>
        <div style={{ padding: "12px 26px", borderRadius: 12, fontSize: 21, fontWeight: 700, background: "#4b6bfb" }}>Buy license</div>
      </div>
    </div>
  );
};

const Hook: React.FC = () => {
  const b = useBeat();
  const frame = useCurrentFrame();
  const { durationInFrames } = useVideoConfig();
  const dim = interpolate(frame, [b(1), b(1.5)], [0, 0.45], clamp);
  return (
    <AbsoluteFill>
      <Window3D aspect={1.6} width={1500} origin={{ x: 0.5, y: 0.5 }} enterFrom="none" glow={false} keys={[{ f: 0, z: 1.0 }, { f: durationInFrames, z: 1.05 }]}
        overlay={
          <>
            <AbsoluteFill style={{ background: `rgba(0,0,0,${dim})` }} />
            <Paywall at={b(1)} leaveAt={durationInFrames - 20} />
            {/* the cursor drifts toward "Buy license"… and hesitates */}
            <Cursor base={{ w: 1500, h: 937.5 }} path={[{ f: b(1.5), x: 1080, y: 820 }, { f: b(4), x: 855, y: 640 }, { f: b(5), x: 870, y: 655 }, { f: b(6), x: 858, y: 642 }]} size={30} />
          </>
        }
      >
        <GenericClient />
      </Window3D>
      <Headline x={0} y={70} w={1920}>
        <div style={{ display: "flex", flexDirection: "column", alignItems: "center" }}>
          <div style={{ position: "relative", padding: "10px 40px", borderRadius: 24, background: "rgba(8,8,14,0.82)" }}>
            <Kinetic text="Still paying every year" at={b(0.1)} size={88} align="center" stagger={Math.round(BEAT * 0.45)} />
            <Kinetic text="to see your own data?" at={b(3)} size={88} align="center" accent={[3, 4]} stagger={Math.round(BEAT * 0.4)} />
          </div>
        </div>
      </Headline>
    </AbsoluteFill>
  );
};

// ---- 2. Reveal: the real app lands, already moving ----
const Intro: React.FC = () => {
  const b = useBeat();
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const pill = spring({ frame: frame - b(5), fps, config: { damping: 200 } });
  return (
    <AbsoluteFill>
      <Window3D
        aspect={1920 / 1150}
        origin={{ x: 0.5, y: 0.3 }}
        enterFrom="depth"
        keys={[{ f: 0, z: 0.86 }, { f: b(12), z: 0.94 }]}
      >
        <Video src={staticFile("clips/grid.mp4")} trimBefore={Math.round(0.9 * FPS)} muted style={{ width: "100%", height: "100%" }} />
      </Window3D>
      <Headline x={0} y={690} w={1920} scrim>
        <div style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 8 }}>
          <div style={{ display: "flex", alignItems: "center", gap: 26 }}>
            <Img src={staticFile("logo.png")} style={{ width: 112, height: 112, opacity: interpolate(frame, [b(0.5), b(1)], [0, 1], clamp), filter: "drop-shadow(0 16px 40px rgba(110,90,255,0.55))" }} />
            <Kinetic text="Meet Tusk." at={b(0.5)} size={112} accent={[1]} />
          </div>
          <Kinetic text="Faster. Native. Free." at={b(2.5)} size={60} weight={700} align="center" accent={[2]} stagger={Math.round(BEAT * 0.5)} />
          <div
            style={{
              marginTop: 10,
              padding: "10px 24px",
              borderRadius: 999,
              fontFamily: SANS,
              fontWeight: 600,
              fontSize: 34,
              color: "white",
              background: "rgba(139,124,246,0.28)",
              boxShadow: "0 0 0 1px rgba(139,124,246,0.6)",
              opacity: pill,
              translate: `0 ${(1 - pill) * 14}px`,
            }}
          >
            Free &amp; open source
          </div>
        </div>
      </Headline>
    </AbsoluteFill>
  );
};

// ---- 3. Engines (the kick comes in here) ----
const ENGINES: [string, string][] = [
  ["postgres", "#336791"], ["mysql", "#00758F"], ["mariadb", "#C0765A"], ["sqlite", "#44A8E0"], ["mssql", "#CC2927"], ["redis", "#DC382D"],
  ["cassandra", "#1287B1"], ["mongodb", "#47A248"], ["oracle", "#C74634"], ["redshift", "#4B6BFB"], ["cockroach", "#6933FF"], ["snowflake", "#29B5E8"],
  ["bigquery", "#669DF6"], ["duckdb", "#D4A017"], ["clickhouse", "#E8C84A"], ["dynamodb", "#4B6BFB"], ["libsql", "#2fb8a0"], ["d1", "#F38020"],
];
const Engines: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const b = useBeat();
  const size = 112;
  const gap = 26;
  return (
    <Drift>
      <Headline x={120} y={330} w={900}>
        <Kinetic text="Connect to" at={b(0)} size={104} />
        <RotatingWord
          at={b(1)}
          every={Math.round(BEAT * 1.5)}
          size={116}
          words={[
            { t: "PostgreSQL", c: "#7aa7ff" },
            { t: "MySQL", c: "#3fc1dc" },
            { t: "ClickHouse", c: "#f2d45c" },
            { t: "Snowflake", c: "#5fd0f5" },
            { t: "MongoDB", c: "#6fd36a" },
            { t: "Redis", c: "#ff6b5e" },
            { t: "20 databases.", c: "#c4b5fd" },
          ]}
        />
      </Headline>
      <AbsoluteFill style={{ perspective: 1600 }}>
        <div
          style={{
            position: "absolute",
            left: 975,
            top: 300,
            display: "grid",
            gridTemplateColumns: `repeat(6, ${size}px)`,
            gap,
            transform: `rotateY(${interpolate(frame, [0, 460], [-22, -10])}deg) rotateX(${interpolate(frame, [0, 460], [10, 4])}deg)`,
            transformStyle: "preserve-3d",
          }}
        >
          {ENGINES.map(([name, color], i) => {
            // one row of icons lands on each of the first beats
            const at = b(Math.floor(i / 6)) + (i % 6) * 3;
            const p = spring({ frame: frame - at, fps, config: { damping: 200 }, durationInFrames: 30 });
            const bob = Math.sin((frame + i * 13) / 40) * 6;
            return (
              <div
                key={name}
                style={{
                  width: size,
                  height: size,
                  borderRadius: 26,
                  background: `linear-gradient(160deg, ${color} 0%, ${color}cc 100%)`,
                  boxShadow: `0 20px 50px rgba(0,0,0,0.5), inset 0 1px 0 rgba(255,255,255,0.25), 0 0 40px ${color}44`,
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "center",
                  opacity: p,
                  transform: `translateZ(${(1 - p) * -700}px) translateY(${(1 - p) * 40 + bob}px)`,
                }}
              >
                <Img src={staticFile(`engines/${name}.svg`)} style={{ width: size * 0.5, height: size * 0.5 }} />
              </div>
            );
          })}
        </div>
      </AbsoluteFill>
    </Drift>
  );
};

// ---- 3. Hero: the real grid, righting from a tilt, then a push-in ----
const Hero: React.FC = () => {
  const b = useBeat();
  return (
    <AbsoluteFill>
      <Window3D
        aspect={1920 / 1150}
        origin={{ x: 0.45, y: 0.35 }}
        enterFrom="below"
        keys={[
          { f: 0, z: 0.94, rx: 14, ry: -16 },
          { f: b(2), z: 1.0, rx: 0, ry: 0 },
          { f: b(4), z: 1.0 },
          { f: b(6), z: 1.18 },
          { f: b(12), z: 1.22 },
        ]}
      >
        <Video src={staticFile("clips/grid.mp4")} trimBefore={Math.round(0.9 * FPS)} muted style={{ width: "100%", height: "100%" }} />
      </Window3D>
      <Headline x={140} y={800} w={1300} scrim>
        <Kinetic text="Millions of rows. Zero stutter." at={b(2)} size={84} accent={[3, 4]} />
      </Headline>
    </AbsoluteFill>
  );
};

// ---- 4. Edit: right-click a cell on the beat, then ⌘S ----
const Edit: React.FC = () => {
  const frame = useCurrentFrame();
  const b = useBeat();
  const click = b(1.5);
  return (
    <TwoShots
      at={b(6)}
      a={
        <AbsoluteFill>
          <Window3D
            src={frame < click + 4 ? "shots/grid.png" : "shots/row_menu.png"}
            aspect={SHOT}
            origin={{ x: 0.7, y: 0.36 }}
            enterFrom="none"
            keys={[
              { f: 0, z: 1.0 },
              { f: b(1.8), z: 1.02 },
              { f: b(3), z: 1.7 },
              { f: b(6), z: 1.76 },
            ]}
            overlay={
              <>
                <Spotlight x={1018} y={196} w={168} h={318} from={b(3)} />
                <Cursor path={[{ f: 0, x: 760, y: 560 }, { f: click, x: 1036, y: 186, click: true }, { f: b(3.5), x: 1040, y: 330 }, { f: b(6), x: 1040, y: 330 }]} />
              </>
            }
          />
          <Headline x={120} y={120} w={900} scrim>
            <Kinetic text="Edit anything, in place." at={b(3)} size={80} accent={[2, 3]} />
          </Headline>
        </AbsoluteFill>
      }
      b={
        <AbsoluteFill>
          <Window3D src="shots/structure.png" aspect={SHOT} origin={{ x: 0.4, y: 0.25 }} enterFrom="none" keys={[{ f: 0, z: 1.3 }, { f: b(6), z: 1.38 }]} />
          <AbsoluteFill style={{ alignItems: "center", justifyContent: "flex-end", paddingBottom: 130, gap: 26, flexDirection: "column" }}>
            {/* ⌘ and S land together on beat 7 (one beat into this shot) */}
            <Keycaps keys={["⌘", "S"]} at={pressAt(b(7) - b(6) + 6)} gap={3} size={124} />
            <Kinetic text="One transaction." at={b(8) - b(6) + 6} size={68} align="center" />
          </AbsoluteFill>
        </AbsoluteFill>
      }
    />
  );
};

// ---- 5. SQL: completions, ⌘↵ on the beat, rows fill in ----
const Sql: React.FC = () => {
  const b = useBeat();
  const cut = b(6);
  const rel = (n: number) => b(n) - cut + 6; // frames inside the second shot
  return (
    <TwoShots
      at={cut}
      a={
        <AbsoluteFill>
          <Window3D
            src="shots/completion.png"
            aspect={SHOT}
            origin={{ x: 0.4, y: 0.2 }}
            enterFrom="none"
            keys={[{ f: 0, z: 1.05 }, { f: b(1.5), z: 1.8 }, { f: cut, z: 1.85 }]}
            overlay={<Spotlight x={546} y={122} w={148} h={192} from={b(2)} />}
          />
          <Headline x={260} y={800} w={1400} scrim>
            <Kinetic text="Completions that know your schema." at={b(2)} size={70} accent={[3, 4]} align="center" />
          </Headline>
        </AbsoluteFill>
      }
      b={
        <AbsoluteFill>
          <Window3D
            src="shots/sql.png"
            aspect={SHOT}
            origin={{ x: 0.25, y: 0.3 }}
            enterFrom="none"
            keys={[{ f: 0, z: 1.35 }, { f: rel(12), z: 1.43 }]}
            overlay={<Reveal x={263} y={308} w={1305} h={600} from={rel(7)} dur={Math.round(BEAT * 1.5)} />}
          />
          <AbsoluteFill style={{ alignItems: "flex-end", justifyContent: "flex-end", padding: "0 140px 130px 0" }}>
            <Keycaps keys={["⌘", "↵"]} at={pressAt(rel(7))} gap={3} size={116} />
          </AbsoluteFill>
        </AbsoluteFill>
      }
    />
  );
};

// ---- 6. AI (the breakdown): one agent step per beat, then you run it ----
const Ai: React.FC = () => {
  const b = useBeat();
  const cut = b(10);
  const rel = (n: number) => b(n) - cut + 6;
  const step = Math.round(BEAT * 0.6);
  return (
    <TwoShots
      at={cut}
      a={
        <AbsoluteFill>
          <Window3D
            src="shots/ai_panel.png"
            aspect={SHOT}
            origin={{ x: 0.9, y: 0.3 }}
            enterFrom="none"
            keys={[{ f: 0, z: 1.0 }, { f: b(1.5), z: 1.62 }, { f: cut, z: 1.7 }]}
            overlay={
              <>
                <Reveal x={1096} y={142} w={466} h={34} from={b(3)} dur={step} dir="right" />
                <Reveal x={1096} y={176} w={466} h={33} from={b(4)} dur={step} dir="right" />
                <Reveal x={1096} y={209} w={466} h={33} from={b(5)} dur={step} dir="right" />
                <Reveal x={1096} y={242} w={466} h={34} from={b(6)} dur={step} dir="right" />
                <Reveal x={1094} y={280} w={470} h={290} from={b(7)} dur={Math.round(BEAT * 2.5)} />
                <Reveal x={263} y={72} w={824} h={454} from={99999} />
              </>
            }
          />
          <Headline x={100} y={330} w={720}>
            <Kinetic text="Just ask." at={b(2)} size={120} accent={[1]} />
            <div style={{ height: 20 }} />
            <Kinetic text="It reads your schema and writes the query." at={b(3)} size={46} weight={600} stagger={3} />
          </Headline>
        </AbsoluteFill>
      }
      b={
        <AbsoluteFill>
          <Window3D
            src="shots/ai_panel.png"
            aspect={SHOT}
            origin={{ x: 0.22, y: 0.3 }}
            enterFrom="none"
            keys={[{ f: 0, z: 1.45 }, { f: rel(16), z: 1.52 }]}
            overlay={
              <>
                <Reveal x={263} y={306} w={824} h={220} from={rel(12)} dur={Math.round(BEAT * 1.2)} />
                <Cursor path={[{ f: 0, x: 760, y: 700 }, { f: rel(12), x: 942, y: 290, click: true }, { f: rel(14), x: 960, y: 330 }]} />
              </>
            }
          />
          <Headline x={130} y={830} w={1300} scrim>
            <Kinetic text="You decide when it runs." at={rel(12)} size={76} accent={[2, 3]} />
          </Headline>
        </AbsoluteFill>
      }
    />
  );
};

// ---- 7. Bento (the drop): ⌘⇧P slams in, themes cut on every beat, then everything assembles ----
const Tile: React.FC<{ src: string; style?: React.CSSProperties }> = ({ src, style }) => (
  <div
    style={{
      position: "absolute",
      borderRadius: 20,
      overflow: "hidden",
      boxShadow: "0 30px 80px rgba(0,0,0,0.6), 0 0 0 1px rgba(255,255,255,0.10)",
      background: "#000",
      ...style,
    }}
  >
    <Img src={staticFile(src)} style={{ width: "100%", height: "100%", display: "block" }} />
  </div>
);

const BENTO: { src: string; x: number; y: number; w: number; h: number }[] = [
  // Pre-cropped to exactly w×h with Lanczos (public/bento): shown 1:1, no browser downscaling.
  { src: "bento/ai_panel.png", x: 80, y: 90, w: 1000, h: 598 },
  { src: "bento/theme_light.png", x: 1104, y: 90, w: 736, h: 287 },
  { src: "bento/palette.png", x: 1104, y: 401, w: 736, h: 287 },
  { src: "bento/theme_tokyo.png", x: 80, y: 712, w: 480, h: 278 },
  { src: "bento/history.png", x: 584, y: 712, w: 496, h: 278 },
  { src: "bento/connections.png", x: 1104, y: 712, w: 736, h: 278 },
];

const BentoGrid: React.FC<{ at: number; step?: number; blur?: number; dim?: number }> = ({ at, step = 7, blur = 0, dim = 1 }) => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  return (
    <AbsoluteFill style={{ perspective: 1800, filter: blur ? `blur(${blur}px)` : undefined, opacity: dim }}>
      {BENTO.map((t, i) => {
        const p = spring({ frame: frame - at - i * step, fps, config: { damping: 200 }, durationInFrames: 30 });
        const fromX = (i % 2 === 0 ? -1 : 1) * 260;
        return (
          <Tile
            key={t.src}
            src={t.src}
            style={{
              left: t.x,
              top: t.y,
              width: t.w,
              height: t.h,
              opacity: p,
              transform: p < 1 ? `translate3d(${(1 - p) * fromX}px, ${(1 - p) * 140}px, ${(1 - p) * -500}px) rotateX(${(1 - p) * 20}deg)` : undefined,
            }}
          />
        );
      })}
    </AbsoluteFill>
  );
};

const Bento: React.FC = () => {
  const frame = useCurrentFrame();
  const b = useBeat();
  const themesAt = b(6);
  const bentoAt = b(10);
  const themes = ["shots/theme_tokyo.png", "shots/theme_light.png", "shots/sql.png", "shots/theme_tokyo.png"];
  // one theme per beat
  const ti = Math.max(0, Math.min(themes.length - 1, [1, 2, 3].filter((k) => frame >= b(6 + k)).length));
  const beatStart = b(6 + ti);
  const pop = interpolate(frame - beatStart, [0, 10], [1.035, 1], clamp);
  return (
    <AbsoluteFill>
      {frame < themesAt ? (
        <AbsoluteFill>
          <Window3D src="shots/palette.png" aspect={SHOT} origin={{ x: 0.5, y: 0.12 }} enterFrom="depth" keys={[{ f: 0, z: 1.0 }, { f: b(2), z: 1.6 }, { f: themesAt, z: 1.66 }]} />
          <AbsoluteFill style={{ alignItems: "center", justifyContent: "flex-end", paddingBottom: 110, flexDirection: "column", gap: 24 }}>
            {/* the three keys land on the drop: beats 0, ½, 1 */}
            <Keycaps keys={["⌘", "⇧", "P"]} at={pressAt(b(0))} gap={Math.round(BEAT / 2)} size={110} />
            <Kinetic text="Every action, one keystroke away." at={b(2)} size={60} align="center" accent={[1]} />
          </AbsoluteFill>
        </AbsoluteFill>
      ) : frame < bentoAt ? (
        <AbsoluteFill>
          <Window3D src={themes[ti]} aspect={SHOT} enterFrom="none" glow={false} keys={[{ f: 0, z: pop }]} />
          <AbsoluteFill style={{ alignItems: "center", justifyContent: "flex-end", paddingBottom: 110 }}>
            <div style={{ padding: "16px 34px 22px", borderRadius: 20, background: "rgba(10,9,20,0.88)", boxShadow: `0 0 0 1px ${ACCENT}88` }}>
              <Kinetic text="30+ themes, light and dark." at={themesAt} size={64} align="center" accent={[0]} />
            </div>
          </AbsoluteFill>
        </AbsoluteFill>
      ) : (
        <BentoGrid at={bentoAt} step={Math.round(BEAT / 4)} />
      )}
    </AbsoluteFill>
  );
};

// ---- 8. Outro ----
const Outro: React.FC = () => {
  const frame = useCurrentFrame();
  const { fps } = useVideoConfig();
  const b = useBeat();
  const bg = interpolate(frame, [0, 50], [0, 1], clamp);
  const a = spring({ frame: frame - b(0.5), fps, config: { damping: 200 } });
  const c = spring({ frame: frame - b(2), fps, config: { damping: 200 } });
  const shine = interpolate(frame, [b(4), b(6)], [-40, 140], clamp);
  return (
    <AbsoluteFill>
      <AbsoluteFill style={{ scale: String(1 + interpolate(frame, [0, 500], [0, 0.04])) }}>
        <BentoGrid at={-120} blur={4 + 6 * bg} dim={1 - 0.72 * bg} />
      </AbsoluteFill>
      <AbsoluteFill style={{ background: `radial-gradient(60% 55% at 50% 50%, rgba(7,7,11,${0.85 * bg}) 0%, rgba(7,7,11,${0.4 * bg}) 100%)` }} />
      <AbsoluteFill style={{ alignItems: "center", justifyContent: "center", flexDirection: "column", gap: 14 }}>
        <Img src={staticFile("logo.png")} style={{ width: 180, height: 180, opacity: a, scale: String(0.88 + 0.12 * a), filter: "drop-shadow(0 30px 60px rgba(110,90,255,0.5))" }} />
        <Letters text="Tusk" at={b(1) - 6} size={140} />
        <div style={{ fontSize: 48, fontWeight: 600, color: "white", opacity: c, translate: `0 ${(1 - c) * 18}px` }}>
          Free &amp; open source <span style={{ color: ACCENT }}>·</span> macOS
        </div>
        <div
          style={{
            marginTop: 20,
            position: "relative",
            overflow: "hidden",
            fontFamily: MONO,
            fontSize: 40,
            color: "white",
            opacity: c,
            padding: "18px 34px",
            borderRadius: 18,
            background: "rgba(139,124,246,0.16)",
            boxShadow: "0 0 0 1px rgba(139,124,246,0.55), 0 0 60px rgba(139,124,246,0.25)",
          }}
        >
          github.com/alpcanaydin/tusk
          <div style={{ position: "absolute", inset: 0, background: `linear-gradient(110deg, transparent ${shine - 15}%, rgba(255,255,255,0.28) ${shine}%, transparent ${shine + 15}%)` }} />
        </div>
      </AbsoluteFill>
    </AbsoluteFill>
  );
};
