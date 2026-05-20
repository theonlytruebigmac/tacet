#!/usr/bin/env bash
# Per-detection report: duration, file_duration, distance from file end.
# Used to investigate whether proposed credits filters would correctly reject
# false positives without regressing valid detections.
#
# Usage: analyze_credits.sh <media_dir> <series_id> <season> <data_dir>
set -euo pipefail

MEDIA_DIR=$1
SERIES=$2
SEASON=$3
DATA_DIR=$4
DB="$DATA_DIR/tacet.db"

MIN_CREDITS_SEC=${MIN_CREDITS_SEC:-30}
MAX_TAIL_GAP=${MAX_TAIL_GAP:-120}

printf "filter thresholds: min_credits_seconds=%s, max_tail_gap=%ss\n\n" \
  "$MIN_CREDITS_SEC" "$MAX_TAIL_GAP"
printf "%-22s | %8s %10s %10s %6s | %3s %3s %3s\n" \
  "episode" "cred_dur" "file_dur" "tail_gap" "conf" \
  "len" "tail" "both"
printf '%s\n' "$(printf '%.0s-' {1..90})"

shopt -s nullglob
declare -A picked
total=0; pass_len=0; pass_tail=0; pass_both=0
for f in "$MEDIA_DIR"/*.mkv "$MEDIA_DIR"/*.mp4 "$MEDIA_DIR"/*.m4v; do
  base=$(basename "$f")
  ep_num=$(printf '%s' "$base" | grep -ioE 's[0-9]{1,2}e[0-9]{1,2}' | head -1 \
           | sed -E 's/[Ss]([0-9]+)[Ee]([0-9]+)/\2/' || true)
  [[ -z $ep_num ]] && continue
  ep_id=$(printf '%s-s%02de%02d' "$SERIES" "$SEASON" "$((10#$ep_num))")
  [[ ${picked[$ep_id]+x} ]] && continue
  picked[$ep_id]=1

  row=$(sqlite3 -separator '|' "$DB" \
    "SELECT IFNULL(credits_start,''), IFNULL(credits_end,''), IFNULL(credits_confidence,'') FROM markers WHERE episode_id = '$ep_id'")
  cs=$(printf '%s' "$row" | cut -d'|' -f1)
  ce=$(printf '%s' "$row" | cut -d'|' -f2)
  cc=$(printf '%s' "$row" | cut -d'|' -f3)
  [[ -z $cs ]] && continue

  fdur=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$f")
  total=$((total+1))

  read -r dur gap len_ok tail_ok both_ok <<<"$(awk \
    -v cs="$cs" -v ce="$ce" -v fd="$fdur" \
    -v minlen="$MIN_CREDITS_SEC" -v maxgap="$MAX_TAIL_GAP" 'BEGIN{
      d = ce - cs
      g = fd - ce
      lo = (d >= minlen) ? "Y" : "."
      to = (g <= maxgap) ? "Y" : "."
      bo = (lo == "Y" && to == "Y") ? "Y" : "."
      printf "%.1f %.1f %s %s %s", d, g, lo, to, bo
    }')"

  [[ $len_ok == "Y" ]] && pass_len=$((pass_len+1))
  [[ $tail_ok == "Y" ]] && pass_tail=$((pass_tail+1))
  [[ $both_ok == "Y" ]] && pass_both=$((pass_both+1))

  printf "%-22s | %8s %10.1f %10s %6.0f%% | %3s %3s %3s\n" \
    "$ep_id" "$dur" "$fdur" "$gap" "$(awk -v c="$cc" 'BEGIN{print c*100}')" \
    "$len_ok" "$tail_ok" "$both_ok"
done

printf '%s\n' "$(printf '%.0s-' {1..90})"
printf "passes len-only:   %d/%d\n" "$pass_len" "$total"
printf "passes tail-only:  %d/%d\n" "$pass_tail" "$total"
printf "passes BOTH:       %d/%d\n" "$pass_both" "$total"
