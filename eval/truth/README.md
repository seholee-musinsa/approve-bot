# Truth cases (local only)

Each `<repo>_<pr>.json` pins one PR at the commit a human reviewer saw, before
the fixes, and lists candidate reference defects with their evidence. Distinguish
`author_reported`, `bot_assessed`, and `commit_verified`; an author reply or a
commit link alone does not independently verify the code fix. The files quote private-repo findings, so they
stay out of git — only this README is tracked.

```json
{
  "repo": "owner/name",
  "pr": 123,
  "sha": "<commit reviewed, before fixes>",
  "kind": "defects | clean",
  "defects": [
    {
      "id": "d1",
      "path": "src/file.ts",
      "line": 42,
      "severity": "blocker | major | minor",
      "summary": "what breaks, for which input",
      "keywords": ["symbol"],
      "evidence": "fix commit / reply url",
      "source": "reviewer + comment url"
    }
  ],
  "rejected_findings": [
    { "path": "...", "summary": "...", "why_rejected": "...", "source": "url" }
  ]
}
```

`clean` cases have no known defects; a blocking finding there counts as a
false block. `rejected_findings` are claims the author refuted with evidence —
useful traps for false positives.

Run: `eval/run2.sh <label>` then `node eval/score2.mjs <label> [<label>...]`.

Portable synthetic draft cases live in `eval/synthetic/truth/` and contain no
private evidence. See [the synthetic evaluation guide](../synthetic/README.md)
for the offline policy suite and optional model/judge comparison. Set
`TRUTH_DIR=eval/synthetic/truth` to score those cases without GitHub access.
