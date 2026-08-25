import { useEffect, useRef, useState } from "react";
import { api, onGhLoginProgress, onStatusChanged } from "../lib/tauri";
import { showToast } from "../lib/toast";
import type { ConnectionStatus as Status } from "../lib/types";

export function ConnectionStatus() {
  const [status, setStatus] = useState<Status | null>(null);
  const [busy, setBusy] = useState(false);
  const [checking, setChecking] = useState(false);
  const [signingIn, setSigningIn] = useState(false);
  const [deviceCode, setDeviceCode] = useState<string | null>(null);
  const [loginError, setLoginError] = useState<string | null>(null);
  const [codeCopied, setCodeCopied] = useState(false);

  // "Check now" is fire-and-forget on the backend; the poll runs async and reports
  // completion via a STATUS_EVENT. Track the pending check with a ref (stable across
  // the status listener closure) plus a timeout fallback in case no event arrives.
  const checkingRef = useRef(false);
  const checkTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  function stopChecking(reason?: "done" | "timeout") {
    const wasChecking = checkingRef.current;
    checkingRef.current = false;
    setChecking(false);
    if (checkTimer.current) {
      clearTimeout(checkTimer.current);
      checkTimer.current = null;
    }
    if (!wasChecking) return;
    if (reason === "done") showToast("Check complete", "success");
    else if (reason === "timeout") showToast("Check finished — no response", "info");
  }

  useEffect(() => {
    api.getConnectionStatus().then(setStatus).catch(() => {});
    const offStatus = onStatusChanged((s) => {
      setStatus(s);
      if (checkingRef.current) stopChecking("done");
    });
    const offLogin = onGhLoginProgress((p) => {
      switch (p.kind) {
        case "started":
          setSigningIn(true);
          setDeviceCode(null);
          setLoginError(null);
          setCodeCopied(false);
          break;
        case "code":
          setDeviceCode(p.code);
          break;
        case "done":
          setSigningIn(false);
          setDeviceCode(null);
          break;
        case "failed":
          setSigningIn(false);
          setDeviceCode(null);
          setLoginError(p.message);
          break;
      }
    });
    return () => {
      offStatus.then((u) => u()).catch(() => {});
      offLogin.then((u) => u()).catch(() => {});
      if (checkTimer.current) clearTimeout(checkTimer.current);
    };
  }, []);

  async function checkNow() {
    checkingRef.current = true;
    setChecking(true);
    try {
      await api.forceCheckNow();
    } catch {
      stopChecking();
      showToast("Check failed", "error");
      return;
    }
    // Fallback: clear the spinner even if no STATUS_EVENT arrives (e.g. no repos configured).
    if (checkTimer.current) clearTimeout(checkTimer.current);
    checkTimer.current = setTimeout(() => stopChecking("timeout"), 10000);
  }

  async function reconnect() {
    setBusy(true);
    setLoginError(null);
    try {
      const s = await api.reconnect();
      setStatus(s);
      const err = (s.last_error ?? "").toLowerCase();
      const needsLogin =
        !s.connected &&
        (err.includes("gh cli") || err.includes("gh auth") || err.includes("not authenticated"));
      if (s.connected) {
        showToast(`Reconnected as @${s.username}`, "success");
      } else if (needsLogin) {
        showToast("Sign-in required", "info");
        await api.startGhLogin();
      } else {
        showToast("Reconnect failed", "error");
      }
    } finally {
      setBusy(false);
    }
  }

  async function signIn() {
    setLoginError(null);
    await api.startGhLogin();
  }

  async function copyCode() {
    if (!deviceCode) return;
    try {
      await navigator.clipboard.writeText(deviceCode);
      setCodeCopied(true);
      setTimeout(() => setCodeCopied(false), 1500);
    } catch {
      // clipboard may be unavailable; ignore
    }
  }

  const ok = status?.connected ?? false;
  const rate =
    status?.rate_limit_remaining != null && status?.rate_limit_total != null
      ? `${status.rate_limit_remaining}/${status.rate_limit_total}`
      : null;

  return (
    <div className="connection" style={{ display: "flex", flexDirection: "column", gap: 6 }}>
      <div className="row" style={{ gap: 12 }}>
        <span>
          <span className={`status-dot ${ok ? "ok" : ""}`} />
          {ok ? (
            <>
              Connected as <b>@{status?.username}</b>
              {rate && <span className="muted"> · rate {rate}</span>}
            </>
          ) : (
            <>
              Disconnected
              {status?.last_error && (
                <span className="error-text"> — {status.last_error}</span>
              )}
            </>
          )}
        </span>
        <button onClick={reconnect} disabled={busy || signingIn}>
          {busy ? "Reconnecting…" : "Reconnect"}
        </button>
        {!ok && !signingIn && (
          <button onClick={signIn} disabled={busy}>
            Sign in with GitHub
          </button>
        )}
        <button onClick={checkNow} disabled={checking || busy}>
          {checking ? "Checking…" : "Check now"}
        </button>
      </div>

      {signingIn && (
        <div
          className="row"
          style={{
            gap: 8,
            padding: "8px 10px",
            border: "1px solid var(--info)",
            borderRadius: 6,
            background: "rgba(59, 130, 246, 0.08)",
          }}
        >
          {deviceCode ? (
            <>
              <span>Enter this code in the browser:</span>
              <code
                style={{
                  fontSize: 16,
                  fontWeight: 700,
                  letterSpacing: "0.1em",
                  padding: "2px 8px",
                  background: "var(--bg)",
                  border: "1px solid var(--border)",
                  borderRadius: 4,
                }}
              >
                {deviceCode}
              </code>
              <button onClick={copyCode}>{codeCopied ? "Copied" : "Copy"}</button>
              <span className="muted">waiting for browser authorization…</span>
            </>
          ) : (
            <span className="muted">Starting GitHub sign-in…</span>
          )}
        </div>
      )}

      {loginError && (
        <div className="error-text">Sign-in failed: {loginError}</div>
      )}
    </div>
  );
}
