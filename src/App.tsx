import { useEffect, useMemo, useState } from "react";
import { ConnectionStatus } from "./components/ConnectionStatus";
import { RepositoriesPanel } from "./components/RepositoriesPanel";
import { AuthorsPanel } from "./components/AuthorsPanel";
import { SettingsPanel } from "./components/SettingsPanel";
import { ReportsTab, ReportViewer } from "./components/ReportsTab";
import { WaitingPanel } from "./components/WaitingPanel";
import { TicketLog } from "./components/TicketLog";
import { SweepTab } from "./components/SweepTab";
import { ActivityLog } from "./components/ActivityLog";
import { ToastHost } from "./components/ToastHost";
import { api } from "./lib/tauri";
import type { AppConfig } from "./lib/types";

const DEFAULT_CFG: AppConfig = {
  repositories: [],
  allowed_authors: [],
  polling_interval_seconds: 60,
  auto_approve_enabled: true,
  approval_message: "",
  skip_drafts: true,
  notifications_enabled: true,
  review_enabled: true,
  review_deep: true,
  approve_only_after_review: true,
  inline_comments_enabled: true,
  report: { weekly_enabled: false, monthly_enabled: false, hour: 9, space_key: "", parent_page_id: "", publish_enabled: false },
  sweep: {
    enabled: false,
    repo: "",
    frequency: "daily",
    weekday: 0,
    month_day: 1,
    hour: 2,
    minute: 0,
    slices_per_run: 1,
    max_slice_lines: 25000,
    max_files_per_ticket: 10,
    create_mode: "draft",
    max_create_per_run: 3,
  },
};

function eq(a: AppConfig, b: AppConfig): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

export default function App() {
  const [openReport, setOpenReport] = useState<string | null>(null);
  const [tab, setTab] = useState<"review" | "sweep" | "report">("review");
  const [saved, setSaved] = useState<AppConfig>(DEFAULT_CFG);
  const [draft, setDraft] = useState<AppConfig>(DEFAULT_CFG);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    api
      .getConfig()
      .then((c) => {
        setSaved(c);
        setDraft(c);
      })
      .catch((e) => setErr(String(e)));
  }, []);

  const dirty = useMemo(() => !eq(saved, draft), [saved, draft]);

  async function save() {
    setBusy(true);
    setErr(null);
    try {
      const next = await api.updateConfig(draft);
      setSaved(next);
      setDraft(next);
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  }

  function reset() {
    setDraft(saved);
    setErr(null);
  }

  const saveBar = dirty ? (
    <div className="dirty-bar">
      <span>You have unsaved changes.</span>
      <span className="row">
        <button onClick={reset} disabled={busy}>
          Discard
        </button>
        <button className="primary" onClick={save} disabled={busy}>
          {busy ? "Saving…" : "Save"}
        </button>
      </span>
    </div>
  ) : null;

  return (
    <div className="app">
      <div className="header">
        <ConnectionStatus />
        <div className="muted">approve-bot</div>
      </div>
      <div className="tabs">
        <button className={tab === "review" ? "tab active" : "tab"} onClick={() => setTab("review")}>
          PR 리뷰
        </button>
        <button className={tab === "sweep" ? "tab active" : "tab"} onClick={() => setTab("sweep")}>
          코드 점검
        </button>
        <button className={tab === "report" ? "tab active" : "tab"} onClick={() => setTab("report")}>
          리포트
        </button>
      </div>
      {tab === "report" ? (
        <div className="body">
          <div className="col">
            <ReportsTab open={openReport} onOpen={setOpenReport} value={draft} onChange={setDraft} dirty={dirty} />
            {saveBar}
            {err && <div className="error-text">{err}</div>}
          </div>
          <div className="col">
            <ReportViewer title={openReport} />
          </div>
        </div>
      ) : tab === "sweep" ? (
        <div className="body">
          <div className="col">
            <SweepTab value={draft} onChange={setDraft} dirty={dirty} />
            {saveBar}
            {err && <div className="error-text">{err}</div>}
          </div>
          <div className="col">
            <WaitingPanel />
            <TicketLog />
          </div>
        </div>
      ) : (
      <div className="body">
        <div className="col">
          <RepositoriesPanel
            value={draft.repositories}
            onChange={(repositories) => setDraft({ ...draft, repositories })}
          />
          <AuthorsPanel
            value={draft.allowed_authors}
            onChange={(allowed_authors) =>
              setDraft({ ...draft, allowed_authors })
            }
          />
          <SettingsPanel value={draft} onChange={setDraft} />
          {saveBar}
          {err && <div className="error-text">{err}</div>}
        </div>
        <div className="col">
          <ActivityLog />
        </div>
      </div>
      )}
      <ToastHost />
    </div>
  );
}
