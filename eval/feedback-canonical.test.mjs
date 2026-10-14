import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

test('thread stats uses canonical evidence and excludes questions from fix denominator offline', () => {
  const dir = mkdtempSync(join(tmpdir(), 'feedback-test-'));
  try {
    const comment = (id, user, body, parent) => ({ id, user: { login: user }, path: 'a.ts', body, in_reply_to_id: parent });
    const input = { pulls: [{ number: 1, author: 'author', comments: [
      comment(1, 'reviewer', '**minor** — empty collection\n<!-- f:aaaa1111 -->'),
      comment(2, 'author', '✅ fixed in abc1234', 1),
      comment(3, 'reviewer', '**question** — deployment?\n<!-- f:bbbb2222 -->'),
      comment(4, 'author', '반영하지 않겠습니다', 3),
      comment(5, 'reviewer', '<!-- fu:resolved -->', 1),
    ] }] };
    const path = join(dir, 'input.json');
    writeFileSync(path, JSON.stringify(input));
    const result = JSON.parse(execFileSync(process.execPath, [fileURLToPath(new URL('./thread-stats.mjs', import.meta.url)), '--input', path, '--repo', 'example/project', '--min-len', '0', '--format', 'json'], { encoding: 'utf8', maxBuffer: 8 * 1024 * 1024 }));
    assert.equal(result.summary.resolution.n, 1);
    assert.equal(result.summary.resolution.fixed, 1);
    assert.equal(result.summary.questions.rejected, 1);
    assert.equal(result.findings[0].author_reported[0].comment_id, 2);
    assert.equal(result.findings[0].bot_assessed[0].outcome, 'fixed');
    assert.deepEqual(result.findings[0].commit_verified, []);
    assert.equal(result.groups.reviewers[0].threads, 1);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});
