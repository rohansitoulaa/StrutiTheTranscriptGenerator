import type { Job, Stage, Status } from "./api";

/** Mirrors `queue::state_label` in the core (PDA Q-1). */
export function stateLabel(job: Pick<Job, "stage" | "status">): string {
  switch (job.status) {
    case "queued":
      return "queued";
    case "waiting":
      return "waiting";
    case "failed":
      return "failed";
    case "cancelled":
      return "cancelled";
    case "done":
      return "done";
  }
  const running: Record<Stage, string> = {
    metadata: "fetching metadata",
    fetch: "downloading",
    transcribe: "transcribing",
    clean: "cleaning",
    file: "filing",
    write: "writing",
    done: "done",
  };
  return running[job.stage];
}

export type Tone = "idle" | "run" | "ok" | "warn" | "bad";

export function tone(job: Pick<Job, "status" | "review">): Tone {
  const tones: Record<Status, Tone> = {
    queued: "idle",
    running: "run",
    waiting: "warn",
    failed: "bad",
    done: "ok",
    cancelled: "idle",
  };
  return job.status === "done" && job.review ? "warn" : tones[job.status];
}

const ORDER: Stage[] = ["metadata", "fetch", "transcribe", "clean", "file", "write", "done"];

/** 0..1 through the stages. */
export function progress(job: Pick<Job, "stage" | "status">): number {
  if (job.status === "done") return 1;
  return ORDER.indexOf(job.stage) / (ORDER.length - 1);
}

export function platform(job: Pick<Job, "video_key">): "youtube" | "instagram" {
  return job.video_key.startsWith("instagram:") ? "instagram" : "youtube";
}

/** A stored transcript exists from the clean stage on. */
export function hasTranscript(job: Pick<Job, "stage">): boolean {
  return ORDER.indexOf(job.stage) >= ORDER.indexOf("clean");
}

export function isOpen(job: Pick<Job, "status">): boolean {
  return job.status === "queued" || job.status === "running" || job.status === "waiting";
}
