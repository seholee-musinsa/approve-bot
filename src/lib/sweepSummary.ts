/** 성공한 빈 점검과 실행 실패를 구별하고, 실제 발급된 티켓만 센다. */
export function summarizeSweepRun(text: string, ok = true): string {
  const lines = text.split("\n");
  const created = lines.filter((l) => l.startsWith("생성: ")).length;
  const waiting = lines.filter((l) => l.startsWith("대기: ")).length;
  const readFailure = lines.find((l) => l.startsWith("읽기에 실패했다:"));
  const createFailure = lines.some((l) => l.startsWith("⚠️ 생성"));
  const jiraFailure = lines.some((l) => l.startsWith("⚠️ Jira 를 읽지 못해"));
  if (readFailure !== undefined) {
    const reason = readFailure.slice("읽기에 실패했다:".length).trim();
    const detail = reason.includes("JSON") ? "모델 응답 형식 오류"
      : reason.includes("분 안에 끝나지") ? "모델 실행 시간 초과"
      : reason.includes("범위 밖 도구") ? "점검 범위 밖 도구 호출"
      : "상세 기록 확인";
    return `${created ? `티켓 ${created}건 생성 · ` : ""}코드점검 실패 · ${detail}`;
  }
  if (jiraFailure) return `${created ? `티켓 ${created}건 생성 · ` : ""}Jira 조회 실패 · 대기 ${waiting}건`;
  if (!ok && !created && !createFailure) return "코드점검 실패 · 상세 기록 확인";
  const parts = [`티켓 ${created}건 생성`, `대기 ${waiting}건`];
  if (createFailure) parts.push("⚠ 생성 실패");
  else if (!ok) parts.push("⚠ 일부 점검 실패");
  return parts.join(" · ");
}
