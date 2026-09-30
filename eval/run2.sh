#!/usr/bin/env bash
# Eval v2 runner. Reviews every truth case (eval/truth/*.json) at its pinned
# commit, the way the running bot does (deep mode), N samples per case.
#
#   eval/run2.sh <label> [guide] [samples]
#
# <label>  names this run (e.g. phaseA, phaseB). Outputs go to eval/out2/<label>/.
#          The binary is whatever is built on the current branch, so a label is
#          "this code + this guide".
# [guide]  default: src-tauri/review-guide.md
# [samples] default: 2 (one sample can't tell a real gain from noise)
# SAMPLE_FROM=<n> starts at sample n, so two shells can split the samples.
#
# Idempotent: an existing non-empty output is skipped, so reruns are cheap.
set -uo pipefail
cd "$(dirname "$0")/.."   # repo root

LABEL=${1:?usage: eval/run2.sh <label> [guide] [samples]}
GUIDE=${2:-src-tauri/review-guide.md}
SAMPLES=${3:-2}
BIN=src-tauri/target/debug/approve-bot
OUT=eval/out2/$LABEL
mkdir -p "$OUT"

# `claude -p` breaks on the corporate CA when NODE_OPTIONS=--use-system-ca is set.
unset NODE_OPTIONS

for f in eval/truth/*.json; do
  k=$(basename "$f" .json)
  read -r repo pr sha kind <<<"$(python3 -c "import json,sys;d=json.load(open(sys.argv[1]));print(d['repo'],d['pr'],d['sha'],d.get('kind',''))" "$f")"
  # Escaped cases pin the PR head at merge, which is what the PR diff already
  # is — and their base branch may be deleted, which breaks base...sha.
  pin=(--sha "$sha"); [ "$kind" = escaped ] && pin=()
  url="https://github.com/$repo/pull/$pr"
  for i in $(seq "${SAMPLE_FROM:-1}" "$SAMPLES"); do
    out="$OUT/$k.s$i.json"
    if [ -s "$out" ]; then echo "skip  $LABEL/$k.s$i"; continue; fi
    echo ">>    $LABEL/$k.s$i"
    if ! "$BIN" review-once --pr "$url" ${pin[@]+"${pin[@]}"} --guide "$GUIDE" >"$out" 2>"$OUT/$k.s$i.err"; then
      echo "FAIL  $LABEL/$k.s$i (see $OUT/$k.s$i.err)"; rm -f "$out"
    fi
  done
done
echo "== run2.sh $LABEL done =="
