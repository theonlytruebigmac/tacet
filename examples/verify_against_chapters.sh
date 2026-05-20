#!/usr/bin/env bash
# Verify tacet markers against MKV chapter markers (ground truth).
#
# Usage: verify_against_chapters.sh <media_dir> <series_id> <season> <data_dir>
#
# Walks every MKV in <media_dir>, pulls the "Opening" / "Ending" chapter times
# via ffprobe, looks up the matching tacet markers in <data_dir>/tacet.db, and
# prints a side-by-side table with per-boundary deltas and IoU.
set -euo pipefail

if [[ $# -lt 4 ]]; then
  echo "usage: $0 <media_dir> <series_id> <season> <data_dir>" >&2
  exit 2
fi

MEDIA_DIR=$1
SERIES=$2
SEASON=$3
DATA_DIR=$4
DB="$DATA_DIR/tacet.db"

if [[ ! -f $DB ]]; then
  echo "no database at $DB — run a scan first" >&2
  exit 1
fi

# Title patterns that count as the intro / credits chapter (case-insensitive).
INTRO_PATTERNS='opening|intro|op '
CREDITS_PATTERNS='ending|credits|ed |outro'

iou() {
  # iou <truth_start> <truth_end> <detected_start> <detected_end>
  awk -v ts="$1" -v te="$2" -v ds="$3" -v de="$4" 'BEGIN{
    if (ts == "" || te == "" || ds == "" || de == "") { print "-"; exit }
    inter_s = (ts > ds ? ts : ds)
    inter_e = (te < de ? te : de)
    inter = inter_e - inter_s; if (inter < 0) inter = 0
    union = (te - ts) + (de - ds) - inter
    if (union <= 0) { print "-"; exit }
    printf "%.2f", inter/union
  }'
}

printf "%-32s | %-19s %-19s %-7s %-7s | %-19s %-19s %-7s %-7s\n" \
  "episode" \
  "intro_truth" "intro_detected" "Δstart" "iou" \
  "credits_truth" "credits_detected" "Δstart" "iou"
printf '%s\n' "$(printf '%.0s-' {1..170})"

total_int=0; matched_int=0; iou_int_sum=0
total_cred=0; matched_cred=0; iou_cred_sum=0

shopt -s nullglob
for f in "$MEDIA_DIR"/*.mkv "$MEDIA_DIR"/*.mp4 "$MEDIA_DIR"/*.m4v; do
  base=$(basename "$f")
  # Pull episode number from filename (S##E##).
  ep_num=$(printf '%s' "$base" | grep -ioE 's[0-9]{1,2}e[0-9]{1,2}' | head -1 \
           | sed -E 's/[Ss]([0-9]+)[Ee]([0-9]+)/\2/' || true)
  [[ -z $ep_num ]] && continue
  ep_id=$(printf '%s-s%02de%02d' "$SERIES" "$SEASON" "$((10#$ep_num))")

  chapters=$(ffprobe -v error -show_chapters "$f")
  intro_truth=$(printf '%s' "$chapters" \
    | awk -v pat="$INTRO_PATTERNS" '
        BEGIN { IGNORECASE=1 }
        /^start_time=/ { s = substr($0, 12) }
        /^end_time=/   { e = substr($0, 10) }
        /^TAG:title=/  { if (tolower($0) ~ pat) { print s "," e; exit } }
      ')
  credits_truth=$(printf '%s' "$chapters" \
    | awk -v pat="$CREDITS_PATTERNS" '
        BEGIN { IGNORECASE=1 }
        /^start_time=/ { s = substr($0, 12) }
        /^end_time=/   { e = substr($0, 10) }
        /^TAG:title=/  { if (tolower($0) ~ pat) { print s "," e; exit } }
      ')

  read -r intro_det <<<"$(sqlite3 "$DB" "SELECT printf('%.1f,%.1f', intro_start, intro_end) FROM markers WHERE episode_id = '$ep_id' AND intro_start IS NOT NULL")"
  read -r cred_det  <<<"$(sqlite3 "$DB" "SELECT printf('%.1f,%.1f', credits_start, credits_end) FROM markers WHERE episode_id = '$ep_id' AND credits_start IS NOT NULL")"

  it_s=$(printf '%s' "$intro_truth" | cut -d, -f1)
  it_e=$(printf '%s' "$intro_truth" | cut -d, -f2)
  id_s=$(printf '%s' "$intro_det" | cut -d, -f1)
  id_e=$(printf '%s' "$intro_det" | cut -d, -f2)
  ct_s=$(printf '%s' "$credits_truth" | cut -d, -f1)
  ct_e=$(printf '%s' "$credits_truth" | cut -d, -f2)
  cd_s=$(printf '%s' "$cred_det" | cut -d, -f1)
  cd_e=$(printf '%s' "$cred_det" | cut -d, -f2)

  ds_int=$(awk -v t="$it_s" -v d="$id_s" 'BEGIN{ if(t=="" || d=="") print "-"; else printf "%+.1f", d-t }')
  ds_cred=$(awk -v t="$ct_s" -v d="$cd_s" 'BEGIN{ if(t=="" || d=="") print "-"; else printf "%+.1f", d-t }')
  iou_intro=$(iou "$it_s" "$it_e" "$id_s" "$id_e")
  iou_cred=$(iou "$ct_s" "$ct_e" "$cd_s" "$cd_e")

  fmt() { awk -v s="$1" -v e="$2" 'BEGIN{ if (s=="") print "—"; else printf "%.1f→%.1f", s, e }'; }

  printf "%-32s | %-19s %-19s %-7s %-7s | %-19s %-19s %-7s %-7s\n" \
    "$ep_id" \
    "$(fmt "$it_s" "$it_e")" "$(fmt "$id_s" "$id_e")" "$ds_int" "$iou_intro" \
    "$(fmt "$ct_s" "$ct_e")" "$(fmt "$cd_s" "$cd_e")" "$ds_cred" "$iou_cred"

  if [[ -n $it_s ]]; then
    total_int=$((total_int+1))
    if [[ -n $id_s ]]; then
      matched_int=$((matched_int+1))
      iou_int_sum=$(awk -v a="$iou_int_sum" -v b="$iou_intro" 'BEGIN{ printf "%.4f", a+b }')
    fi
  fi
  if [[ -n $ct_s ]]; then
    total_cred=$((total_cred+1))
    if [[ -n $cd_s ]]; then
      matched_cred=$((matched_cred+1))
      iou_cred_sum=$(awk -v a="$iou_cred_sum" -v b="$iou_cred" 'BEGIN{ printf "%.4f", a+b }')
    fi
  fi
done

printf '%s\n' "$(printf '%.0s-' {1..170})"
printf "intro:   detected %d/%d, mean IoU (matched only) = %.3f\n" \
  "$matched_int" "$total_int" \
  "$(awk -v s="$iou_int_sum" -v n="$matched_int" 'BEGIN{ if(n) printf "%.3f", s/n; else print 0 }')"
printf "credits: detected %d/%d, mean IoU (matched only) = %.3f\n" \
  "$matched_cred" "$total_cred" \
  "$(awk -v s="$iou_cred_sum" -v n="$matched_cred" 'BEGIN{ if(n) printf "%.3f", s/n; else print 0 }')"
