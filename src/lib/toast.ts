// Minimal in-app toast bus. Decoupled from React so any module can fire a toast
// via showToast(); ToastHost subscribes and renders. (Separate from the macOS
// desktop notifications the poller sends on approve.)
export type ToastKind = "success" | "error" | "info";

export interface ToastMsg {
  id: number;
  text: string;
  kind: ToastKind;
}

const EVENT = "app://toast";
let seq = 0;

export function showToast(text: string, kind: ToastKind = "info"): void {
  const detail: ToastMsg = { id: ++seq, text, kind };
  window.dispatchEvent(new CustomEvent<ToastMsg>(EVENT, { detail }));
}

export function onToast(cb: (t: ToastMsg) => void): () => void {
  const handler = (e: Event) => cb((e as CustomEvent<ToastMsg>).detail);
  window.addEventListener(EVENT, handler);
  return () => window.removeEventListener(EVENT, handler);
}
