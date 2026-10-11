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
};

/** Errors from commands arrive as plain strings; show them raw (PDA UI-8). */
export function errorText(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
}
