export interface AppConfig {
  repositories: string[];
  allowed_authors: string[];
  polling_interval_seconds: number;
  auto_approve_enabled: boolean;
  approval_message: string;
  skip_drafts: boolean;
  notifications_enabled: boolean;
}

export interface ConnectionStatus {
  connected: boolean;
  username: string | null;
  rate_limit_remaining: number | null;
  rate_limit_total: number | null;
  last_error: string | null;
  checked_at: string;
}

export type ActivityKind = "approved" | "skipped" | "error" | "info";

export interface GhUserHint {
  login: string;
  avatar_url: string | null;
}

export interface ActivityEntry {
  timestamp: string;
  kind: ActivityKind;
  repo: string | null;
  pr_number: number | null;
  pr_title: string | null;
  author: string | null;
  url: string | null;
  message: string;
}

export type GhLoginProgress =
  | { kind: "started" }
  | { kind: "code"; code: string }
  | { kind: "done" }
  | { kind: "failed"; message: string };
