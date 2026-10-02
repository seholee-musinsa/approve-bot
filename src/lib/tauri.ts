import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open as openShell } from "@tauri-apps/plugin-shell";
import type {
  ActivityEntry,
  AppConfig,
  ConnectionStatus,
  GhLoginProgress,
  Assignee,
  GhUserHint,
  JiraView,
  ReportMeta,
  ResultsView,
  SweepRun,
  SweepStatus,
  TicketRow,
} from "./types";

export const ACTIVITY_EVENT = "approve-bot://activity";
export const STATUS_EVENT = "approve-bot://status-changed";
export const GH_LOGIN_EVENT = "approve-bot://gh-login";

export const api = {
  getConnectionStatus: () => invoke<ConnectionStatus>("get_connection_status"),
  reconnect: () => invoke<ConnectionStatus>("reconnect"),
  getConfig: () => invoke<AppConfig>("get_config"),
  updateConfig: (config: AppConfig) =>
    invoke<AppConfig>("update_config", { config }),
  getActivityLog: (limit = 100) =>
    invoke<ActivityEntry[]>("get_activity_log", { limit }),
  clearActivityLog: () => invoke<void>("clear_activity_log"),
  forceCheckNow: () => invoke<void>("force_check_now"),
  searchUsers: (query: string) =>
    invoke<GhUserHint[]>("search_users", { query }),
  startGhLogin: () => invoke<void>("start_gh_login"),
  getSweepStatus: () => invoke<SweepStatus>("get_sweep_status"),
  getSweepLog: (limit = 30) => invoke<SweepRun[]>("get_sweep_log", { limit }),
  runSweepNow: () => invoke<boolean>("run_sweep_now"),
  resetSweepCycle: () => invoke<void>("reset_sweep_cycle"),
  getJiraSettings: () => invoke<JiraView>("get_jira_settings"),
  updateJiraSettings: (view: JiraView) =>
    invoke<JiraView>("update_jira_settings", { view }),
  searchJiraUsers: (query: string) =>
    invoke<Assignee[]>("search_jira_users", { query }),
  testJiraConnection: () => invoke<string>("test_jira_connection"),
  collectSweepResults: () => invoke<ResultsView>("collect_sweep_results"),
  listBotTickets: () => invoke<TicketRow[]>("list_bot_tickets"),
  listReports: () => invoke<ReportMeta[]>("list_reports"),
  readReport: (title: string) => invoke<string>("read_report", { title }),
  generateReport: (kind: "weekly" | "monthly") =>
    invoke<string>("generate_report", { kind }),
  publishReport: (title: string) => invoke<string>("publish_report", { title }),
  checkReportParent: () => invoke<string>("check_report_parent"),
  assignParentBulk: (keys: string[]) =>
    invoke<string>("assign_parent_bulk", { keys }),
};

export function onActivity(
  cb: (entry: ActivityEntry) => void,
): Promise<UnlistenFn> {
  return listen<ActivityEntry>(ACTIVITY_EVENT, (e) => cb(e.payload));
}

export function onStatusChanged(
  cb: (status: ConnectionStatus) => void,
): Promise<UnlistenFn> {
  return listen<ConnectionStatus>(STATUS_EVENT, (e) => cb(e.payload));
}

export function onGhLoginProgress(
  cb: (progress: GhLoginProgress) => void,
): Promise<UnlistenFn> {
  return listen<GhLoginProgress>(GH_LOGIN_EVENT, (e) => cb(e.payload));
}

export function openExternal(url: string): Promise<void> {
  return openShell(url);
}
