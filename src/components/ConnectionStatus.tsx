import { useEffect, useRef, useState } from "react";
import { api, onGhLoginProgress, onPassState, onStatusChanged } from "../lib/tauri";
import { showToast } from "../lib/toast";
import type { CheckNowAck, ConnectionStatus as Status } from "../lib/types";

export function ConnectionStatus() {
  const [status, setStatus] = useState<Status | null>(null);
  const [busy, setBusy] = useState(false);
  const [checking, setChecking] = useState(false);
  const [signingIn, setSigningIn] = useState(false);
  const [deviceCode, setDeviceCode] = useState<string | null>(null);
  const [loginError, setLoginError] = useState<string | null>(null);
  const [codeCopied, setCodeCopied] = useState(false);

  // "Check now" is fire-and-forget on the backend. The pass itself reports its
  // start and end via PASS_EVENT, which is what drives the spinner — a pass
  // that runs the review engine takes minutes, far longer than any timeout we
  // could guess at. Refs stay stable across the listener closures.
  const checkingRef = useRef(false);
  const passRunningRef = useRef(false);
  // Only covers the gap before the pass starts (e.g. no token, so no pass runs
  // at all). Cancelled the moment PASS_EVENT says a pass is under way.
  const startTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  function clearStartTimer() {
    if (startTimer.current) {
      clearTimeout(startTimer.current);
      startTimer.current = null;
    }
  }

  function stopChecking(reason?: "done" | "no-pass") {
    const wasChecking = checkingRef.current;
    checkingRef.current = false;
    setChecking(false);
    clearStartTimer();
    if (!wasChecking) return;
    if (reason === "done") showToast("Check complete", "success");
    else if (reason === "no-pass") showToast("아직 검사가 시작되지 않았어요", "info");
  }

  useEffect(() => {
    api.getConnectionStatus().then(setStatus).catch(() => {});
    const offStatus = onStatusChanged((s) => {
      setStatus(s);
      // A pass emits its own status snapshot on the way out, so let PASS_EVENT
      // close the spinner. This branch covers the no-pass case (connect only).
      if (checkingRef.current && !passRunningRef.current) stopChecking("done");
    });
    const offPass = onPassState((p) => {
      passRunningRef.current = p.running;
      if (p.running) {
        // The pass is really under way — no need for the start fallback.
        clearStartTimer();
        if (checkingRef.current) setChecking(true);
      } else if (checkingRef.current) {
        stopChecking("done");
      }
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
      offPass.then((u) => u()).catch(() => {});
      offLogin.then((u) => u()).catch(() => {});
      clearStartTimer();
    };
  }, []);

  async function checkNow() {
    checkingRef.current = true;
    setChecking(true);
    let ack: CheckNowAck;
    try {
      ack = await api.forceCheckNow();
    } catch {
      stopChecking();
      showToast("Check failed", "error");
      return;
    }
    if (ack === "queued") {
      // A pass (usually a review) is already running. The press is not lost —
      // one more pass follows this one.
      showToast("검사 중이에요 — 끝나면 한 번 더 돌아요", "info");
      clearStartTimer();
      return;
    }
    // Fallback for the gap before the pass starts: without a token the poller
    // only reconnects and no pass runs, so nothing would clear the spinner.
    clearStartTimer();
    startTimer.current = setTimeout(() => stopChecking("no-pass"), 10000);
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
