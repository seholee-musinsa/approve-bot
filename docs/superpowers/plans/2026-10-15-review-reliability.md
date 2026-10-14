# Review reliability implementation plan

> For agentic workers: implement independent file-owned tasks in parallel, integrate sequentially, and verify the whole branch. Use test-driven-development for behavior changes.

**Goal:** Apply the approved P0/P1 review improvement backlog: safe suggestions, reliable feedback, fresh publication, dependency context, calibration, durable reporting, and bounded retry/review work.

**Architecture:** Keep the existing Rust review pipeline and React app. Add focused policy and audit modules; use conservative defaults for legacy model output. Preserve existing configuration and avoid live GitHub writes during verification.

**Tech Stack:** Rust, Tokio, serde, chrono, Tauri 2, React/TypeScript, Node evaluation scripts.

**Spec:** User-approved review analysis of 2026-10-14 (private evidence in `.context/review-report-2026-10-15/report-2026-10-14.md`). The portable requirements are reproduced below without internal PR content.

## Global constraints

- Keep the current branch and existing Conductor workspace.
- No private PR text in tracked fixtures; use synthetic regression cases.
- Distinguish author reports, bot assessments and verified commit evidence.
- No claim of Weave inclusion, scoring improvement or model quality improvement without measured evidence.
- Unknown PR state must prevent publication. Preserve sensitive-file approval gates.
- Existing configs and historical records must deserialize with defaults.
- Do not overwrite a user's custom review guide.

## Review focus

- Changes requiring multiple edits must not become a misleading one-click patch (task A).
- Negation, partial fixes, later rejection and questions must not inflate resolution statistics (task B).
- Merge, draft or head change during model execution and lookup failures must prevent stale publication (task C).
- Dependency content is untrusted, bounded, pinned, and cannot be confused with current-PR code (task D).
- Disk corruption, restarts, missing cost, timezone boundaries and outages must not silently corrupt daily totals or stall all repositories (tasks F/G).

## Tasks

- [x] A/E: `review.rs`, bundled guide, `eval/` — add explicit suggestion applicability/dependency evidence with conservative legacy behavior; unsafe suggestions remain readable plain code. Calibrate impact separately from confidence. Add synthetic regression/holdout cases and a deterministic safety eval runnable without private repositories or model credentials. RED: dependent permission change not clickable; GREEN: safe standalone patch still clickable. Preserve major/blocker visibility.
- [x] B: `feedback.rs`, `eval/thread-stats.mjs` — classify chronological author replies with negation and partial/deferred handling; keep bot follow-up evidence separate. Add source IDs/timestamps/commit references, JSON output and question-excluded resolution summaries. RED: commit-prefixed and emoji-prefixed fixes, negation, mixed outcomes; GREEN: explicit evidence without claiming code verification.
- [x] C: `github.rs`, `poller.rs` — add fresh PR lookup and pure publication decision for open/draft/head. Recheck before model work and immediately before writes; old-head work is recorded and newest head revisited. RED: closed/draft/changed/lookup failure; GREEN: only fresh eligible head may post.
- [x] D: `context.rs` — read explicitly linked PR dependencies from the description, bounded to same-repo references plus explicitly allowed same-owner links. Include each dependency's pinned base/head, state and excerpt with unavailable/truncated labels. RED: unrelated links, duplicate refs, untrusted text, bounds; GREEN: actionable context without treating dependency code as current code.
- [x] F: new `audit.rs`, `state.rs`, `build.rs`, `eval_cli.rs`, pipeline hooks — append durable structured review events outside the 200-row UI log; store build/guide fingerprints, SHA, timing, cost availability, finding IDs, publication outcome/IDs where available. Provide date/timezone-filtered JSON/CSV daily export and historical activity import clearly marked incomplete. RED: >200 events, restart, corrupt line, timezone boundary, CSV escaping; GREEN: no double counting and missing cost remains unknown.
- [x] G: new `retry.rs`, state/poller hooks — per-repository exponential backoff with retry hints, reset on success, manual refresh behavior, grouped outage/recovery messages. Re-fetch latest head before expensive work to coalesce queued pushes; bound re-review work by risk/delta without skipping safety checks. RED: retry sequence, repository isolation, recovery, latest-head substitution; GREEN: no hot loop or global starvation.
- [x] Integration: run complete Rust suite, frontend build, deterministic safety evaluation, CLI fixture exports, and whole-diff review. Update README with operational commands, report interpretation, configuration provenance and known limitations. No deployment or external publication implied.

## Verification commands

`cargo test --manifest-path src-tauri/Cargo.toml --lib`

`npm run build`

`node --test eval/*.test.mjs` (new offline regressions)

Daily export smoke test uses a temporary synthetic audit directory, never the live application data directory.

## Progress

- Plan authorized by the user's instruction to apply all P0 and P1 items; routine implementation choices resolved in this session.
- Existing linked worktree preserved. Implementation and offline integration checks completed; no deployment or GitHub writes.
- Historical CLI import reproduced 90 normal reviews, 4 failures, 35 findings across 46 PRs, plus 5 bare current approvals. A dismissed bare review has no recoverable approval state and is intentionally excluded. Re-import added zero rows.
- Feedback validation recovered the 7 missed author reports and preserved the mixed response as partial.
- Independent review findings (prose suggestion fences, English negation/partial replies, incomplete cache identity, fallback finding loss) were reproduced and fixed.
- Whole Rust suite: 299 passed, 4 intentionally ignored live/model tests; Node: 5 passed; frontend build succeeded; policy fixtures: 15 passed, zero unsafe clickable outputs. Build fingerprint test also passed.
- Remaining measurement work: human calibration labels, paid model baseline/candidate holdout comparison, production cost/wait-time comparison, and actual Weave collection/score verification. These are not inferred from offline policy tests.
- Completed-result reuse is bounded in memory (128 PRs); the audit survives restarts, but automatic outcome reuse does not. GitHub preflight and POST remain non-atomic; post-publication observations expose the remaining race.
