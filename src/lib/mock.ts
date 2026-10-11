// Sample data for looking at the UI in a plain browser (`npm run dev`),
// where there is no Tauri backend. Never part of a production build.

import type { Folders, Job, Stage, Status, TranscriptView } from "./api";

const titles = [
  "How to think like a GENIUS UI/UX designer",
  "The secret behind weirdly perfect UI designs",
  "Drishyam The Conclusion Movie Explain In HINDI",
  "You ever find the cancel button for a",
  "Google just changed the future of UI/UX design...",
  "The stupid simple way to learn UI/UX design in exactly 10 minutes",
  "Me at the zoo",
  "Why every SaaS dashboard looks the same",
];
const states: [Stage, Status][] = [
  ["done", "done"],
  ["done", "done"],
  ["clean", "running"],
  ["transcribe", "running"],
  ["fetch", "running"],
  ["metadata", "queued"],
  ["clean", "waiting"],
  ["fetch", "failed"],
  ["done", "done"],
  ["metadata", "cancelled"],
];

const jobs: Job[] = Array.from({ length: 64 }, (_, i) => {
  const [stage, status] = states[i % states.length];
  const instagram = i % 5 === 3;
  const key = instagram ? `instagram:Dbo${String(i).padStart(8, "x")}` : `youtube:vid${String(i).padStart(8, "0")}`;
  return {
    id: 100 - i,
    batch_id: "00000000-0000-0000-0000-000000000000",
    input: "https://www.youtube.com/@kolejain",
    url: instagram ? `https://www.instagram.com/reel/${key.slice(10)}/` : `https://www.youtube.com/watch?v=${key.slice(8)}`,
    video_key: key,
    stage,
    status,
    reason:
      status === "failed"
        ? "yt-dlp failed (exit status: 1): ERROR: [youtube] vid: Sign in to confirm you're not a bot"
        : status === "waiting"
          ? "LLM not reachable at http://localhost:1234/v1/chat/completions: connection refused"
          : null,
    caption_error: null,
    attempts: status === "failed" ? 1 : 0,
    review: i % 11 === 1,
    note_path: status === "done" ? `/vault/Shruti/Design/UX/${titles[i % titles.length]}.md` : null,
    folder: status === "done" ? (i % 3 === 0 ? "Design/UX" : i % 3 === 1 ? "Film/Analysis" : "Inbox") : null,
    folder_new: i === 1,
    title: stage === "metadata" ? null : titles[i % titles.length],
    channel: stage === "metadata" ? null : instagram ? "synsation_" : i % 2 ? "Kole Jain" : "Some Channel",
  };
});

const folders: Folders = {
  inbox: 3,
  areas: [
    { name: "Design", topics: [{ name: "UX", path: "Design/UX", notes: 14 }, { name: "Motion", path: "Design/Motion", notes: 2 }] },
    { name: "Film", topics: [{ name: "Analysis", path: "Film/Analysis", notes: 6 }] },
  ],
};

const transcript: TranscriptView = {
  id: 98,
  key: "youtube:vid00000002",
  title: titles[2],
  language: "hi",
  source: "captions",
  raw: "दोस्तों आज हम बात करेंगे दृश्यम के बारे में। यह फिल्म एक परिवार की कहानी है।\n\nविजय अपने परिवार को बचाने के लिए हर हद पार कर जाता है।",
  cleaned:
    "Today we'll talk about Drishyam. The film is the story of a family.\n\nVijay crosses every line to protect his family.",
  translated: true,
  review: false,
  model: "lmstudio/google/gemma-4-31b",
  note_path: "/vault/Shruti/Film/Analysis/Drishyam.md",
};

const defaults = {
  out_dir: "/home/me/Documents/Obsidian Vault/Shruti",
  database_url: "postgres://me@%2Fvar%2Frun%2Fpostgresql/shruti",
  llm_provider: "lmstudio",
  llm_model: "google/gemma-4-31b",
  llm_base_url: "http://localhost:1234/v1",
  llm_concurrency: 1,
  chunk_tokens: 1200,
  cleanup_prompt: "You clean up raw video transcripts…",
  min_ratio: 0.25,
  max_ratio: 1.1,
  max_ratio_translated: 1.6,
  whisper_model: "/repo/models/ggml-large-v3-turbo-q5_0.bin",
  gpu: "auto",
  ig_delay_secs: 3,
  ig_batch: 20,
  ig_pause_secs: 60,
};

const replies: Record<string, (args: Record<string, unknown>) => unknown> = {
  get_settings: () => ({
    settings: { cookies_browser: "brave" },
    defaults,
    keys: [["openai", false], ["gemini", true], ["custom", false]],
    log_file: "/home/me/.local/share/shruti/shruti.log",
  }),
  health: () => [
    { name: "Postgres", ok: true, detail: "connected" },
    { name: "LLM", ok: false, detail: "LLM not reachable at http://localhost:1234/v1/models: connection refused" },
    { name: "GPU", ok: true, detail: "Vulkan device 1" },
    { name: "Whisper", ok: true, detail: defaults.whisper_model },
    { name: "Queue", ok: true, detail: "running" },
  ],
  ytdlp_info: () => ({
    path: "/usr/bin/yt-dlp",
    version: "2026.08.19",
    age_days: 54,
    managed: false,
    latest: "2026.10.01",
    error: null,
  }),
  list_models: () => ["google/gemma-4-31b", "qwen/qwen3-8b"],
  test_llm: () => 1234,
  status: () => ({ ready: true, error: null, out_dir: "/vault/Shruti", llm: "lmstudio/google/gemma-4-31b", paused: false }),
  jobs: () => jobs,
  folders: () => folders,
  transcript: () => transcript,
  preview: (a) =>
    (a.lines as string[]).map((input) => ({ input, title: "Kole Jain", total: 52, new: 51, error: null })),
  enqueue: (a) => ({
    batch_id: "x",
    added: (a.lines as string[]).map((_, i) => 1000 + i),
    retried: [],
    already_queued: [],
    already_done: [],
    rejected: [],
    errors: [],
    collections: [],
  }),
};

export async function invoke<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  const reply = replies[cmd];
  return (reply ? reply(args) : true) as T;
}
