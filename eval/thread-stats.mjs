#!/usr/bin/env node
// 리뷰 스레드 반영률 집계. 리뷰어별·지적 형식별로 "작성자가 수정했다고 답한 비율"을 낸다.
// 사용: node eval/thread-stats.mjs --repo owner/name [--limit 80] [--min-len 80]
// 결과는 stdout 에만 찍는다. 사내 PR 내용이 섞이므로 파일로 저장해 커밋하지 않는다(public repo).
//
// 판정: 첫 코멘트 작성자 외의 답글이 `수정·반영·적용…` 등으로 시작하면 fixed,
// `반영하지 않…·그대로 두…·철회` 등이 있으면 rejected. 답글 없이 고친 건 못 잡으므로 반영률은 하한이다.
import { execFileSync } from 'node:child_process';

const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, a, i, all) => {
    if (a.startsWith('--')) acc.push([a.slice(2), all[i + 1]]);
    return acc;
  }, []),
);
const repo = args.repo;
if (!repo || !repo.includes('/')) {
  console.error('usage: node eval/thread-stats.mjs --repo owner/name [--limit 80] [--min-len 80]');
  process.exit(1);
}
const [owner, name] = repo.split('/');
const limit = Number(args.limit ?? 80);
const minLen = Number(args['min-len'] ?? 80);

const FIXED = /^\s*(\*\*)?(수정|반영|적용|고쳤|지웠|걷어|추가했|바꿨|Fixed|Done)/;
const REJECTED = /반영하지 않|반영은 하지|그대로 두|유지합니다|의도한|의도된|철회/;
const SEVERITY = /🔴|🟡|P1|P2|blocker|major/;

const gh = (a) => execFileSync('gh', a, { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });

const prs = JSON.parse(
  gh(['pr', 'list', '--repo', repo, '--state', 'merged', '--limit', String(limit), '--json', 'number']),
).map((p) => p.number);

const QUERY = `query($o:String!,$n:String!,$pr:Int!){repository(owner:$o,name:$n){pullRequest(number:$pr){
  reviewThreads(first:100){nodes{comments(first:8){nodes{author{login} body}}}}}}}`;

const threads = [];
for (const pr of prs) {
  try {
    const out = JSON.parse(
      gh(['api', 'graphql', '-f', `query=${QUERY}`, '-f', `o=${owner}`, '-f', `n=${name}`, '-F', `pr=${pr}`]),
    );
    for (const t of out.data.repository.pullRequest.reviewThreads.nodes) {
      const [first, ...replies] = t.comments.nodes;
      if (!first || (first.body ?? '').length < minLen) continue;
      const user = first.author?.login ?? 'ghost';
      threads.push({
        user,
        code: first.body.includes('```'),
        sev: SEVERITY.test(first.body),
        fixed: replies.some((r) => r.author?.login !== user && FIXED.test(r.body ?? '')),
        rejected: replies.some((r) => REJECTED.test(r.body ?? '')),
      });
    }
  } catch (e) {
    console.error(`PR ${pr} 건너뜀: ${String(e.message).split('\n')[0]}`);
  }
}

const group = (key) => {
  const m = new Map();
  for (const t of threads) {
    const k = key(t);
    const g = m.get(k) ?? { n: 0, fixed: 0, rejected: 0 };
    g.n += 1;
    g.fixed += t.fixed ? 1 : 0;
    g.rejected += t.rejected ? 1 : 0;
    m.set(k, g);
  }
  return [...m.entries()]
    .sort((a, b) => b[1].n - a[1].n)
    .map(([k, g]) => ({
      group: k,
      threads: g.n,
      fixed: g.fixed,
      'fixed%': Math.floor((g.fixed * 100) / g.n),
      'rejected%': Math.floor((g.rejected * 100) / g.n),
    }));
};

console.log(`\n${repo} 최근 머지 ${prs.length} PR, 스레드 ${threads.length}개 (본문 ${minLen}자 이상)`);
console.log('\n리뷰어별');
console.table(group((t) => t.user));
console.log('수정 코드 블록');
console.table(group((t) => (t.code ? '있음' : '없음')));
console.log('심각도 표시');
console.table(group((t) => (t.sev ? '있음' : '없음')));
