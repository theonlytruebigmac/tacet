#!/usr/bin/env bash
# Extract a short audio slice at each detected intro/credits boundary and
# concatenate them into a single audio file you can play once to spot-check.
#
# If detection is correct, the intro compilation plays the OP/theme repeatedly
# and the credits compilation plays the ED. A mislabeled boundary stands out
# instantly as dialogue or scene audio.
#
# Usage: extract_marker_clips.sh <media_dir> <series_id> <season> <data_dir> [clip_seconds]
set -euo pipefail

if [[ $# -lt 4 ]]; then
  echo "usage: $0 <media_dir> <series_id> <season> <data_dir> [clip_seconds=5]" >&2
  exit 2
fi

MEDIA_DIR=$1
SERIES=$2
SEASON=$3
DATA_DIR=$4
CLIP=${5:-5}
DB="$DATA_DIR/tacet.db"

if [[ ! -f $DB ]]; then
  echo "no database at $DB — run a scan first" >&2
  exit 1
fi

OUT_DIR="$DATA_DIR/clips/${SERIES}-s$(printf %02d "$SEASON")"
mkdir -p "$OUT_DIR"

intro_list="$OUT_DIR/intro_concat.txt"
credits_list="$OUT_DIR/credits_concat.txt"
: >"$intro_list"
: >"$credits_list"

shopt -s nullglob
declare -A picked
for f in "$MEDIA_DIR"/*.mkv "$MEDIA_DIR"/*.mp4 "$MEDIA_DIR"/*.m4v; do
  base=$(basename "$f")
  ep_num=$(printf '%s' "$base" | grep -ioE 's[0-9]{1,2}e[0-9]{1,2}' | head -1 \
           | sed -E 's/[Ss]([0-9]+)[Ee]([0-9]+)/\2/' || true)
  [[ -z $ep_num ]] && continue
  ep_id=$(printf '%s-s%02de%02d' "$SERIES" "$SEASON" "$((10#$ep_num))")
  [[ ${picked[$ep_id]+x} ]] && continue
  picked[$ep_id]=1

  row=$(sqlite3 -separator '|' "$DB" "SELECT IFNULL(intro_start,''), IFNULL(credits_start,'') FROM markers WHERE episode_id = '$ep_id'")
  intro_start=$(printf '%s' "$row" | cut -d'|' -f1)
  credits_start=$(printf '%s' "$row" | cut -d'|' -f2)

  if [[ -n $intro_start ]]; then
    out="$OUT_DIR/intro_${ep_id}.m4a"
    ffmpeg -nostdin -loglevel error -y \
      -ss "$intro_start" -t "$CLIP" -i "$f" \
      -vn -map a:0 -ac 1 -ar 44100 -c:a aac -b:a 96k \
      "$out"
    printf "file '%s'\n" "$out" >>"$intro_list"
  fi

  if [[ -n $credits_start ]]; then
    out="$OUT_DIR/credits_${ep_id}.m4a"
    ffmpeg -nostdin -loglevel error -y \
      -ss "$credits_start" -t "$CLIP" -i "$f" \
      -vn -map a:0 -ac 1 -ar 44100 -c:a aac -b:a 96k \
      "$out"
    printf "file '%s'\n" "$out" >>"$credits_list"
  fi
done

if [[ -s $intro_list ]]; then
  intro_out="$OUT_DIR/intro_compilation.m4a"
  ffmpeg -nostdin -loglevel error -y -f concat -safe 0 -i "$intro_list" -c copy "$intro_out"
  echo "intro compilation:   $intro_out"
fi
if [[ -s $credits_list ]]; then
  credits_out="$OUT_DIR/credits_compilation.m4a"
  ffmpeg -nostdin -loglevel error -y -f concat -safe 0 -i "$credits_list" -c copy "$credits_out"
  echo "credits compilation: $credits_out"
fi

echo "individual clips in:  $OUT_DIR"
