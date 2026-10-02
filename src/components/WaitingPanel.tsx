import { useEffect, useState } from "react";
import { api } from "../lib/tauri";
import type { WaitingCandidate } from "../lib/types";

const REFRESH_MS = 15_000;

/** 상한 때문에 아직 못 만든 티켓 후보. 골라서 지금 만들거나 버린다(새 점검 구간은 읽지 않는다). */
export function WaitingPanel({ onChanged }: { onChanged?: () => void }) {
  const [rows, setRows] = useState<WaitingCandidate[]>([]);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [confirm, setConfirm] = useState<"create" | "discard" | null>(null);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);

  async function load() {
    try {
      const r = await api.listWaitingCandidates();
      setRows(r);
      // 목록에서 사라진 항목은 선택에서도 뺀다.
      setPicked((p) => new Set([...p].filter((id) => r.some((x) => x.id === id))));
    } catch {
      // 다음 주기에 다시 시도한다.
    }
  }

  useEffect(() => {
    void load();
    const t = setInterval(() => void load(), REFRESH_MS);
    return () => clearInterval(t);
  }, []);

  function toggle(id: string) {
    setConfirm(null);
    setPicked((p) => {
      const n = new Set(p);
      if (n.has(id)) n.delete(id);
      else n.add(id);
      return n;
    });
  }

  async function run(kind: "create" | "discard") {
    if (confirm !== kind) {
      setConfirm(kind);
      return;
    }
    setConfirm(null);
    setBusy(true);
    setMsg(null);
    const ids = [...picked];
    try {
      if (kind === "create") {
        setMsg(await api.createWaitingCandidates(ids));
      } else {
        const n = await api.discardWaitingCandidates(ids);
        setMsg(`${n}건을 대기 목록에서 버렸습니다.`);
      }
      setPicked(new Set());
      await load();
      onChanged?.();
    } catch (e) {
      setMsg(`⚠️ ${String(e)}`);
    } finally {
      setBusy(false);
    }
  }

  const n = picked.size;

  return (
    <div className="panel">
      <div className="row" style={{ justifyContent: "space-between", alignItems: "center" }}>
        <h2 style={{ margin: 0 }}>대기 중인 후보 ({rows.length})</h2>
        <button
          type="button"
          onClick={() => setPicked(n === rows.length ? new Set() : new Set(rows.map((r) => r.id)))}
          disabled={rows.length === 0 || busy}
        >
          {n === rows.length && rows.length > 0 ? "선택 해제" : "모두 선택"}
        </button>
      </div>
      <div className="muted">
        상한이나 건수 제한 때문에 아직 티켓이 되지 못한 후보입니다. 다음 실행에서 자동으로 만들어지고, 여기서 골라 지금 만들 수도 있습니다.
      </div>
      {rows.length === 0 ? (
        <div className="muted">대기 중인 후보가 없습니다.</div>
      ) : (
        <ul className="list">
          {rows.map((r) => (
            <li key={r.id} style={{ alignItems: "flex-start" }}>
              <label className="toggle" style={{ alignItems: "flex-start", flex: 1 }}>
                <input type="checkbox" checked={picked.has(r.id)} onChange={() => toggle(r.id)} disabled={busy} />
                <span>
                  <b>{r.title}</b>
                  <div className="muted">
                    {r.category} · {r.effort} · 파일 {r.files.length}개
                    {r.files[0] ? ` · ${shorten(r.files[0])}` : ""}
                  </div>
                </span>
              </label>
            </li>
          ))}
        </ul>
      )}
      {rows.length > 0 && (
        <div className="row">
          <button className="primary" onClick={() => void run("create")} disabled={busy || n === 0}>
            {confirm === "create" ? `정말 ${n}건 지금 만들기` : `선택한 ${n}건 지금 만들기`}
          </button>
          <button className="danger" onClick={() => void run("discard")} disabled={busy || n === 0}>
            {confirm === "discard" ? `정말 ${n}건 버리기` : `선택한 ${n}건 버리기`}
          </button>
          {confirm && <button onClick={() => setConfirm(null)}>취소</button>}
        </div>
      )}
      {confirm === "create" && (
        <div className="muted">
          Jira 에 실제 티켓이 만들어집니다. 열린 자동 생성 티켓 상한 안에서만 만들고, 이미 같은 항목의 티켓이 있으면 건너뜁니다.
        </div>
      )}
      {confirm === "discard" && <div className="muted">버린 후보는 되돌릴 수 없습니다. 같은 항목은 다음에 다시 발견되면 새 후보가 됩니다.</div>}
      {msg && <div className="muted" style={{ whiteSpace: "pre-wrap" }}>{msg}</div>}
    </div>
  );
}

function shorten(p: string): string {
  const parts = p.split("/");
  return parts.length > 3 ? `…/${parts.slice(-3).join("/")}` : p;
}
