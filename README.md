# Approve Bot

로컬 데스크탑 앱 (Tauri + React) 으로, 지정한 GitHub repository에서 허용된 작성자가 올린 PR을
자동으로 approve 한다.

## 요구사항

- macOS
- Node.js 18+
- Rust 1.70+
- [GitHub CLI (`gh`)](https://cli.github.com/) — `gh auth login` 으로 로그인되어 있어야 함

## 실행

처음 clone 받은 뒤 production 모드로:

```bash
pnpm install
pnpm start
```

- 첫 실행은 Rust release 빌드라 5–10분 정도 걸릴 수 있음. 이후는 캐시되어 빠름.
- 빌드 결과물: `src-tauri/target/release/bundle/macos/Approve Bot.app`

개발 중 핫리로드가 필요할 때:

```bash
pnpm tauri dev
```

## 동작 개요

- 앱 시작 시 `gh auth token` 을 호출해 GitHub 토큰을 메모리에 로드 (디스크 저장 안 함).
- 설정한 polling interval(기본 60초) 마다 등록된 repository 의 오픈 PR 을 확인.
- 다음 조건을 모두 만족하는 PR 만 approve:
  - `auto_approve_enabled` 가 켜져 있고
  - 작성자가 allowed authors 목록에 있고
  - 작성자가 본인이 아니고
  - 게시 직전에도 open 상태이며 draft 가 아니고 검토한 SHA와 같고
  - 본인이 아직 APPROVED 리뷰를 안 남겼고
- approve 결과는 활동 로그 + macOS 알림으로 표시.

## 설정 위치

`~/Library/Application Support/com.approvebot.app/config.json`

## 빌드

```bash
npm run tauri build
```

## 리뷰 신뢰성과 일일 보고서

리뷰 엔진은 단독 적용 가능 여부, 연계 변경 여부, 검증 근거가 명시된 제안만 GitHub의
클릭 가능한 suggestion으로 렌더링한다. 안전 조건을 확인하지 못한 제안은 일반 코드와
설명으로 남는다. 이 검사는 모델이 적은 검증 근거의 진실성까지 보증하지는 않는다.
심각도는 영향 범위로, confidence는 근거 확실성으로 구분한다.

PR 본문의 명시적 의존 PR을 같은 저장소 또는 같은 소유자의 저장소에서 최대 4개 읽는다.
각 참조의 base/head, 상태, 제한된 본문을 별도 비신뢰 자료로 제공하고, 읽기 실패와 생략을
표시한다. 과거 시점 평가에서는 현재 의존 PR을 읽지 않아 미래 정보가 섞이지 않게 한다.

모델 실행 전후와 GitHub 게시 직전에 PR 상태를 다시 확인한다. 닫힘·병합·draft·head 변경이나
조회 실패 시 게시를 보류한다. 실행 전후에는 base와 본문 변경도 확인한다. GitHub의 확인과
게시가 원자적이지 않으므로 게시 뒤 상태도 감사 기록에 남긴다. 네트워크 오류로 응답을 받지
못한 POST는 실패로 단정하지 않고 `publication_unknown`으로 기록하며, 다음 조회에서 실행
마커로 실제 게시 여부를 대조한다.

모델이 끝나지 않은 리뷰(사용 한도, 로그인 만료, 시간 초과, 출력 파싱 실패)는 GitHub에 게시하지
않는다. 감사 기록에 `model_failed`와 오류 문구로 남기고 활동 로그에 한 줄 알린다. 사용 한도는 오류
문구의 reset 시각까지 엔진 전체를 멈추고(연결 상태에 표시, 수동 새로고침으로 풀리지 않음), 로그인
만료는 15분, 그 외는 같은 커밋에서 3회까지 15분 간격으로 다시 시도한 뒤 다음 push까지 멈춘다.
예전 빌드가 올린 실패 안내 리뷰는 같은 커밋에 정상 리뷰가 게시되면 숨긴다.

저장소·요청별 실패는 30초부터 최대 15분까지 지수 지연하고, GitHub의 Retry-After/reset이
더 길면 그 시간을 따른다. 수동 새로고침은 로컬 지연만 초기화한다. 대기 중 최신 head를
다시 조회하며, 게시가 실패한 완료 결과는 현재 앱 세션에서 최대 128개 PR에 대해 재사용한다.
head, base, diff, 가이드, 문맥 또는 실행 설정이 달라지면 재사용하지 않는다. 이 캐시는 재시작
후에는 복구되지 않지만 결과와 비용은 감사 기록에 남는다. 재리뷰 모델·thinking·second pass는
델타 위험도를 따르되 전체 PR의 권한·서버·민감 경로와 미해결 major/blocker가 있으면 높은
위험도를 유지한다. 전체 diff와 승인 안전 검사는 계속 적용한다.

감사 기록은 설정 폴더의 `review-audit.jsonl`에 추가 저장하며 UI의 최근 200건 제한과 별개다.
review/finding ID, SHA, 실행·게시 상태, 모델, 빌드 입력 fingerprint, 가이드 경로·hash,
비용·실행 시간의 기록 여부를 포함한다. 부분적으로만 알려진 다중 패스 비용은 총액으로
표시하지 않는다. 리뷰 가이드는 `review_guide_path`가 비어 있으면 번들 가이드를 쓴다. 예전 빌드가
설정 폴더에 남긴 `review-guide.md`는 첫 실행 때 `review-guide.md.stale-날짜`로 옮기고, 직접 고친
파일이면 `review_guide_path`로 올린다. 번들 가이드 첫 줄의 버전 마커가 없는 가이드는 감사 기록의
`guide_source`에 `(stale)`로 표시된다. `guide_hash`로 실제 사용본을 확인할 수 있다.

```bash
# 먼저 npm ci && npm run build
cargo run --manifest-path src-tauri/Cargo.toml -- review-report \
  --date 2026-10-14 --timezone +09:00 --format json
cargo run --manifest-path src-tauri/Cargo.toml -- review-report \
  --date 2026-10-14 --timezone +09:00 --format csv > reviews.csv
```

`--dir /path/to/audit-dir`로 감사 저장소를 지정할 수 있다. `--import-activity activity.json`은
이전 UI 기록을 불완전한 자료로 가져온다. `--import-github snapshot.json --me LOGIN`은
로컬 GitHub REST 스냅샷을 가져온다. 형식은 `{repo, number, reviews: [...], comments: [...]}`
객체 또는 그 배열이다. 가져오기만으로 GitHub에 접속하지 않는다. 날짜는 게시 시각 또는 마지막
실행 상태 시각을 지정 시간대로 변환하여 선택한다. 동일 실행의 상태 변경, 같은 review ID의
중복 가져오기는 합치며, 빈 답글 리뷰 객체는 제외한다. 손상된 줄은 개수와 함께 보고한다.
이전 UI 기록에는 review ID가 없어 GitHub 기록과 자동으로 대조할 수 없으므로, 동일 날짜의
정확한 집계에는 GitHub 스냅샷을 별도 디렉터리로 가져오는 편이 적합하다.

기간 지표는 같은 감사 기록에서 주·모델별로 집계한다. 게시 수, 지적 0건 승인 비율, 확신 80 이상
major를 동반한 승인, 실패 안내 게시 수와 모델 실패 종류, 지적·question 수, `# 검증 근거` 중복
본문, 옛 제목(`# 총평`, `# 이전 지적 처리`) 본문, 본문·게시 본문 길이 중앙값을 보여 준다. 게시 본문
길이는 이 기능이 들어간 뒤의 기록에만 있다. Weave나 GitHub를 조회하지 않는다.

```bash
cargo run --manifest-path src-tauri/Cargo.toml -- review-metrics \
  --from 2026-10-15 --to 2026-10-27 --timezone +09:00 --format md
```

작성자가 push 없이 답글만 남긴 스레드도 정리한다. PR의 `updated_at`이 바뀐 경우에만 스레드를 읽고,
내 마지막 답글 뒤에 온 작성자 답글 하나에 한 번만 답한다. question은 어떤 답이든 닫고, nit·minor는
반영 보고·반대 이유·보류면 닫는다. major·blocker는 반영 보고가 와도 push 뒤 재리뷰가 코드로 확인할
때까지 열어 두고, 반대·보류 이유에는 메모 한 번만 남긴다. 정리 결과는 감사 기록 `threads_settled`와
활동 로그에 남는다. 답글 기반 정리는 코드 검증이 아니며 `feedback` 집계에서 봇 판단으로 구분된다.

지적이 하나도 없는 승인은 판정 줄과 요약 표만 보이고, 검증 근거·미확인·산입하지 않은 것·재점검은
`검토 기록 n건` 하나로 접힌다. 미확인에 작성자에게 묻는 문장이 남아 있으면 그 섹션만 펼친 채 둔다.
프롬프트는 작성자가 답할 수 있는 확인 요청을 `# 미확인`이나 요약의 `checks`가 아니라 question 지적으로
내도록 요구하고, `review-metrics`의 `본문 밖 질문` 열이 남은 건수를 센다.

답글 통계는 작성자 보고, 참여자 보고, 봇의 후속 판단, 검증된 커밋 증거를 분리한다.
이 수집기는 커밋을 직접 검증하지 않으므로 commit_verified는 비어 있다. 부분 반영·보류·반대는
반영 완료와 분리하고, 질문은 별도 분모로 집계한다.

```bash
cargo run --manifest-path src-tauri/Cargo.toml -- feedback --repo owner/repo --format json
# 오프라인: {pulls:[{number, author, comments:[REST comment objects]}]}
cargo run --manifest-path src-tauri/Cargo.toml -- feedback \
  --input threads.json --me reviewer --repo owner/repo --format json
```

오프라인 회귀 검증:

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib
npm run build
node --test eval/*.test.mjs
node eval/safety.mjs
```

모델 평가 절차와 합성 holdout은 [eval/synthetic/README.md](eval/synthetic/README.md)에 있다.
정책 테스트 통과는 실제 탐지율·오탐률·비용 개선을 의미하지 않는다. 모델 샘플 비교와 사람의
영향 라벨 확인은 별도 평가가 필요하다. 이 기능은 Weave 점수나 수집 여부를 조회하지 않는다.
PR에는 문제·동작 변경·검증 근거를 연결하고, 실제 Weave 수집 저장소와 작성자 매핑은 별도로
확인해야 한다.
