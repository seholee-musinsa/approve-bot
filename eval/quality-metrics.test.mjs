import test from 'node:test';
import assert from 'node:assert/strict';
import { metrics } from './quality-metrics.mjs';

const row = (c = {}, review = {}, j = {}) => ({
  c: { defects: [], ...c },
  review: { finished_cleanly: true, ...review },
  j: { defects: [], claims: [], ...j },
});

test('clean cases count actual blocker findings and request_changes, not only blocking_issues', () => {
  const result = metrics([
    row({}, { findings: [{ severity: 'blocker' }] }),
    row({}, { verdict: 'request_changes' }),
    row({}, { verdict: 'approve' }),
  ]);
  assert.equal(result.clean_false_block_rate, 2 / 3);
});

test('judge omissions do not inflate recall by shrinking the known-defect denominator', () => {
  const result = metrics([row({ defects: [{ id: 'd1' }, { id: 'd2' }] }, {}, {
    defects: [{ id: 'd1', result: 'found' }, { id: 'invented', result: 'found' }],
  })]);
  assert.equal(result.recall_strict, 0.5);
  assert.equal(result.recall, 0.5);
});

test('unscored suggestion safety and severity remain unknown', () => {
  const result = metrics([row()]);
  assert.ok(Number.isNaN(result.unsafe_suggestion_rate));
  assert.ok(Number.isNaN(result.severity_alignment));
});

test('quality reporting counts independent suggestion and severity assessments', () => {
  const result = metrics([row({ defects: [{ id: 'd1' }] }, {}, {
    defects: [{ id: 'd1', result: 'found', severity_result: 'understated' }],
    suggestions: [{ verdict: 'unsafe' }, { verdict: 'safe' }, { verdict: 'unverifiable' }],
  })]);
  assert.equal(result.unsafe_suggestion_rate, 1 / 3);
  assert.equal(result.unverifiable_suggestion_rate, 1 / 3);
  assert.equal(result.severity_alignment, 0);
});
