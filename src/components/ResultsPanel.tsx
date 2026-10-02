import { useState } from "react";
import { api } from "../lib/tauri";
import type { ResultsView } from "../lib/types";

/** 자동 생성 티켓의 결과(반영률)와 상위 에픽이 없는 티켓. 읽기 전용이고, 일괄 지정만 Jira 에 쓴다. */
export function ResultsPanel() {
  const [view, setView] = useState<ResultsView | null>(null);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  const [confirm, setConfirm] = useState(false);

  async function collect() {
    setBusy(true);
    setMsg(null);
    setConfirm(false);
    try {
      setView(await api.collectSweepResults());
    } catch (e) {
      setMsg(`⚠️ ${String(e)}`);
    } finally {
      setBusy(false);
    }
  }

  async function assign() {
    if (!view) return;
    if (!confirm) {
      setConfirm(true);
      return;
    }
    setConfirm(false);
    setBusy(true);
    try {
      setMsg(await api.assignParentBulk(view.orphans.map((o) => o.key)));
      setView(await api.collectSweepResults());
    } catch (e) {
      setMsg(`⚠️ ${String(e)}`);
    } finally {
      setBusy(false);
    }
  }

  const r = view?.results;
  const judged = r ? r.done + r.rejected : 0;

  return (
    <div className="panel">
      <h2>결과와 반영률</h2>
      <div className="row">
        <button onClick={collect} disabled={busy}>
          {busy ? "읽는 중…" : "결과 수집"}
        </button>
        <span className="muted">Jira 에서 자동 생성 티켓의 처리 결과를 읽어 이력에 반영합니다(읽기 전용).</span>
      </div>
      {msg && <div className="muted">{msg}</div>}
      {r && (
        <>
          <div>
            자동 생성 티켓 {r.total}건 · 열림 {r.open} · 완료 {r.done} · 거절 {r.rejected}
          </div>
          <div className="row">
            <span>
              반영률{" "}
              <b>{r.adoption_percent === null ? "-" : `${r.adoption_percent.toFixed(0)}%`}</b>
              <span className="muted"> (완료 ÷ 결과 난 건 {judged}건)</span>
            </span>
            {r.ready_to_expand ? (
              <span className="badge">확대 기준 충족</span>
            ) : (
              <span className="muted">확대 기준: 결과 10건 이상 · 반영률 60% 이상</span>
            )}
          </div>
          {r.rejected > 0 && (
            <div className="muted">
              거절 사유: 내용이 틀림 {r.wrong} · 가치 낮음 {r.low_value} · 지금은 어려움 {r.size_timing} · 중복 {r.duplicate} · 이미 해결 {r.already_fixed} · 사유 없음 {r.no_reason}
            </div>
          )}
          {r.stale > 0 && <div className="muted">30일 넘게 처리 없이 열려 있는 티켓 {r.stale}건(미처리, 반영률에서 제외)</div>}

          <h2 style={{ marginTop: 8 }}>상위 에픽이 없는 자동 생성 티켓 ({view!.orphans.length})</h2>
          {view!.orphans.length === 0 ? (
            <div className="muted">없음</div>
          ) : (
            <>
              <ul className="list">
                {view!.orphans.map((o) => (
                  <li key={o.key}>
                    <b>{o.key}</b> {o.summary}
                  </li>
                ))}
              </ul>
              <div className="row">
                <button
                  className="primary"
                  onClick={assign}
                  disabled={busy || view!.parent_key === ""}
                >
                  {confirm ? `정말 ${view!.parent_key} 아래로 지정` : `상위 에픽 ${view!.parent_key || "(미설정)"} 로 일괄 지정`}
                </button>
                {confirm && <button onClick={() => setConfirm(false)}>취소</button>}
              </div>
            </>
          )}
        </>
      )}
    </div>
  );
}
