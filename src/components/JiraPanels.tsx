import { useEffect, useRef, useState } from "react";
import { api } from "../lib/tauri";
import type { Assignee, JiraView } from "../lib/types";

/** 티켓(Jira) 설정과 담당자. 앱 설정(config.json)과 별개 파일이라 자체 저장 버튼을 쓴다. */
export function JiraPanels() {
  const [saved, setSaved] = useState<JiraView | null>(null);
  const [v, setV] = useState<JiraView | null>(null);
  const [msg, setMsg] = useState<string | null>(null);
  const [testing, setTesting] = useState(false);

  useEffect(() => {
    api
      .getJiraSettings()
      .then((x) => {
        setSaved(x);
        setV(x);
      })
      .catch((e) => setMsg(String(e)));
  }, []);

  if (!v || !saved) return <div className="panel"><h2>티켓 설정</h2><div className="muted">{msg ?? "불러오는 중…"}</div></div>;

  const dirty = JSON.stringify(v) !== JSON.stringify(saved);

  async function save(next: JiraView = v!, onlyAssignees = false) {
    setMsg(null);
    try {
      const out = await api.updateJiraSettings(next);
      setSaved(out);
      // 담당자만 저장한 경우 다른 항목의 미저장 입력은 그대로 둔다.
      setV((cur) => (onlyAssignees && cur ? { ...cur, assignees: out.assignees } : out));
      setMsg("저장했습니다.");
    } catch (e) {
      setMsg(String(e));
    }
  }

  async function test() {
    setTesting(true);
    setMsg(null);
    try {
      setMsg(await api.testJiraConnection());
    } catch (e) {
      setMsg(`⚠️ ${String(e)}`);
    } finally {
      setTesting(false);
    }
  }

  return (
    <>
      <div className="panel">
        <h2>티켓 설정 (Jira)</h2>
        <Row label="Cloud ID">
          <input type="text" value={v.cloud_id} onChange={(e) => setV({ ...v, cloud_id: e.target.value })} />
        </Row>
        <Row label="프로젝트">
          <input type="text" value={v.project} onChange={(e) => setV({ ...v, project: e.target.value })} />
        </Row>
        <Row label="상위 에픽">
          <input
            type="text"
            placeholder="비우면 상위 에픽 없이 만들고 나중에 지정"
            value={v.parent_key}
            onChange={(e) => setV({ ...v, parent_key: e.target.value })}
          />
        </Row>
        <Row label="열린 자동 생성 티켓 상한">
          <input
            type="number"
            style={{ maxWidth: 96 }}
            min={1}
            max={100}
            value={v.open_cap}
            onChange={(e) => {
              const n = Number(e.target.value);
              if (Number.isFinite(n)) setV({ ...v, open_cap: Math.min(100, Math.max(1, Math.round(n))) });
            }}
          />
          <span className="muted">한 사람 기준입니다. 모두 이 수에 이르면 새 티켓을 만들지 않고 대기</span>
        </Row>
        <div className="muted">티켓을 실제로 만들지는 "코드 점검" 설정의 "티켓 만들기"에서 정합니다.</div>
        <div className="row">
          <button className="primary" onClick={() => save()} disabled={!dirty}>
            저장
          </button>
          <button onClick={() => setV(saved)} disabled={!dirty}>
            되돌리기
          </button>
          <button onClick={test} disabled={testing || dirty || saved.cloud_id === ""}>
            {testing ? "확인 중…" : "연결 테스트"}
          </button>
        </div>
        {msg && <div className="muted">{msg}</div>}
      </div>
      <Assignees
        value={v.assignees}
        onChange={(assignees) => {
          const next = { ...saved, assignees };
          setV({ ...v, assignees });
          // 담당자는 바꾸는 즉시 저장한다(다른 항목의 미저장 변경은 건드리지 않는다).
          void save(next, true);
        }}
        disabled={saved.cloud_id === ""}
      />
    </>
  );
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="row" style={{ gap: 12 }}>
      <span className="field-label">{label}</span>
      <span className="row" style={{ flex: 1, gap: 8 }}>
        {children}
      </span>
    </div>
  );
}

function Assignees({
  value,
  onChange,
  disabled,
}: {
  value: Assignee[];
  onChange: (next: Assignee[]) => void;
  disabled: boolean;
}) {
  const [q, setQ] = useState("");
  const [hits, setHits] = useState<Assignee[]>([]);
  const [err, setErr] = useState<string | null>(null);
  const seq = useRef(0);

  useEffect(() => {
    const query = q.trim();
    if (query.length < 2) {
      setHits([]);
      return;
    }
    const mine = ++seq.current;
    const t = setTimeout(() => {
      api
        .searchJiraUsers(query)
        .then((r) => {
          if (mine === seq.current) {
            setHits(r);
            setErr(null);
          }
        })
        .catch((e) => mine === seq.current && setErr(String(e)));
    }, 250);
    return () => clearTimeout(t);
  }, [q]);

  function add(a: Assignee) {
    if (value.some((x) => x.id === a.id)) return;
    onChange([...value, a]);
    setQ("");
    setHits([]);
  }

  return (
    <div className="panel">
      <h2>담당자 ({value.length})</h2>
      <div className="muted">
        티켓은 이 목록의 사람들에게 나눠 배정됩니다. 열린 자동 생성 티켓이 가장 적은 사람에게 먼저 가고, 같으면 무작위입니다. 상한에 닿은 사람은 건너뜁니다. 목록이 비어 있으면 담당자 없이 만들어집니다.
      </div>
      <div className="autocomplete-wrap">
        <input
          type="text"
          placeholder={disabled ? "먼저 Cloud ID 를 저장해 주세요" : "이름 또는 이메일로 검색 (2글자 이상)"}
          value={q}
          disabled={disabled}
          onChange={(e) => setQ(e.target.value)}
        />
        {hits.length > 0 && (
          <ul className="autocomplete-menu">
            {hits.map((h) => (
              <li key={h.id} onClick={() => add(h)}>
                {h.name}
              </li>
            ))}
          </ul>
        )}
      </div>
      {err && <div className="error-text">{err}</div>}
      {value.length === 0 ? (
        <div className="muted">담당자 목록이 비어 있음</div>
      ) : (
        value.map((a) => (
          <div key={a.id} className="row" style={{ justifyContent: "space-between" }}>
            <span>
              {a.name}
            </span>
            <button className="danger" onClick={() => onChange(value.filter((x) => x.id !== a.id))}>삭제</button>
          </div>
        ))
      )}
    </div>
  );
}
