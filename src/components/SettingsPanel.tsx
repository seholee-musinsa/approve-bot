import type { AppConfig } from "../lib/types";

interface Props {
  value: AppConfig;
  onChange: (next: AppConfig) => void;
}

export function SettingsPanel({ value, onChange }: Props) {
  function patch(p: Partial<AppConfig>) {
    onChange({ ...value, ...p });
  }

  return (
    <div className="panel">
      <h2>Settings</h2>
      <label className="toggle">
        <input
          type="checkbox"
          checked={value.auto_approve_enabled}
          onChange={(e) => patch({ auto_approve_enabled: e.target.checked })}
        />
        Auto-approve enabled
      </label>
      <label className="toggle">
        <input
          type="checkbox"
          checked={value.skip_drafts}
          onChange={(e) => patch({ skip_drafts: e.target.checked })}
        />
        Skip draft PRs
      </label>
      <label className="toggle">
        <input
          type="checkbox"
          checked={value.notifications_enabled}
          onChange={(e) => patch({ notifications_enabled: e.target.checked })}
        />
        Desktop notifications on approve
      </label>
      <div className="row">
        <span style={{ minWidth: 120 }}>Polling interval</span>
        <input
          type="number"
          min={30}
          max={3600}
          step={10}
          value={value.polling_interval_seconds}
          onChange={(e) =>
            patch({
              polling_interval_seconds: Math.max(
                30,
                Math.min(3600, parseInt(e.target.value || "60", 10) || 60),
              ),
            })
          }
          style={{ maxWidth: 100 }}
        />
        <span className="muted">seconds (30–3600)</span>
      </div>
      <div>
        <div className="muted" style={{ marginBottom: 4 }}>
          Approval message (optional)
        </div>
        <textarea
          rows={2}
          placeholder="e.g. LGTM (auto-approved by approve-bot)"
          value={value.approval_message}
          onChange={(e) => patch({ approval_message: e.target.value })}
          style={{ width: "100%", resize: "vertical" }}
        />
      </div>
    </div>
  );
}
