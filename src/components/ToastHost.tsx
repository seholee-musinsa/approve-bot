import { useEffect, useState } from "react";
import { onToast, type ToastKind, type ToastMsg } from "../lib/toast";

const AUTO_DISMISS_MS = 3000;

function accent(kind: ToastKind): string {
  switch (kind) {
    case "success":
      return "var(--accent)";
    case "error":
      return "var(--danger)";
    case "info":
      return "var(--info)";
  }
}

function icon(kind: ToastKind): string {
  switch (kind) {
    case "success":
      return "✅";
    case "error":
      return "⚠";
    case "info":
      return "ℹ";
  }
}

export function ToastHost() {
  const [toasts, setToasts] = useState<ToastMsg[]>([]);

  useEffect(() => {
    return onToast((t) => {
      setToasts((prev) => [...prev, t]);
      setTimeout(
        () => setToasts((prev) => prev.filter((x) => x.id !== t.id)),
        AUTO_DISMISS_MS,
      );
    });
  }, []);

  if (toasts.length === 0) return null;

  return (
    <div
      style={{
        position: "fixed",
        right: 16,
        bottom: 16,
        display: "flex",
        flexDirection: "column",
        gap: 8,
        zIndex: 1000,
        pointerEvents: "none",
      }}
    >
      {toasts.map((t) => (
        <div
          key={t.id}
          onClick={() =>
            setToasts((prev) => prev.filter((x) => x.id !== t.id))
          }
          style={{
            pointerEvents: "auto",
            cursor: "pointer",
            display: "flex",
            alignItems: "center",
            gap: 8,
            padding: "10px 14px",
            minWidth: 200,
            maxWidth: 360,
            background: "var(--panel)",
            color: "var(--text)",
            border: "1px solid var(--border)",
            borderLeft: `3px solid ${accent(t.kind)}`,
            borderRadius: 8,
            boxShadow: "0 4px 16px rgba(0, 0, 0, 0.25)",
          }}
        >
          <span>{icon(t.kind)}</span>
          <span>{t.text}</span>
        </div>
      ))}
    </div>
  );
}
