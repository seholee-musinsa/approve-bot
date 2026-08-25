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
          checked={value.review_enabled}
          onChange={(e) => patch({ review_enabled: e.target.checked })}
        />
        리뷰 작성 후 승인 (끄면 승인만)
      </label>
      {value.review_enabled ? (
        <div style={{ marginLeft: 24 }}>
          <label className="toggle">
            <input
              type="checkbox"
              checked={value.review_deep}
              onChange={(e) => patch({ review_deep: e.target.checked })}
            />
            깊은 리뷰
          </label>
          <div className="muted" style={{ marginLeft: 26 }}>
            PR 코드를 clone 해 직접 탐색 (끄면 diff만, 빠름·저렴)
          </div>
        </div>
      ) : (
        <div className="muted" style={{ marginLeft: 24 }}>
          리뷰 없이 허용 author 의 PR 을 바로 승인합니다.
        </div>
      )}
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
          Approval message (optional) — 리뷰 꺼짐일 때만 사용
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
