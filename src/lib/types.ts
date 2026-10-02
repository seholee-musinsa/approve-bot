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
  /** true = attach inline line comments (resolvable threads). Turn off if a repo
   *  requires conversation resolution to merge. */
  inline_comments_enabled: boolean;
  /** Regular repo sweep. Always sent back whole so saving never resets it. */
  sweep: SweepSettings;
}

export type Frequency = "daily" | "weekly" | "monthly";
export type CreateMode = "draft" | "auto";

export interface SweepSettings {
  enabled: boolean;
  repo: string;
  frequency: Frequency;
  /** 0 = Monday … 6 = Sunday (weekly) */
  weekday: number;
  /** 1..28 (monthly) */
  month_day: number;
  hour: number;
  minute: number;
  slices_per_run: number;
  max_slice_lines: number;
  max_files_per_ticket: number;
  create_mode: CreateMode;
}

export interface SweepStatus {
  running: boolean;
  cycle_no: number | null;
  slices_done: number;
  slices_total: number;
  next_slice: string | null;
  carryover: number;
  created_keys: number;
  done_keys: number;
  rejected_keys: number;
  finished_cycles: number;
  last_run: string | null;
  next_run: string | null;
}

export interface SweepRun {
  /** unix seconds */
  at: number;
  ok: boolean;
  seconds: number;
  text: string;
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

export interface Assignee {
  id: string;
  name: string;
}

export interface JiraView {
  cloud_id: string;
  project: string;
  parent_key: string;
  open_cap: number;
  assignees: Assignee[];
  /** read only: the second lock on writing to Jira, changed in the file by hand */
  allow_create: boolean;
}

export interface SweepResults {
  total: number;
  open: number;
  stale: number;
  done: number;
  rejected: number;
  wrong: number;
  low_value: number;
  size_timing: number;
  duplicate: number;
  already_fixed: number;
  no_reason: number;
  adoption_percent: number | null;
  ready_to_expand: boolean;
}

export interface OrphanTicket {
  key: string;
  summary: string;
}

export interface ResultsView {
  results: SweepResults;
  orphans: OrphanTicket[];
  parent_key: string;
  allow_create: boolean;
}

export interface TicketRow {
  key: string;
  summary: string;
  status: string;
  resolution: string | null;
  assignee: string | null;
  /** unix seconds */
  created: number;
  url: string;
  has_parent: boolean;
}
