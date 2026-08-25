import { useEffect, useMemo, useState } from "react";
import { ConnectionStatus } from "./components/ConnectionStatus";
import { RepositoriesPanel } from "./components/RepositoriesPanel";
import { AuthorsPanel } from "./components/AuthorsPanel";
import { SettingsPanel } from "./components/SettingsPanel";
import { ActivityLog } from "./components/ActivityLog";
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
};

function eq(a: AppConfig, b: AppConfig): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

export default function App() {
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

  return (
    <div className="app">
      <div className="header">
        <ConnectionStatus />
        <div className="muted">approve-bot</div>
      </div>
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
          {dirty && (
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
          )}
          {err && <div className="error-text">{err}</div>}
        </div>
        <div className="col">
          <ActivityLog />
        </div>
      </div>
    </div>
  );
}
