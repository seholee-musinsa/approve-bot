import { useEffect, useState } from "react";
import { api, openExternal } from "../lib/tauri";
import type { AppConfig, ReportMeta, ReportSettings } from "../lib/types";

interface Props {
  open: string | null;
  onOpen: (title: string) => void;
  value: AppConfig;
  onChange: (next: AppConfig) => void;
  dirty: boolean;
}

/** 주간·월간 리포트. 지금은 설정 폴더에 마크다운으로 저장하고 여기서 본다(Confluence 게시는 다음 단계). */
export function ReportsTab({ open, onOpen, value, onChange, dirty }: Props) {
  const r = value.report;
  function patch(p: Partial<ReportSettings>) {
    onChange({ ...value, report: { ...r, ...p } });
  }
  const [reports, setReports] = useState<ReportMeta[]>([]);
  const [busy, setBusy] = useState<"weekly" | "monthly" | null>(null);
  const [msg, setMsg] = useState<string | null>(null);
  const [confirmPub, setConfirmPub] = useState<string | null>(null);
  const [pubBusy, setPubBusy] = useState(false);

  async function load() {
    try {
      setReports(await api.listReports());
    } catch (e) {
      setMsg(String(e));
    }
  }
  useEffect(() => {
    void load();
  }, []);

  async function checkParent() {
    setMsg(null);
    try {
      setMsg(await api.checkReportParent());
    } catch (e) {
      setMsg(`⚠️ ${String(e)}`);
    }
  }

  async function publish(title: string) {
    if (confirmPub !== title) {
      setConfirmPub(title);
      return;
    }
    setConfirmPub(null);
    setPubBusy(true);
    setMsg(null);
    try {
      const url = await api.publishReport(title);
      setMsg(`게시했습니다: ${url || title}`);
      await load();
    } catch (e) {
      setMsg(`⚠️ 게시 실패(리포트는 로컬에 남아 있습니다): ${String(e)}`);
    } finally {
      setPubBusy(false);
    }
  }

  async function generate(kind: "weekly" | "monthly") {
    setBusy(kind);
    setMsg(null);
    try {
      const title = await api.generateReport(kind);
      setMsg(`만들었습니다: ${title}`);
      await load();
      onOpen(title);
    } catch (e) {
      setMsg(`⚠️ ${String(e)}`);
    } finally {
      setBusy(null);
    }
  }

  return (
    <>
      <div className="panel">
        <h2>리포트 설정</h2>
        <label className="toggle">
          <input type="checkbox" checked={r.weekly_enabled} onChange={(e) => patch({ weekly_enabled: e.target.checked })} />
          주간 리포트 (매주 월요일, 지난주 월~일)
        </label>
        <label className="toggle">
          <input type="checkbox" checked={r.monthly_enabled} onChange={(e) => patch({ monthly_enabled: e.target.checked })} />
          월간 리포트 (매월 1일, 지난달 전체)
        </label>
        <div className="row" style={{ gap: 12 }}>
          <span className="field-label">생성 시각</span>
          <input
            type="number"
            style={{ maxWidth: 96 }}
            min={0}
            max={23}
            value={r.hour}
            onChange={(e) => {
              const n = Number(e.target.value);
              if (Number.isFinite(n)) patch({ hour: Math.min(23, Math.max(0, Math.round(n))) });
            }}
          />
          시
        </div>
        <div className="muted">앱이 꺼져 있던 동안 지난 시각은 켜질 때 한 번만 만듭니다. 처음 켠 시점의 과거 기간은 소급해서 만들지 않습니다.</div>
        <div className="row" style={{ gap: 12 }}>
          <span className="field-label">Confluence space</span>
          <input type="text" placeholder="예: FCPGNP" value={r.space_key} onChange={(e) => patch({ space_key: e.target.value })} />
        </div>
        <div className="row" style={{ gap: 12 }}>
          <span className="field-label">부모 페이지 ID</span>
          <input type="text" placeholder="리포트를 모아 둘 페이지의 ID" value={r.parent_page_id} onChange={(e) => patch({ parent_page_id: e.target.value })} />
        </div>
        <label className="toggle">
          <input type="checkbox" checked={r.publish_enabled} onChange={(e) => patch({ publish_enabled: e.target.checked })} />
          만든 리포트를 Confluence 에 자동 게시
        </label>
        <div className="muted">
          자동 게시를 켜면 새 리포트와, 게시에 실패해 남아 있는 리포트를 부모 페이지 아래에 올립니다. 같은 제목의 페이지가 있으면 새로 만들지 않고 내용을 갱신합니다. 끄면 아래 목록에서 직접 게시합니다.
        </div>
        <div className="row">
          <button onClick={checkParent} disabled={dirty || r.space_key === "" || r.parent_page_id === ""}>
            부모 페이지 확인(읽기)
          </button>
        </div>
        <div className="row">
          <button className="primary" onClick={() => generate("weekly")} disabled={busy !== null || dirty}>
            {busy === "weekly" ? "만드는 중…" : "지난주 리포트 지금 생성"}
          </button>
          <button className="primary" onClick={() => generate("monthly")} disabled={busy !== null || dirty}>
            {busy === "monthly" ? "만드는 중…" : "지난달 리포트 지금 생성"}
          </button>
        </div>
        {dirty && <div className="muted">저장하지 않은 변경이 있어 저장 후 만들 수 있습니다.</div>}
        {msg && <div className="muted">{msg}</div>}
      </div>
      <div className="panel">
        <h2>만든 리포트 ({reports.length})</h2>
        {reports.length === 0 ? (
          <div className="muted">아직 만든 리포트가 없습니다.</div>
        ) : (
          <ul className="list">
            {reports.map((x) => (
              <li key={x.title} style={{ cursor: "pointer" }} onClick={() => onOpen(x.title)}>
                <span>{open === x.title ? "▼ " : "▶ "}{x.title}</span>
                <span className="row" onClick={(e) => e.stopPropagation()}>
                  {x.published_url === null ? (
                    <>
                      <span className="muted">미게시</span>
                      <button
                        onClick={() => void publish(x.title)}
                        disabled={pubBusy || dirty || r.space_key === "" || r.parent_page_id === ""}
                      >
                        {confirmPub === x.title ? "정말 게시" : "Confluence 에 게시"}
                      </button>
                      {confirmPub === x.title && <button onClick={() => setConfirmPub(null)}>취소</button>}
                    </>
                  ) : x.published_url ? (
                    <a
                      href={x.published_url}
                      onClick={(ev) => {
                        ev.preventDefault();
                        openExternal(x.published_url!).catch(() => {});
                      }}
                    >
                      게시됨 ↗
                    </a>
                  ) : (
                    <span className="muted">게시됨</span>
                  )}
                  <span className="muted">{new Date(x.generated_at * 1000).toLocaleString("ko-KR")}</span>
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>
    </>
  );
}

/** 선택한 리포트의 본문(마크다운 원문). */
export function ReportViewer({ title }: { title: string | null }) {
  const [body, setBody] = useState("");
  useEffect(() => {
    if (!title) return;
    let alive = true;
    setBody("불러오는 중…");
    api
      .readReport(title)
      .then((b) => alive && setBody(b))
      .catch((e) => alive && setBody(String(e)));
    return () => {
      alive = false;
    };
  }, [title]);
  return (
    <div className="panel" style={{ flex: 1, minHeight: 0 }}>
      <h2>{title ?? "리포트"}</h2>
      {title ? <pre className="review-body" style={{ overflow: "auto" }}>{body}</pre> : <div className="muted">왼쪽 목록에서 리포트를 고르면 여기에 본문이 보입니다.</div>}
    </div>
  );
}
