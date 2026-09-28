// Generate one MP3 per scene with ElevenLabs (key from ~/.config/tusk-promo.env).
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { homedir } from "node:os";
const env = Object.fromEntries(readFileSync(`${homedir()}/.config/tusk-promo.env`, "utf8")
  .split("\n").filter((l) => l.includes("=")).map((l) => [l.slice(0, l.indexOf("=")).trim(), l.slice(l.indexOf("=") + 1).trim()]));
const key = env.ELEVENLABS_API_KEY ?? env.ELEVANLAB_API_KEY;
const voiceId = "cjVigY5qzO86Huf0OWal"; // Eric: smooth, trustworthy
const force = process.argv.includes("--force");
for (const scene of JSON.parse(readFileSync("script.json", "utf8"))) {
  const out = `public/vo/${scene.id}.mp3`;
  if (existsSync(out) && !force) continue;
  const res = await fetch(`https://api.elevenlabs.io/v1/text-to-speech/${voiceId}?output_format=mp3_44100_128`, {
    method: "POST",
    headers: { "xi-api-key": key, "Content-Type": "application/json", Accept: "audio/mpeg" },
    body: JSON.stringify({ text: scene.text, model_id: "eleven_multilingual_v2",
      voice_settings: { stability: 0.55, similarity_boost: 0.8, style: 0.25, use_speaker_boost: true } }),
  });
  if (!res.ok) throw new Error(`${scene.id}: ${res.status} ${await res.text()}`);
  writeFileSync(out, Buffer.from(await res.arrayBuffer()));
  console.log("wrote", out);
}
