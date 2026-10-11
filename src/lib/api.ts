import { invoke as tauriInvoke } from "@tauri-apps/api/core";

/** Inside the app window; false in a plain browser during development. */
export const inTauri = "__TAURI_INTERNALS__" in window;

function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (inTauri || !import.meta.env.DEV) return tauriInvoke<T>(cmd, args);
  return import("./mock").then((m) => m.invoke<T>(cmd, args));
}

export type Stage = "metadata" | "fetch" | "transcribe" | "clean" | "file" | "write" | "done";
export type Status = "queued" | "running" | "waiting" | "failed" | "done" | "cancelled";

export interface Job {
  id: number;
  batch_id: string;
  input: string;
  url: string;
  video_key: string;
  stage: Stage;
  status: Status;
  reason: string | null;
  caption_error: string | null;
  attempts: number;
  review: boolean;
  note_path: string | null;
  folder: string | null;
  folder_new: boolean;
  title: string | null;
  channel: string | null;
}

export interface JobEvent {
  job_id: number;
  stage: Stage;
  status: Status;
  reason: string | null;
}

export interface CollectionReport {
  input: string;
  title: string;
  total: number;
  new: number;
}

export interface EnqueueReport {
  batch_id: string;
  added: number[];
  retried: number[];
  already_queued: number[];
  already_done: [string, string | null][];
  rejected: [string, string][];
  errors: [string, string][];
  collections: CollectionReport[];
}

export interface Preview {
  input: string;
  title: string | null;
  total: number;
  new: number;
  error: string | null;
}

export interface AppStatus {
  ready: boolean;
  error: string | null;
  out_dir: string;
  llm: string | null;
  paused: boolean;
}

export interface Topic {
  name: string;
  path: string;
  notes: number;
}

export interface Area {
  name: string;
  topics: Topic[];
}

export interface Folders {
  inbox: number;
  areas: Area[];
}

export interface TranscriptView {
  id: number;
  key: string;
  title: string | null;
  language: string | null;
  source: string;
  raw: string;
  cleaned: string | null;
  translated: boolean;
  review: boolean;
  model: string | null;
  note_path: string | null;
}

export interface Check {
  name: string;
  ok: boolean;
  detail: string;
}

/** Saved settings; unset fields use the defaults (core/src/settings.rs). */
export interface Settings {
  out_dir?: string | null;
  database_url?: string | null;
  llm_provider?: string | null;
  llm_model?: string | null;
  llm_base_url?: string | null;
  llm_concurrency?: number | null;
  chunk_tokens?: number | null;
  cleanup_prompt?: string | null;
  min_ratio?: number | null;
  max_ratio?: number | null;
  max_ratio_translated?: number | null;
  whisper_model?: string | null;
  gpu?: string | null;
  cookies_browser?: string | null;
  ig_delay_secs?: number | null;
  ig_batch?: number | null;
  ig_pause_secs?: number | null;
}

export interface Defaults {
  out_dir: string;
  database_url: string;
  llm_provider: string;
  llm_model: string;
  llm_base_url: string;
  llm_concurrency: number;
  chunk_tokens: number;
  cleanup_prompt: string;
  min_ratio: number;
  max_ratio: number;
  max_ratio_translated: number;
  whisper_model: string;
  gpu: string;
  ig_delay_secs: number;
  ig_batch: number;
  ig_pause_secs: number;
}

export interface SettingsView {
  settings: Settings;
  defaults: Defaults;
  keys: [string, boolean][];
  log_file: string;
}

export interface YtDlpInfo {
  path: string;
  version: string | null;
  age_days: number | null;
  managed: boolean;
  latest: string | null;
  error: string | null;
}

export interface RecleanResult {
  key: string;
  job_id: number | null;
  error: string | null;
}

export const api = {
  status: () => invoke<AppStatus>("status"),
  jobs: () => invoke<Job[]>("jobs"),
  enqueue: (lines: string[]) => invoke<EnqueueReport>("enqueue", { lines }),
  preview: (lines: string[]) => invoke<Preview[]>("preview", { lines }),
  retry: (id: number) => invoke<boolean>("retry", { id }),
  retryFailed: () => invoke<number>("retry_failed"),
  cancel: (id: number) => invoke<boolean>("cancel", { id }),
  reclean: (keys: string[]) => invoke<RecleanResult[]>("reclean", { keys }),
  setPaused: (paused: boolean) => invoke<boolean>("set_paused", { paused }),
  folders: () => invoke<Folders>("folders"),
  transcript: (id: number) => invoke<TranscriptView>("transcript", { id }),
  reveal: (id: number) => invoke<void>("reveal", { id }),
  openNote: (id: number) => invoke<void>("open_note", { id }),
  health: () => invoke<Check[]>("health"),
  getSettings: () => invoke<SettingsView>("get_settings"),
  saveSettings: (settings: Settings) => invoke<void>("save_settings", { settings }),
  setApiKey: (provider: string, key: string) => invoke<void>("set_api_key", { provider, key }),
  removeApiKey: (provider: string) => invoke<void>("remove_api_key", { provider }),
  listModels: (provider: string, baseUrl: string | null) => invoke<string[]>("list_models", { provider, baseUrl }),
  testLlm: (provider: string, model: string, baseUrl: string | null) =>
    invoke<number>("test_llm", { provider, model, baseUrl }),
  ytdlpInfo: () => invoke<YtDlpInfo>("ytdlp_info"),
  updateYtdlp: () => invoke<string>("update_ytdlp"),
  openLog: () => invoke<void>("open_log"),
};

/** Errors from commands arrive as plain strings; show them raw (PDA UI-8). */
export function errorText(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
}
