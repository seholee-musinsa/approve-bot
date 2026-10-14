# Synthetic reliability evaluation

These fictional inputs contain no private PR text, author replies, or fix commits. The severity labels are draft expectations to review with a human before making quality claims.

## Offline policy check

```sh
node eval/safety.mjs
node --test eval/*.test.mjs
```

The first command runs the actual Rust suggestion renderer against `safety.json`: 7 regression and 8 holdout cases. It checks clickable output, retained proposals, and major/blocker visibility. It needs the normal Rust build prerequisites (including `npm ci && npm run build` on a fresh checkout), but no GitHub or model credentials. The holdout split is separate fixture coverage, not evidence of model generalization.

A passing result means only that these supplied model outputs obey the deterministic policy. The engine cannot prove that model-reported dependency or verification evidence is true. It does not demonstrate improved detection, false-positive rate, or model quality.

## Optional quality samples

`truth/` contains 8 semantic examples: normalized empty values, save-time races, partial success, permission consumers, pagination, explicit policy, and a clean guard. Four are development cases and four are holdout cases. Keep holdout labels out of prompt tuning; review labels before the first measured comparison.

To run the real diff-only review engine with Claude credentials, choose a fresh label and an explicitly installed model. This incurs model costs but performs no GitHub reads or writes:

```sh
EVAL_LABEL=baseline_dev EVAL_MODEL='<model-id>' SPLIT=dev EVAL_SAMPLES=2 \
  cargo test --manifest-path src-tauri/Cargo.toml --lib \
  review::tests::synthetic_quality_samples -- --exact --ignored --nocapture
```

`EVAL_GUIDE=/absolute/path/to/guide.md` optionally pins a guide; otherwise the bundled guide is used. The harness feeds only the diff and neutral metadata to the model, never truth labels or later fixes. It stores review JSON in `eval/out2/<label>/<case>.s<n>.json`, with model, guide fingerprint, split, cost availability and completion status. Preserve the code revision alongside each run; a label alone is not provenance.

Generate candidate samples with the same model, guide and sample count unless one of those is the deliberate experimental variable. For final evaluation repeat with `SPLIT=holdout` and fresh labels. Then use the existing judge:

```sh
TRUTH_DIR=eval/synthetic/truth SPLIT=holdout \
  node eval/score2.mjs baseline_holdout candidate_holdout
```

The scorer uses embedded synthetic diffs, so it never fetches GitHub for this dataset. Judge execution requires Claude credentials and adds cost. Scores are cached per label; use fresh labels after changing reviews, truth, or judge instructions. Historical caches without safety/severity assessments report those metrics as unknown. Inspect raw judge decisions and disagreements; do not treat the judge as independent human verification.

Compare known-defect recall, incorrect claims, clean-case blocking, severity alignment, unsafe/unverifiable clickable proposals, sample counts and cost together. This small synthetic dataset is a regression tool. No actual model-quality improvement is claimed by adding it or passing the offline checks.
