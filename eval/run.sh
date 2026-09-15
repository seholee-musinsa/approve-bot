#!/usr/bin/env bash
# Generate reviews for every PR under each guide variant (diff-only, so results
# are reproducible), and pull the sungh0lim reference ("ceiling") review once.
# Idempotent: an existing non-empty output is skipped, so reruns are cheap.
set -uo pipefail
cd "$(dirname "$0")/.."   # repo root

BIN=src-tauri/target/debug/approve-bot
VARIANTS=(baseline phase1)

key() { sed -E 's#.*/([^/]+)/pull/([0-9]+).*#\1_\2#' <<<"$1"; }

while read -r url; do
  [ -z "$url" ] && continue
  k=$(key "$url")
  read -r o r n <<<"$(sed -E 's#https://github.com/([^/]+)/([^/]+)/pull/([0-9]+).*#\1 \2 \3#' <<<"$url")"

  for v in "${VARIANTS[@]}"; do
    out="eval/out/$v/$k.json"
    if [ -s "$out" ]; then echo "skip  $v/$k"; continue; fi
    echo ">>    $v/$k"
    if ! "$BIN" review-once --pr "$url" --guide "eval/guides/$v.md" \
        >"$out" 2>"eval/out/$v/$k.err"; then
      echo "FAIL  $v/$k (see eval/out/$v/$k.err)"; rm -f "$out"
    fi
  done

  cel="eval/ceiling/$k.txt"
  if [ ! -s "$cel" ]; then
    gh api "repos/$o/$r/pulls/$n/reviews" \
      --jq '[.[]|select(.user.login=="sungh0lim" and (.body|length>40))][0].body // ""' \
      >"$cel" 2>/dev/null || true
    [ -s "$cel" ] && echo "ceil  $k ($(wc -c <"$cel") bytes)" || echo "ceil  $k MISSING"
  fi
done < eval/prs.txt
echo "== run.sh done =="
