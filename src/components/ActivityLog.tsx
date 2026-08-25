import { useEffect, useState } from "react";
import { api, onActivity, openExternal } from "../lib/tauri";
import type { ActivityEntry } from "../lib/types";

function formatTime(iso: string): string {
  try {
    const d = new Date(iso);
    return d.toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
    });
  } catch {
    return iso;
  }
}

export function ActivityLog() {
  const [entries, setEntries] = useState<ActivityEntry[]>([]);

  useEffect(() => {
    api.getActivityLog(100).then(setEntries).catch(() => {});
    const off = onActivity((entry) => {
      setEntries((prev) => [entry, ...prev].slice(0, 200));
    });
    return () => {
      off.then((u) => u()).catch(() => {});
    };
  }, []);

  async function clearAll() {
    try {
      await api.clearActivityLog();
      setEntries([]);
    } catch {
      // ignore — backend clear failed; leave entries as-is
    }
  }

  return (
    <div className="panel" style={{ flex: 1, minHeight: 0 }}>
      <div
        className="row"
        style={{ justifyContent: "space-between", alignItems: "center" }}
      >
        <h2 style={{ margin: 0 }}>Activity ({entries.length})</h2>
        <button
          type="button"
          onClick={clearAll}
          disabled={entries.length === 0}
        >
          Clear
        </button>
      </div>
      <div className="activity">
        {entries.length === 0 && (
          <div className="muted">No activity yet. Polling will report results here.</div>
        )}
        {entries.map((e, i) => {
          const inlineLink = e.kind === "approved" && e.url && e.repo;
          return (
            <div className={`entry ${e.kind}`} key={`${e.timestamp}-${i}`}>
              <span className="time">{formatTime(e.timestamp)}</span>
              <span>
                <b>{labelFor(e)}</b>
                {e.repo && (
                  <>
                    {" "}
                    <span className="muted">in</span>{" "}
                    {inlineLink ? (
                      <a
                        href={e.url!}
                        onClick={(ev) => {
                          ev.preventDefault();
                          openExternal(e.url!).catch(() => {});
                        }}
                      >
                        {e.repo}
                        {e.pr_number != null && <> #{e.pr_number}</>}
                      </a>
                    ) : (
                      <>
                        {e.repo}
                        {e.pr_number != null && <> #{e.pr_number}</>}
                      </>
                    )}
                  </>
                )}
                {e.author && (
                  <>
                    {" "}
                    <span className="muted">by</span> @{e.author}
                  </>
                )}
                {e.pr_title && <div className="muted">{e.pr_title}</div>}
                <div className="muted">{e.message}</div>
              </span>
              {e.url && !inlineLink && (
                <a
                  href={e.url}
                  onClick={(ev) => {
                    ev.preventDefault();
                    openExternal(e.url!).catch(() => {});
                  }}
                >
                  Open
                </a>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}

function labelFor(e: ActivityEntry): string {
  switch (e.kind) {
    case "approved":
      return "✅ Approved";
    case "skipped":
      return "⏭ Skipped";
    case "error":
      return "⚠ Error";
    case "info":
      return "ℹ Info";
  }
}
