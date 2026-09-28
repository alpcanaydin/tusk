#!/bin/bash
# Lay a music bed under the rendered launch video.
#   ./mix-music.sh [music file] [video] [start seconds into the music] [music dB under voice]
#   (defaults: newest file in public/music, out/tusk-launch-v7.mp4, $MUSIC_START or 0, 14)
# The edit is cut to Bouge-toi from 46.997 s (src/grid.json).
#
# The music stays at one constant level the whole way (no ducking), set a
# fixed number of dB under the voiceover, with a fade in and a fade out. The
# final mix is brought to -16 LUFS with one fixed gain (two-pass, linear), so
# nothing pumps.
set -euo pipefail
cd "$(dirname "$0")"
music="${1:-$(ls -t public/music/* | head -1)}"
video="${2:-out/tusk-launch-v7.mp4}"
start="${3:-${MUSIC_START:-0}}"
under="${4:-14}"
fade_in=2
fade_out=4
out="${video%.mp4}-music.mp4"

lufs() { ffmpeg -hide_banner -nostats "$@" -af ebur128=framelog=quiet -f null - 2>&1 | sed -n 's/^ *I: *\(-*[0-9.]*\) LUFS/\1/p' | tail -1; }
dur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$video")
voice=$(lufs -i "$video" -vn)
bed=$(lufs -ss "$start" -t "$dur" -i "$music")
gain=$(python3 -c "print(round(($voice - $under) - $bed, 2))")
end=$(python3 -c "print($start + $dur)")
fade_at=$(python3 -c "print(max(0, $dur - $fade_out))")
echo "voice ${voice} LUFS, music ${bed} LUFS → music gain ${gain} dB (${under} dB under the voice)"

mix="[1:a]atrim=${start}:${end},asetpts=N/SR/TB,volume=${gain}dB,afade=t=in:d=${fade_in},afade=t=out:st=${fade_at}:d=${fade_out},aformat=sample_rates=48000:channel_layouts=stereo[m];
  [0:a]aformat=sample_rates=48000:channel_layouts=stereo[vo];
  [vo][m]amix=inputs=2:duration=first:normalize=0"

# Pass 1: measure the mix; pass 2: apply one linear gain to -16 LUFS.
stats=$(ffmpeg -hide_banner -nostats -i "$video" -i "$music" -filter_complex "${mix},loudnorm=I=-16:TP=-1.5:LRA=11:print_format=json" -f null - 2>&1 | sed -n '/^{/,/^}/p')
get() { python3 -c "import json,sys; print(json.loads(sys.argv[1])['$1'])" "$stats"; }
ffmpeg -hide_banner -loglevel error -y -i "$video" -i "$music" -filter_complex "${mix},loudnorm=I=-16:TP=-1.5:LRA=11:linear=true:measured_I=$(get input_i):measured_TP=$(get input_tp):measured_LRA=$(get input_lra):measured_thresh=$(get input_thresh):offset=$(get target_offset)[a]" \
  -map 0:v -map "[a]" -c:v copy -c:a aac -b:a 256k -ar 48000 "$out"
echo "$out"
