import { useEffect, useState } from "react";
import { api } from "../lib/tauri";
import { JiraPanels } from "./JiraPanels";
import type {
  AppConfig,
  CreateMode,
  Frequency,
  SweepRun,
  SweepSettings,
  SweepStatus,
} from "../lib/types";

interface Props {
  value: AppConfig;
  onChange: (next: AppConfig) => void;
  /** 저장하지 않은 변경이 있으면 실행 버튼을 막는다(저장된 설정으로 돌기 때문). */
  dirty: boolean;
}

const WEEKDAYS = ["월", "화", "수", "목", "금", "토", "일"];
const REPO_PATTERN = /^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/;

export function SweepTab({ value, onChange, dirty }: Props) {
  const s = value.sweep;
  function patch(p: Partial<SweepSettings>) {
    onChange({ ...value, sweep: { ...s, ...p } });
  }
  const repoOk = s.repo === "" || REPO_PATTERN.test(s.repo);

  return (
    <>
      <StatusPanel settings={s} dirty={dirty} repoOk={repoOk} />
      <div className="panel">
        <h2>정기 스윕 설정</h2>
        <label className="toggle">
          <input
            type="checkbox"
            checked={s.enabled}
            onChange={(e) => patch({ enabled: e.target.checked })}
          />
          정기 스윕 사용
        </label>
        <Field label="대상 repo">
          <input
            type="text"
            placeholder="owner/repo"
            value={s.repo}
            onChange={(e) => patch({ repo: e.target.value })}
          />
        </Field>
        {!repoOk && <div className="error-text">owner/repo 형식으로 입력해 주세요</div>}
        <Field label="주기">
          <select
            value={s.frequency}
            onChange={(e) => patch({ frequency: e.target.value as Frequency })}
          >
            <option value="daily">매일</option>
            <option value="weekly">매주</option>
            <option value="monthly">매월</option>
          </select>
          {s.frequency === "weekly" && (
            <select
              value={s.weekday}
              onChange={(e) => patch({ weekday: Number(e.target.value) })}
            >
              {WEEKDAYS.map((d, i) => (
                <option key={d} value={i}>
                  {d}요일
                </option>
              ))}
            </select>
          )}
          {s.frequency === "monthly" && (
            <span className="row">
              <NumberInput value={s.month_day} min={1} max={28} onChange={(n) => patch({ month_day: n })} />
              일
            </span>
          )}
        </Field>
        <Field label="실행 시각">
          <NumberInput value={s.hour} min={0} max={23} onChange={(n) => patch({ hour: n })} />
          시
          <NumberInput value={s.minute} min={0} max={59} onChange={(n) => patch({ minute: n })} />
          분
        </Field>
        <div className="muted">
          앱이 꺼져 있던 동안 지난 시각은 켜질 때 한 번만 보정해서 돌립니다. PR 리뷰가 도는 중에는 끝나길 기다립니다.
        </div>
        <Field label="회당 조각 수">
          <NumberInput value={s.slices_per_run} min={1} max={3} onChange={(n) => patch({ slices_per_run: n })} />
          <span className="muted">1~3</span>
        </Field>
        <Field label="조각 최대 줄 수">
          <NumberInput value={s.max_slice_lines} min={2000} max={100000} step={1000} onChange={(n) => patch({ max_slice_lines: n })} />
        </Field>
        <Field label="묶음당 최대 파일">
          <NumberInput value={s.max_files_per_ticket} min={1} max={30} onChange={(n) => patch({ max_files_per_ticket: n })} />
        </Field>
        <Field label="티켓 생성 방식">
          <select
            value={s.create_mode}
            onChange={(e) => patch({ create_mode: e.target.value as CreateMode })}
          >
            <option value="draft">초안만 (Jira 에 쓰지 않음)</option>
            <option value="auto">자동 생성</option>
          </select>
        </Field>
        {s.create_mode === "auto" && (
          <div className="error-text">
            자동 생성은 설정 폴더의 sweep-jira.json 에서 allow_create 가 켜져 있을 때만 Jira 에 씁니다. 켜져 있으면 "지금 실행"도 실제 티켓을 만듭니다.
          </div>
        )}
      </div>
      <JiraPanels />
      <RunLog />
    </>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="row" style={{ gap: 12 }}>
      <span className="field-label">{label}</span>
      <span className="row" style={{ flex: 1, gap: 8 }}>
        {children}
      </span>
    </div>
  );
}

function NumberInput(p: {
  value: number;
  min: number;
  max: number;
  step?: number;
  onChange: (n: number) => void;
}) {
  return (
    <input
      type="number"
      style={{ maxWidth: 96 }}
      value={p.value}
      min={p.min}
      max={p.max}
      step={p.step ?? 1}
      onChange={(e) => {
        const n = Number(e.target.value);
        if (Number.isFinite(n)) p.onChange(Math.min(p.max, Math.max(p.min, Math.round(n))));
      }}
    />
  );
}

function StatusPanel({
  settings,
  dirty,
  repoOk,
}: {
  settings: SweepSettings;
  dirty: boolean;
  repoOk: boolean;
}) {
  const [st, setSt] = useState<SweepStatus | null>(null);
  const [msg, setMsg] = useState<string | null>(null);
  const [confirmReset, setConfirmReset] = useState(false);

  useEffect(() => {
    let alive = true;
    const load = () =>
      api
        .getSweepStatus()
        .then((x) => alive && setSt(x))
        .catch(() => {});
    load();
    const t = setInterval(load, 5000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);

  async function runNow() {
    setMsg(null);
    try {
      const started = await api.runSweepNow();
      setMsg(started ? "실행을 시작했습니다. 결과는 아래 실행 기록에 쌓입니다." : "이미 실행 중입니다.");
      api.getSweepStatus().then(setSt).catch(() => {});
    } catch (e) {
      setMsg(String(e));
    }
  }

  async function reset() {
    if (!confirmReset) {
      setConfirmReset(true);
      return;
    }
    setConfirmReset(false);
    try {
      await api.resetSweepCycle();
      setMsg("바퀴를 처음으로 되돌렸습니다. 키 이력은 그대로 둡니다.");
      api.getSweepStatus().then(setSt).catch(() => {});
    } catch (e) {
      setMsg(String(e));
    }
  }

  const pct = st && st.slices_total > 0 ? Math.round((st.slices_done / st.slices_total) * 100) : 0;
  const canRun = !!st && !st.running && !dirty && settings.repo !== "" && repoOk;

  return (
    <div className="panel">
      <h2>진행 상황</h2>
      {!st ? (
        <div className="muted">불러오는 중…</div>
      ) : (
        <>
          <div className="row">
            <span>
              {st.cycle_no
                ? `바퀴 ${st.cycle_no} · 읽은 조각 ${st.slices_done}/${st.slices_total} (${pct}%)`
                : "아직 시작한 바퀴가 없습니다"}
            </span>
            {st.running && <span className="badge">실행 중</span>}
          </div>
          {st.cycle_no && (
            <div className="progress">
              <div className="progress-bar" style={{ width: `${pct}%` }} />
            </div>
          )}
          {st.next_slice && <div className="muted">다음 조각: {st.next_slice}</div>}
          <div className="muted">
            이월 후보 {st.carryover}건 · 만든 티켓 {st.created_keys} · 완료 {st.done_keys} · 거절 {st.rejected_keys} · 끝낸 바퀴 {st.finished_cycles}
          </div>
          <div className="muted">
            마지막 실행 {st.last_run ?? "-"} · 다음 실행 {st.next_run ?? (settings.enabled ? "-" : "꺼져 있음")}
          </div>
          <div className="row">
            <button className="primary" onClick={runNow} disabled={!canRun}>
              지금 실행
            </button>
            <button onClick={reset} disabled={!st || st.running}>
              {confirmReset ? "정말 초기화" : "바퀴 초기화"}
            </button>
            {confirmReset && <button onClick={() => setConfirmReset(false)}>취소</button>}
          </div>
          {dirty && <div className="muted">저장하지 않은 변경이 있어 저장 후 실행할 수 있습니다.</div>}
          {msg && <div className="muted">{msg}</div>}
        </>
      )}
    </div>
  );
}

function RunLog() {
  const [runs, setRuns] = useState<SweepRun[]>([]);
  useEffect(() => {
    let alive = true;
    const load = () =>
      api
        .getSweepLog(30)
        .then((x) => alive && setRuns(x))
        .catch(() => {});
    load();
    const t = setInterval(load, 5000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);

  return (
    <div className="panel">
      <h2>실행 기록</h2>
      {runs.length === 0 ? (
        <div className="muted">아직 실행 기록이 없습니다.</div>
      ) : (
        runs.map((r) => (
          <details key={r.at} className="review-detail">
            <summary>
              {r.ok ? "✅" : "⚠"} {new Date(r.at * 1000).toLocaleString("ko-KR")} · {r.seconds}초
            </summary>
            <pre className="review-body">{r.text}</pre>
          </details>
        ))
      )}
    </div>
  );
}
