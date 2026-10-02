import { useEffect, useState } from "react";
import { api, openExternal } from "../lib/tauri";
import type { TicketRow } from "../lib/types";

const REFRESH_MS = 60_000;

/** 봇이 만든 티켓을 최신순으로 쌓아 보여 주고, 누르면 Jira 로 이동한다(읽기 전용). */
export function TicketLog() {
  const [rows, setRows] = useState<TicketRow[]>([]);
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [loaded, setLoaded] = useState(false);

  async function load() {
    setLoading(true);
    try {
      setRows(await api.listBotTickets());
      setErr(null);
    } catch (e) {
      setErr(String(e));
    } finally {
      setLoading(false);
      setLoaded(true);
    }
  }

  useEffect(() => {
    void load();
    const t = setInterval(() => void load(), REFRESH_MS);
    return () => clearInterval(t);
  }, []);

  return (
    <div className="panel" style={{ flex: 1, minHeight: 0 }}>
      <div className="row" style={{ justifyContent: "space-between", alignItems: "center" }}>
        <h2 style={{ margin: 0 }}>만든 티켓 ({rows.length})</h2>
        <button type="button" onClick={() => void load()} disabled={loading}>
          {loading ? "읽는 중…" : "새로고침"}
        </button>
      </div>
      {err && <div className="error-text">{err}</div>}
      <div className="activity">
        {loaded && !err && rows.length === 0 && (
          <div className="muted">아직 만든 티켓이 없습니다. 티켓을 만들면 여기에 쌓입니다.</div>
        )}
        {rows.map((r) => {
          const kind = stateOf(r);
          return (
            <div className={`entry ${kind.cls}`} key={r.key}>
              <span className="time">{ago(r.created)}</span>
              <span>
                <b>{kind.label}</b>{" "}
                <a
                  href={r.url || "#"}
                  title={r.url ? "Jira 에서 열기" : "Jira 주소를 아직 알 수 없습니다"}
                  onClick={(ev) => {
                    ev.preventDefault();
                    if (r.url) openExternal(r.url).catch(() => {});
                  }}
                >
                  {r.key}
                </a>{" "}
                {r.summary}
                <div className="muted">
                  {r.assignee ?? "담당자 없음"} · {r.status}
                  {r.resolution ? ` (${r.resolution})` : ""}
                  {!r.has_parent && " · ⚠ 부모 없음"}
                </div>
              </span>
              {r.url && (
                <a
                  href={r.url}
                  onClick={(ev) => {
                    ev.preventDefault();
                    openExternal(r.url).catch(() => {});
                  }}
                >
                  Jira
                </a>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}

function stateOf(r: TicketRow): { label: string; cls: string } {
  switch (r.resolution) {
    case "Done":
      return { label: "✅ 완료", cls: "approved" };
    case null:
    case undefined:
      return { label: "🆕 열림", cls: "info" };
    default:
      return { label: "⏭ 닫힘", cls: "skipped" };
  }
}

function ago(unix: number): string {
  if (!unix) return "-";
  const s = Math.max(0, Math.floor(Date.now() / 1000 - unix));
  if (s < 3600) return `${Math.max(1, Math.floor(s / 60))}분 전`;
  if (s < 86400) return `${Math.floor(s / 3600)}시간 전`;
  return `${Math.floor(s / 86400)}일 전`;
}
