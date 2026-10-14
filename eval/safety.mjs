#!/usr/bin/env node
// Exercise the production Rust policy, not a JavaScript copy of it.
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
const root = fileURLToPath(new URL('..', import.meta.url));
const run = spawnSync('cargo', ['test', '--manifest-path', 'src-tauri/Cargo.toml', '--lib',
  'review::tests::synthetic_safety_corpus', '--', '--exact', '--nocapture'], { cwd: root, encoding: 'utf8' });
if (run.error || run.status !== 0) {
  process.stderr.write(run.error?.message || `${run.stdout || ''}${run.stderr || ''}`);
  process.exit(1);
}
const result = run.stdout.match(/^SAFETY_EVAL (.+)$/m);
if (!result) throw new Error('Rust policy evaluator did not return a report');
console.log(JSON.stringify(JSON.parse(result[1]), null, 2));
