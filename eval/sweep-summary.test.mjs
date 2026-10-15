import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import ts from 'typescript';
const source = readFileSync(new URL('../src/lib/sweepSummary.ts', import.meta.url), 'utf8');
const js = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext } }).outputText;
const { summarizeSweepRun } = await import(`data:text/javascript;base64,${Buffer.from(js).toString('base64')}`);

test('model failures do not look like successful zero-ticket scans', () => {
  for (const reason of ['', 'claude 실행 실패: 20분 안에 끝나지 않아 중단', '모델 출력에서 JSON 을 찾지 못함']) {
    const text = `읽기에 실패했다: ${reason}\n실패를 장부에 기록했다(저장)`;
    const summary = summarizeSweepRun(text, true); // Legacy incorrectly persisted ok=true.
    assert.match(summary, /점검 실패/);
    assert.doesNotMatch(summary, /티켓 0건 생성/);
  }
});
test('real empty findings and partially successful creation stay distinct', () => {
  assert.match(summarizeSweepRun('발견 0건 → 티켓 후보 0건', true), /티켓 0건 생성/);
  assert.match(summarizeSweepRun('생성: TEST-1 sample\n⚠️ 생성 실패: service unavailable', false), /티켓 1건 생성.*실패/);
  assert.match(summarizeSweepRun('connection failed', false), /점검 실패/);
});
test('Jira lookup failure retains queued and previously created work', () => {
  const failed = '⚠️ Jira 를 읽지 못해 오늘은 만들지 않고 모두 대기시킨다: offline\n대기: sample';
  assert.match(summarizeSweepRun(failed, false), /Jira 조회 실패 · 대기 1건/);
  assert.match(summarizeSweepRun(`생성: TEST-2 previous slice\n${failed}`, false), /티켓 1건 생성 · Jira 조회 실패 · 대기 1건/);
});
