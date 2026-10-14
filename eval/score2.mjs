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
import { DIMS, metrics } from './quality-metrics.mjs';
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

// SPLIT=dev (cases without a split) | holdout | holdout2 | ... | all (default). The held-out
// batch was collected after the guide was tuned, so it is the honest check.
const SPLIT = process.env.SPLIT || 'all';
const TRUTH_DIR = process.env.TRUTH_DIR || P('eval', 'truth');
const cases = readdirSync(TRUTH_DIR)
  .filter((f) => f.endsWith('.json'))
  .map((f) => ({ key: f.replace('.json', ''), ...JSON.parse(readFileSync(`${TRUTH_DIR}/${f}`, 'utf8')) }))
  .filter((c) => SPLIT === 'all' || (SPLIT === 'dev' ? !c.split || c.split === 'dev' : c.split === SPLIT))
  .sort((a, b) => a.key.localeCompare(b.key));

// ---- inputs -----------------------------------------------------------------
function caseDiff(c) {
  // Synthetic cases are entirely local and never trigger a GitHub lookup.
  if (typeof c.diff === 'string') return c.diff.slice(0, MAX_DIFF);
  const f = P('eval', 'cache', `${c.key}.diff`);
  if (!existsSync(f)) {
    mkdirSync(P('eval', 'cache'), { recursive: true });
    const pr = JSON.parse(execFileSync('gh', ['api', `repos/${c.repo}/pulls/${c.pr}`], { encoding: 'utf8' }));
    // Escaped cases pin the merge-time head (= the PR diff); their base branch may be gone.
    // Others diff from base.sha: the branch name moves, and main...sha is empty once merged.
    const endpoint = c.kind === 'escaped' ? `repos/${c.repo}/pulls/${c.pr}` : `repos/${c.repo}/compare/${pr.base.sha}...${c.sha}`;
    const diff = execFileSync('gh', ['api', endpoint, '-H', 'Accept: application/vnd.github.v3.diff'], {
      encoding: 'utf8',
      maxBuffer: 256 * 1024 * 1024,
    });
    writeFileSync(f, diff);
  }
  const d = readFileSync(f, 'utf8');
  return d.length > MAX_DIFF ? `${d.slice(0, MAX_DIFF)}\n[... diff truncated for the judge ...]` : d;
}

function reviewText(r) {
  const inline = (r.inline || []).map((c) => `- ${c.path}:${c.line} — ${c.comment || c.body || ''}`).join('\n');
  return `${r.body || ''}\n\n[인라인 코멘트]\n${inline || '(없음)'}\n\n[blocking_issues]\n${(r.blocking_issues || []).join('\n') || '(없음)'}\n\n[구조화 지적]\n${JSON.stringify(r.findings || [])}`;
}

// ---- judge ------------------------------------------------------------------
const INSTRUCTIONS = `너는 코드리뷰 채점관이다. PR diff, 출처가 명시된 기준 결함 목록, 채점할 리뷰(본문+인라인+blocking)를 받는다.
정답 출처는 각 사례의 provenance 를 따른다. 합성 사례는 작성된 기대 결과이며 실제 작성자의 반영 증거가 아니다. 목록에 없는 문제도 실제로 있을 수 있다.

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

4. 각 known defect 를 찾았으면 severity_result 를 aligned|understated|overstated|unverifiable 로 적는다. 영향과 도달 가능성으로 평가하며 확신도·수정 줄 수만으로 판정하지 않는다. 놓쳤으면 unverifiable 이다.
5. suggestions: 실제 적용 버튼이 있는 코드 블록마다 단독 적용 안전성을 safe|unsafe|unverifiable 로 판정한다. 다른 import·권한·소비처·마이그레이션 변경이 필요한데 빠졌으면 unsafe 다. 일반 코드 제안에는 적용 버튼이 없으므로 이 목록에 넣지 않는다. 구조화 findings 의 suggestion 필드만으로 버튼이 있다고 보지 말고, 실제 인라인의 suggestion 펜스를 확인한다.

출력은 아래 JSON 하나만. 설명 문장 금지.
{"defects":[{"id":"d1","result":"found|partial|missed","severity_result":"aligned|understated|overstated|unverifiable","why":"짧게"}],
 "claims":[{"summary":"짧게","verdict":"correct|incorrect|unverifiable","severity":"blocking|non_blocking","matches":"d1|null"}],
 "suggestions":[{"verdict":"safe|unsafe|unverifiable","why":"짧게"}],
 "quality":{"fp_discipline":n,"confidence_separation":n,"signal_density":n,"actionability":n,"tone_structure":n}}`;

function judge(c, review) {
  const truth = c.defects.length
    ? c.defects.map((d) => `- ${d.id} [${d.severity}] ${d.path}${d.line ? `:${d.line}` : ''} — ${d.summary}`).join('\n')
    : '(없음 — 알려진 결함이 없는 PR)';
  const prompt = `${INSTRUCTIONS}\n\n=== PR ===\n${c.repo}#${c.pr} @ ${c.sha}\n\n=== 정답 출처 ===\n${c.provenance || "Local truth; inspect source/evidence fields"}\n\n=== 정답 결함 ===\n${truth}\n\n=== 채점할 리뷰 ===\n${reviewText(review)}\n\n=== diff ===\n${caseDiff(c)}`;
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
  ['심각도 평가 수', 'severity_assessments'],
  ['심각도 일치율', 'severity_alignment', true],
  ['적용 제안 평가 수', 'suggestion_assessments'],
  ['위험한 적용 제안 비율', 'unsafe_suggestion_rate', true],
  ['적용 안전성 미확인 비율', 'unverifiable_suggestion_rate', true],
  ['품질 총점 (0~5)', 'quality'],
  ...DIMS.map((d) => [`  ${d}`, d]),
  ['인라인 / 리뷰', 'inline_per_review'],
  ['딥 탐색 실제 수행', 'explored_rate', true],
  ['미완료 리뷰', 'unfinished'],
  ['비용 / 리뷰 ($)', 'cost_per_review'],
];

let md = `\n## eval v2 — ${LABELS.join(' vs ')} — split=${SPLIT} — ${new Date().toISOString().slice(0, 10)}\n\n`;
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
