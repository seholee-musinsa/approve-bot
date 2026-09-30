#!/usr/bin/env node
// Eval v2 scorer. For every review produced by run2.sh it asks a judge that
// sees the DIFF, the review body, the inline comments and the known defects of
// that case (eval/truth/<key>.json) to:
//   1. mark each known defect found / partial / missed  -> recall
//   2. list every problem the review ASSERTS and label it
//      correct / incorrect / unverifiable              -> false positives
//   3. rate the review on the 5 quality axes used since Phase 1
// Clean cases (no known defects) measure false blockers.
//
// Usage: node eval/score2.mjs <label> [<label> ...]
//   Scores each label (cached per review in eval/scores2/<label>/), then prints
//   and appends a side-by-side table to eval/results.md.
import { execFileSync } from 'node:child_process';
import { appendFileSync, existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';

const LABELS = process.argv.slice(2);
if (!LABELS.length) {
  console.error('usage: node eval/score2.mjs <label> [<label> ...]');
  process.exit(1);
}
const JUDGE_MODEL = process.env.JUDGE_MODEL || 'claude-opus-5-5';
const MAX_DIFF = 80_000;
const ROOT = new URL('..', import.meta.url).pathname;
const P = (...s) => ROOT + s.join('/');
const CLAUDE = process.env.CLAUDE_PATH || `${process.env.HOME}/.local/bin/claude`;

const cases = readdirSync(P('eval', 'truth'))
  .filter((f) => f.endsWith('.json'))
  .map((f) => ({ key: f.replace('.json', ''), ...JSON.parse(readFileSync(P('eval', 'truth', f), 'utf8')) }))
  .sort((a, b) => a.key.localeCompare(b.key));

// ---- inputs -----------------------------------------------------------------
function caseDiff(c) {
  const f = P('eval', 'cache', `${c.key}.diff`);
  if (!existsSync(f)) {
    mkdirSync(P('eval', 'cache'), { recursive: true });
    const pr = JSON.parse(execFileSync('gh', ['api', `repos/${c.repo}/pulls/${c.pr}`], { encoding: 'utf8' }));
    const diff = execFileSync(
      'gh',
      ['api', `repos/${c.repo}/compare/${pr.base.ref}...${c.sha}`, '-H', 'Accept: application/vnd.github.v3.diff'],
      { encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 },
    );
    writeFileSync(f, diff);
  }
  const d = readFileSync(f, 'utf8');
  return d.length > MAX_DIFF ? `${d.slice(0, MAX_DIFF)}\n[... diff truncated for the judge ...]` : d;
}

function reviewText(r) {
  const inline = (r.inline || []).map((c) => `- ${c.path}:${c.line} — ${c.comment}`).join('\n');
  return `${r.body || ''}\n\n[인라인 코멘트]\n${inline || '(없음)'}\n\n[blocking_issues]\n${(r.blocking_issues || []).join('\n') || '(없음)'}`;
}

// ---- judge ------------------------------------------------------------------
const INSTRUCTIONS = `너는 코드리뷰 채점관이다. PR diff, 그 PR 에 대해 이미 확정된 결함 목록(정답), 채점할 리뷰(본문+인라인+blocking)를 받는다.
정답 목록은 사람 리뷰어가 지적하고 작성자가 실제로 고친 것만 모은 것이다. 목록에 없는 문제도 실제로 있을 수 있다.

해야 할 일:
1. defects: 정답 결함마다 리뷰가 찾았는지 판정한다.
   - "found": 같은 문제를 같은 위치(파일·심볼)로 짚었고 원인이 맞다.
   - "partial": 불확실한 우려·질문으로만 짚었거나, 위치는 맞지만 원인이 흐리다.
   - "missed": 언급이 없다.
2. claims: 리뷰가 "문제"라고 주장한 항목을 전부 뽑는다(칭찬·요약·확인했다는 문장은 제외, 불확실 우려와 질문은 포함).
   각 항목을 diff 로 판정한다.
   - "correct": diff 로 보아 실제 문제다(정답 목록에 없어도 된다).
   - "incorrect": diff 로 보아 틀린 주장이다(존재하지 않는 코드, 잘못 읽은 동작, 이미 처리된 경우).
   - "unverifiable": diff 만으로는 판정할 수 없다.
   각 항목에 severity 를 붙인다: 리뷰가 blocking/머지 차단으로 올렸으면 "blocking", 아니면 "non_blocking".
   정답 결함과 같은 항목이면 matches 에 그 id 를 적는다.
3. quality: 리뷰 전체를 0~5(0.5 단위)로 채점한다.
   - fp_discipline: 근거로 설명되는 지적만 있는가
   - confidence_separation: 확정과 불확실을 구분해 쓰는가
   - signal_density: 비자명한 발견의 밀도
   - actionability: 파일·라인·수정 방법이 구체적인가
   - tone_structure: 자연스러운 한국어, 읽기 쉬운 구조

출력은 아래 JSON 하나만. 설명 문장 금지.
{"defects":[{"id":"d1","result":"found|partial|missed","why":"짧게"}],
 "claims":[{"summary":"짧게","verdict":"correct|incorrect|unverifiable","severity":"blocking|non_blocking","matches":"d1|null"}],
 "quality":{"fp_discipline":n,"confidence_separation":n,"signal_density":n,"actionability":n,"tone_structure":n}}`;

function judge(c, review) {
  const truth = c.defects.length
    ? c.defects.map((d) => `- ${d.id} [${d.severity}] ${d.path}${d.line ? `:${d.line}` : ''} — ${d.summary}`).join('\n')
    : '(없음 — 알려진 결함이 없는 PR)';
  const prompt = `${INSTRUCTIONS}\n\n=== PR ===\n${c.repo}#${c.pr} @ ${c.sha}\n\n=== 정답 결함 ===\n${truth}\n\n=== 채점할 리뷰 ===\n${reviewText(review)}\n\n=== diff ===\n${caseDiff(c)}`;
  const raw = execFileSync(
    CLAUDE,
    ['-p', '--output-format', 'json', '--model', JUDGE_MODEL, '--allowedTools', '', '--setting-sources', 'user'],
    { input: prompt, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024, env: { ...process.env, NODE_OPTIONS: '' } },
  );
  const txt = (JSON.parse(raw).result || '').trim();
  const m = txt.match(/\{[\s\S]*\}/);
  if (!m) throw new Error(`judge produced no JSON: ${txt.slice(0, 200)}`);
  return JSON.parse(m[0]);
}

// ---- score one label ----------------------------------------------------------
function scoreLabel(label) {
  const dir = P('eval', 'out2', label);
  const cacheDir = P('eval', 'scores2', label);
  mkdirSync(cacheDir, { recursive: true });
  const rows = [];
  for (const c of cases) {
    const files = existsSync(dir) ? readdirSync(dir).filter((f) => f.startsWith(`${c.key}.s`) && f.endsWith('.json')) : [];
    for (const f of files.sort()) {
      const review = JSON.parse(readFileSync(`${dir}/${f}`, 'utf8'));
      const cached = `${cacheDir}/${f}`;
      let j;
      if (existsSync(cached)) {
        j = JSON.parse(readFileSync(cached, 'utf8'));
      } else {
        try {
          j = judge(c, review);
          writeFileSync(cached, JSON.stringify(j, null, 2));
          console.error(`judged ${label}/${f}`);
        } catch (e) {
          console.error(`judge FAIL ${label}/${f}: ${e.message}`);
          continue;
        }
      }
      rows.push({ c, review, j });
    }
  }
  return rows;
}

// ---- metrics ------------------------------------------------------------------
const avg = (a) => (a.length ? a.reduce((x, y) => x + y, 0) / a.length : NaN);
const DIMS = ['fp_discipline', 'confidence_separation', 'signal_density', 'actionability', 'tone_structure'];

function metrics(rows) {
  const withDefects = rows.filter((r) => r.c.defects.length);
  const recallOf = (r) =>
    avg(r.j.defects.map((d) => (d.result === 'found' ? 1 : d.result === 'partial' ? 0.5 : 0)));
  const strictRecallOf = (r) => avg(r.j.defects.map((d) => (d.result === 'found' ? 1 : 0)));
  const claims = rows.flatMap((r) => r.j.claims || []);
  const incorrect = claims.filter((c) => c.verdict === 'incorrect');
  const clean = rows.filter((r) => !r.c.defects.length);
  return {
    reviews: rows.length,
    recall: avg(withDefects.map(recallOf)),
    recall_strict: avg(withDefects.map(strictRecallOf)),
    claims_per_review: claims.length / (rows.length || 1),
    precision: claims.length ? claims.filter((c) => c.verdict === 'correct').length / claims.length : NaN,
    incorrect_per_review: incorrect.length / (rows.length || 1),
    incorrect_blocking_per_review: incorrect.filter((c) => c.severity === 'blocking').length / (rows.length || 1),
    clean_false_block_rate: clean.length
      ? clean.filter((r) => (r.review.blocking_issues || []).length > 0).length / clean.length
      : NaN,
    quality: avg(rows.flatMap((r) => DIMS.map((d) => r.j.quality?.[d]).filter((x) => typeof x === 'number'))),
    ...Object.fromEntries(DIMS.map((d) => [d, avg(rows.map((r) => r.j.quality?.[d]).filter((x) => typeof x === 'number'))])),
    inline_per_review: avg(rows.map((r) => (r.review.inline || []).length)),
    explored_rate: avg(rows.map((r) => (r.review.explored ? 1 : 0))),
    unfinished: rows.filter((r) => !r.review.finished_cleanly).length,
    cost_per_review: avg(rows.map((r) => r.review.cost_usd).filter((x) => typeof x === 'number')),
  };
}

// ---- report -------------------------------------------------------------------
const results = Object.fromEntries(LABELS.map((l) => [l, scoreLabel(l)]));
const M = Object.fromEntries(LABELS.map((l) => [l, metrics(results[l])]));
const fmt = (n, pct) => (Number.isFinite(n) ? (pct ? `${(n * 100).toFixed(0)}%` : n.toFixed(2)) : '—');

const ROWS = [
  ['리뷰 수', 'reviews'],
  ['recall (found=1, partial=0.5)', 'recall', true],
  ['recall (found 만)', 'recall_strict', true],
  ['주장 수 / 리뷰', 'claims_per_review'],
  ['precision (correct / 주장)', 'precision', true],
  ['틀린 주장 / 리뷰', 'incorrect_per_review'],
  ['틀린 blocking / 리뷰', 'incorrect_blocking_per_review'],
  ['clean PR 오차단율', 'clean_false_block_rate', true],
  ['품질 총점 (0~5)', 'quality'],
  ...DIMS.map((d) => [`  ${d}`, d]),
  ['인라인 / 리뷰', 'inline_per_review'],
  ['딥 탐색 실제 수행', 'explored_rate', true],
  ['미완료 리뷰', 'unfinished'],
  ['비용 / 리뷰 ($)', 'cost_per_review'],
];

let md = `\n## eval v2 — ${LABELS.join(' vs ')} — ${new Date().toISOString().slice(0, 10)}\n\n`;
md += `cases: ${cases.length} (defects ${cases.filter((c) => c.defects.length).length} · clean ${cases.filter((c) => !c.defects.length).length}) · judge=${JUDGE_MODEL}\n\n`;
md += `| 지표 | ${LABELS.join(' | ')} |\n|---|${LABELS.map(() => '--:').join('|')}|\n`;
for (const [name, key, pct] of ROWS) md += `| ${name} | ${LABELS.map((l) => fmt(M[l][key], pct)).join(' | ')} |\n`;

md += `\n### 결함별 결과\n\n| case | defect | ${LABELS.join(' | ')} |\n|---|---|${LABELS.map(() => '---').join('|')}|\n`;
for (const c of cases) {
  for (const d of c.defects) {
    const cells = LABELS.map((l) =>
      results[l]
        .filter((r) => r.c.key === c.key)
        .map((r) => ({ found: '●', partial: '◐', missed: '○' })[r.j.defects.find((x) => x.id === d.id)?.result] || '?')
        .join(''),
    );
    md += `| ${c.key} | ${d.id} ${d.summary.slice(0, 40).replace(/\|/g, '/')} | ${cells.join(' | ')} |\n`;
  }
}

appendFileSync(P('eval', 'results.md'), md);
writeFileSync(P('eval', 'scores2', 'latest.json'), JSON.stringify(M, null, 2));
console.log(md);
