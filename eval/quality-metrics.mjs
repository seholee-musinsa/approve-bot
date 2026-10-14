const avg = (a) => (a.length ? a.reduce((x, y) => x + y, 0) / a.length : NaN);
export const DIMS = ['fp_discipline', 'confidence_separation', 'signal_density', 'actionability', 'tone_structure'];

export function metrics(rows) {
  const withDefects = rows.filter((r) => r.c.defects.length);
  const recallOf = (r) =>
    avg(r.c.defects.map((truth) => { const d = r.j.defects.find(d => d.id === truth.id); return d?.result === 'found' ? 1 : d?.result === 'partial' ? 0.5 : 0; }));
  const strictRecallOf = (r) => avg(r.c.defects.map((truth) => r.j.defects.find(d => d.id === truth.id)?.result === 'found' ? 1 : 0));
  const claims = rows.flatMap((r) => r.j.claims || []);
  const incorrect = claims.filter((c) => c.verdict === 'incorrect');
  const clean = rows.filter((r) => !r.c.defects.length);
  const suggestions = rows.flatMap(r => r.j.suggestions || []);
  const severity = rows.flatMap(r => r.c.defects.map(truth => r.j.defects.find(d => d.id === truth.id)))
    .filter(d => ['aligned', 'understated', 'overstated'].includes(d?.severity_result));
  return {
    suggestion_assessments: suggestions.length,
    unsafe_suggestion_rate: suggestions.length ? suggestions.filter(s => s.verdict === 'unsafe').length / suggestions.length : NaN,
    unverifiable_suggestion_rate: suggestions.length ? suggestions.filter(s => s.verdict === 'unverifiable').length / suggestions.length : NaN,
    severity_assessments: severity.length,
    severity_alignment: severity.length ? severity.filter(d => d.severity_result === 'aligned').length / severity.length : NaN,
    reviews: rows.length,
    recall: avg(withDefects.map(recallOf)),
    recall_strict: avg(withDefects.map(strictRecallOf)),
    claims_per_review: claims.length / (rows.length || 1),
    precision: claims.length ? claims.filter((c) => c.verdict === 'correct').length / claims.length : NaN,
    incorrect_per_review: incorrect.length / (rows.length || 1),
    incorrect_blocking_per_review: incorrect.filter((c) => c.severity === 'blocking').length / (rows.length || 1),
    clean_false_block_rate: clean.length
      ? clean.filter((r) => (r.review.blocking_issues || []).length > 0 || r.review.verdict === 'request_changes' || (r.review.findings || []).some(f => f.severity === 'blocker') || (r.j.claims || []).some(c => c.severity === 'blocking')).length / clean.length
      : NaN,
    quality: avg(rows.flatMap((r) => DIMS.map((d) => r.j.quality?.[d]).filter((x) => typeof x === 'number'))),
    ...Object.fromEntries(DIMS.map((d) => [d, avg(rows.map((r) => r.j.quality?.[d]).filter((x) => typeof x === 'number'))])),
    inline_per_review: avg(rows.map((r) => (r.review.inline || []).length)),
    explored_rate: avg(rows.map((r) => (r.review.explored ? 1 : 0))),
    unfinished: rows.filter((r) => !r.review.finished_cleanly).length,
    cost_per_review: avg(rows.map((r) => r.review.cost_usd).filter((x) => typeof x === 'number')),
  };
}
