#!/usr/bin/env node
// Score the generated reviews two ways:
//   (1) deterministic auto-metrics parsed straight from each review (no LLM),
//   (2) a blind opus judge that ranks baseline / phase1 / ceiling per PR on a
//       rubric, with review order randomized and identities hidden.
// Emits eval/scores.json (raw) and appends a table to eval/results.md.
//
// Usage: node eval/score.mjs <phaseLabel>   e.g. `node eval/score.mjs phase1`
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync, readdirSync, existsSync, appendFileSync } from 'node:fs';

const PHASE = process.argv[2] || 'phase1';
const JUDGE_MODEL = 'claude-opus-4-8';
const ROOT = new URL('..', import.meta.url).pathname;
const P = (...s) => ROOT + s.join('/');

// Which variants to compare. baseline + the phase under test, plus ceiling.
const VARIANTS = ['baseline', PHASE];

const keys = readdirSync(P('eval', 'out', 'baseline'))
  .filter((f) => f.endsWith('.json'))
  .map((f) => f.replace('.json', ''))
  .sort();

// ---- (1) deterministic auto-metrics -------------------------------------
function autoMetrics(review) {
  const body = review.body || '';
  return {
    verdict: review.verdict,
    score: review.score,
    blocking: (review.blocking_issues || []).length,
    inline: (review.inline || []).length,
    // confidence tiering signal: a dedicated body-only uncertainty section
    has_uncertainty_section: /#\s*확정 못한 우려/.test(body) ? 1 : 0,
    body_chars: body.length,
    cost_usd: review.cost_usd ?? null,
  };
}

function loadReview(variant, k) {
  const f = P('eval', 'out', variant, `${k}.json`);
  return existsSync(f) ? JSON.parse(readFileSync(f, 'utf8')) : null;
}
function loadCeiling(k) {
  const f = P('eval', 'ceiling', `${k}.txt`);
  const t = existsSync(f) ? readFileSync(f, 'utf8').trim() : '';
  return t.length > 40 ? t : null;
}

// ---- (2) blind opus judge ------------------------------------------------
const RUBRIC = `너는 코드리뷰 품질 심사관이다. 같은 PR에 대한 리뷰 3개(X/Y/Z)를 받는다. 어느 것이 누구/어떤 버전인지 모른다.
각 리뷰를 아래 5개 축에서 0~5로 채점하라(0.5 단위 허용). 반드시 근거를 리뷰에서 인용해 판단하고, 인용 못 하면 낮게 준다.

축:
- fp_discipline: 억지·추측 지적 없이 근거로 설명되는 지적만 있는가. 추측성 "다른 데 깨질 수도"·자명한 일반론이 많으면 낮음.
- confidence_separation: 확정한 지적과 불확실한 우려를 구분해 배치하는가(불확실한 것을 확정처럼 단정하지 않음).
- signal_density: 비자명하고 실질적인 발견의 밀도. 교과서 설명·분량 채우기는 감점.
- actionability: 무엇을 어떻게 고칠지 구체적인가(파일·함수·코드 제안).
- tone_structure: 자연스러운 한국어 동료체 + 읽기 쉬운 구조.

출력은 오직 아래 JSON 한 개. 설명 문장 금지:
{"X":{"fp_discipline":n,"confidence_separation":n,"signal_density":n,"actionability":n,"tone_structure":n},"Y":{...},"Z":{...},"best":"X|Y|Z","why":"한 문장"}`;

function judge(reviews) {
  // reviews: [{label, body}] already shuffled. Build X/Y/Z payload.
  const tags = ['X', 'Y', 'Z'];
  const map = {};
  let prompt = RUBRIC + '\n\n';
  reviews.forEach((r, i) => {
    map[tags[i]] = r.label;
    prompt += `=== 리뷰 ${tags[i]} ===\n${r.body}\n\n`;
  });
  const raw = execFileSync(
    process.env.CLAUDE_PATH || `${process.env.HOME}/.local/bin/claude`,
    ['-p', prompt, '--output-format', 'json', '--model', JUDGE_MODEL, '--allowedTools', '', '--setting-sources', 'user'],
    { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024, env: { ...process.env, NODE_OPTIONS: '' } },
  );
  const env = JSON.parse(raw);
  let txt = (env.result || '').trim();
  const m = txt.match(/\{[\s\S]*\}/);
  if (!m) throw new Error('judge produced no JSON: ' + txt.slice(0, 200));
  const scored = JSON.parse(m[0]);
  // remap X/Y/Z -> variant labels
  const out = { best: map[scored.best] || null, why: scored.why || '' };
  for (const t of tags) if (scored[t]) out[map[t]] = scored[t];
  return out;
}

function shuffle(a) {
  for (let i = a.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [a[i], a[j]] = [a[j], a[i]];
  }
  return a;
}

// ---- run -----------------------------------------------------------------
const rows = [];
for (const k of keys) {
  const reviews = {};
  for (const v of VARIANTS) {
    const r = loadReview(v, k);
    if (r) reviews[v] = r;
  }
  const ceiling = loadCeiling(k);
  const auto = {};
  for (const v of VARIANTS) if (reviews[v]) auto[v] = autoMetrics(reviews[v]);

  const panel = [];
  for (const v of VARIANTS) if (reviews[v]?.body) panel.push({ label: v, body: reviews[v].body });
  if (ceiling) panel.push({ label: 'ceiling', body: ceiling });

  let verdict = null;
  try {
    verdict = panel.length >= 2 ? judge(shuffle(panel.slice())) : null;
    console.error(`judged ${k}: best=${verdict?.best}`);
  } catch (e) {
    console.error(`judge FAIL ${k}: ${e.message}`);
  }
  rows.push({ pr: k, auto, judge: verdict, has_ceiling: !!ceiling });
}

writeFileSync(P('eval', 'scores.json'), JSON.stringify({ phase: PHASE, rows }, null, 2));

// ---- render results.md ---------------------------------------------------
const dims = ['fp_discipline', 'confidence_separation', 'signal_density', 'actionability', 'tone_structure'];
const avg = (arr) => (arr.length ? arr.reduce((a, b) => a + b, 0) / arr.length : NaN);
const f = (n) => (Number.isFinite(n) ? n.toFixed(2) : '—');

function dimAvg(variant, d) {
  return avg(rows.map((r) => r.judge?.[variant]?.[d]).filter((x) => typeof x === 'number'));
}
function totAvg(variant) {
  return avg(rows.flatMap((r) => (r.judge?.[variant] ? dims.map((d) => r.judge[variant][d]) : [])));
}
function autoAvg(variant, field) {
  return avg(rows.map((r) => r.auto?.[variant]?.[field]).filter((x) => typeof x === 'number'));
}

const compareTargets = [...VARIANTS, 'ceiling'];
let md = `\n## ${PHASE} — ${new Date().toISOString().slice(0, 10)}\n\n`;
md += `PR set: ${keys.length}건 · judge=${JUDGE_MODEL} · diff-only · baseline=pre-Phase1\n\n`;

md += `### Judge 루브릭 평균 (0~5)\n\n`;
md += `| 축 | baseline | ${PHASE} | ceiling | Δ(${PHASE}−base) |\n|---|--:|--:|--:|--:|\n`;
for (const d of dims) {
  const b = dimAvg('baseline', d), p = dimAvg(PHASE, d), c = dimAvg('ceiling', d);
  md += `| ${d} | ${f(b)} | ${f(p)} | ${f(c)} | ${f(p - b)} |\n`;
}
md += `| **총점** | **${f(totAvg('baseline'))}** | **${f(totAvg(PHASE))}** | **${f(totAvg('ceiling'))}** | **${f(totAvg(PHASE) - totAvg('baseline'))}** |\n\n`;

const bestCounts = Object.fromEntries(compareTargets.map((v) => [v, rows.filter((r) => r.judge?.best === v).length]));
md += `**Judge best pick**: ` + compareTargets.map((v) => `${v} ${bestCounts[v] || 0}`).join(' · ') + `\n\n`;

md += `### Auto-metrics 평균 (결정론)\n\n`;
md += `| 지표 | baseline | ${PHASE} |\n|---|--:|--:|\n`;
md += `| inline 코멘트 수 | ${f(autoAvg('baseline', 'inline'))} | ${f(autoAvg(PHASE, 'inline'))} |\n`;
md += `| blocking 수 | ${f(autoAvg('baseline', 'blocking'))} | ${f(autoAvg(PHASE, 'blocking'))} |\n`;
md += `| "확정 못한 우려" 섹션 有(0~1) | ${f(autoAvg('baseline', 'has_uncertainty_section'))} | ${f(autoAvg(PHASE, 'has_uncertainty_section'))} |\n`;
md += `| 본문 길이(자) | ${f(autoAvg('baseline', 'body_chars'))} | ${f(autoAvg(PHASE, 'body_chars'))} |\n`;
md += `| score 평균 | ${f(autoAvg('baseline', 'score'))} | ${f(autoAvg(PHASE, 'score'))} |\n\n`;

md += `### PR별 judge best\n\n| PR | best | why |\n|---|---|---|\n`;
for (const r of rows) md += `| ${r.pr} | ${r.judge?.best ?? '—'} | ${(r.judge?.why ?? '').replace(/\|/g, '/')} |\n`;

appendFileSync(P('eval', 'results.md'), md);
console.error(`\nwrote eval/scores.json + appended eval/results.md`);
console.error(md);
