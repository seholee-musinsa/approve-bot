#!/usr/bin/env node
// Read-only collection; classification belongs exclusively to Rust feedback.rs.
// node eval/thread-stats.mjs --repo owner/name [--limit 80] [--min-len 80] [--format json]
// Offline: --input fixture.json. APPROVE_BOT_BIN can select an already built binary.
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const args = {};
for (let i = 2; i < process.argv.length; i += 2) {
  const flag = process.argv[i];
  if (!['--repo', '--limit', '--min-len', '--format', '--input'].includes(flag) || process.argv[i + 1] == null) {
    throw new Error(`Unknown flag or missing value: ${flag}`);
  }
  args[flag.slice(2)] = process.argv[i + 1];
}
const repo = args.repo ?? '';
if ((!repo || !/^[^/]+\/[^/]+$/.test(repo)) && !args.input) {
  throw new Error('usage: node eval/thread-stats.mjs --repo owner/name [--limit 80] [--min-len 80] [--format json] [--input file]');
}
const limit = Number(args.limit ?? 80);
const minLen = Number(args['min-len'] ?? 80);
if (!Number.isInteger(limit) || limit <= 0 || !Number.isInteger(minLen) || minLen < 0) throw new Error('Invalid limit or min-len');
if (!['text', 'json'].includes(args.format ?? 'text')) throw new Error('--format must be text or json');
const gh = (a) => JSON.parse(execFileSync('gh', a, { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 }));
const errors = [];
let input;
let requestedPulls;
if (args.input) {
  input = JSON.parse(readFileSync(args.input, 'utf8'));
  requestedPulls = input.pulls.length;
} else {
  const prs = gh(['pr', 'list', '--repo', repo, '--state', 'merged', '--limit', String(limit), '--json', 'number,author']);
  input = { pulls: [] };
  requestedPulls = prs.length;
  for (const pr of prs) {
    try {
      // REST pagination fetches all replies; truncated GraphQL threads could
      // otherwise hide later rejections or fixes.
      const comments = gh(['api', '--paginate', '--slurp', `repos/${repo}/pulls/${pr.number}/comments?per_page=100`]).flat();
      input.pulls.push({ number: pr.number, author: pr.author.login, comments });
    } catch (e) {
      const message = `PR ${pr.number} skipped: ${String(e.message).split('\n')[0]}`;
      errors.push(message);
      console.error(message);
    }
  }
}
for (const pr of input.pulls) {
  pr.comments = pr.comments.filter(c => c.in_reply_to_id != null || (c.body ?? '').length >= minLen);
}
const flags = ['feedback', '--input', '-', '--all-reviewers', '--format', 'json'];
if (repo) flags.push('--repo', repo);
const binary = process.env.APPROVE_BOT_BIN;
const command = binary ?? 'cargo';
const commandArgs = binary ? flags : ['run', '--quiet', '--manifest-path', fileURLToPath(new URL('../src-tauri/Cargo.toml', import.meta.url)), '--', ...flags];
const report = JSON.parse(execFileSync(command, commandArgs, { input: JSON.stringify(input), encoding: 'utf8', maxBuffer: 64 * 1024 * 1024, stdio: ['pipe', 'pipe', 'inherit'] }));
report.requested_pulls = requestedPulls;
report.incomplete ||= errors.length > 0;
report.errors.push(...errors);
const group = key => {
  const groups = new Map();
  for (const t of report.findings.filter(t => t.severity !== 'question')) {
    const k = key(t);
    const g = groups.get(k) ?? { group: k, threads: 0, fixed: 0, partial: 0, deferred: 0, rejected: 0 };
    g.threads += 1;
    if (Object.hasOwn(g, t.outcome)) g[t.outcome] += 1;
    groups.set(k, g);
  }
  return [...groups.values()].sort((a, b) => b.threads - a.threads).map(g => ({ ...g, 'fixed%': Math.floor(g.fixed * 100 / g.threads), 'rejected%': Math.floor(g.rejected * 100 / g.threads) }));
};
report.groups = {
  reviewers: group(t => t.reviewer),
  code: group(t => t.has_code ? '있음' : '없음'),
  severity: group(t => t.severity),
};
if (args.format === 'json') console.log(JSON.stringify(report, null, 2));
else {
  console.log(`\n${repo} PR ${report.pulls}건, 스레드 ${report.findings.length}개 (본문 ${minLen}자 이상)`);
  console.log(`작성자 보고 기준; question ${report.summary.questions.n}건은 resolution 분모에서 제외. 코드 검증 아님.`);
  for (const [name, groups] of Object.entries(report.groups)) { console.log(name); console.table(groups); }
  if (report.incomplete) console.log('일부 PR 수집 실패: 집계 불완전');
}
