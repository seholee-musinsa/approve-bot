export interface AppConfig {
  repositories: string[];
  allowed_authors: string[];
  polling_interval_seconds: number;
  auto_approve_enabled: boolean;
  approval_message: string;
  skip_drafts: boolean;
  notifications_enabled: boolean;
  /** true = write a Claude review then gate approve on the score;
   *  false = legacy blind approve only (no review). */
  review_enabled: boolean;
  /** true = deep review (clone PR head, explore code); false = diff-only. */
  review_deep: boolean;
  /** true = once a PR has an engine review, later commits are approved without
   *  re-reviewing (first review always runs). */
  approve_only_after_review: boolean;
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
  /** Long-form detail (full review body) — shown collapsed, expandable. */
  detail?: string | null;
}

export type GhLoginProgress =
  | { kind: "started" }
  | { kind: "code"; code: string }
  | { kind: "done" }
  | { kind: "failed"; message: string };
