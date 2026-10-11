# Shruti — Tech Stack & Architecture

> Product requirements live in [PDA.md](PDA.md). Where the two disagree, PDA.md wins.

A desktop app. You paste links and it writes a cleaned, English transcript for
each one into the Obsidian vault. YouTube videos, YouTube channels/playlists,
Instagram reels, and whole Instagram profiles.

**Decision: we are building this as a Tauri v2 app.** Not a web page, not a PWA.
The reasoning is at the bottom under "Why Tauri".

---

## The shape of the thing

The engine is two external binaries plus a local LLM. Everything else is
plumbing and UI.

```
URL
 │
 ├─ yt-dlp ──► captions already exist?  ──► yes ──► .vtt  ──┐
 │                                                           │
 │                                      ──► no  ──► 16kHz mono .wav
 │                                                           │     │
 │                                        whisper.cpp ◄───────┘     │
 │                                             │                   │
 └─────────────────────────────────────────────┴──► raw text ──► Postgres
                                                                     │
                                   LLM (LM Studio / OpenAI / Gemini): strip fluff,
                                   translate Hindi → English
                                                                     │
                                                    Postgres + .md note in vault
```

Two important consequences of this:

1. **We do not need `youtube-transcript-api` or any Python at all.** yt-dlp
   fetches captions itself with `--write-auto-subs --skip-download`. When
   captions exist (most YouTube videos), a transcript takes about a second and
   never touches Whisper.
2. **Instagram reels have no captions, ever.** Every reel goes through the
   download → Whisper path. This is the slow path and it's the one worth
   optimising.

---

## Frontend

The UI is a queue. A box to paste links into, a table of jobs with live status,
a transcript viewer, and a settings page. That's it. Pick the stack that makes a
dense, fast table look good, and ignore everything built for marketing pages.

| Concern | Choice | Why |
| --- | --- | --- |
| Framework | **React + TypeScript + Vite** | Tauri's default path. Same stack as the desktop-surface project, so the learning carries over. |
| Styling | **Tailwind CSS v4** | CSS-first config, no `tailwind.config.js`, faster builds. |
| Components | **shadcn/ui** | Radix primitives + Tailwind, copy-pasted into the repo so we own and restyle the code. No runtime dependency, no fighting a library's opinions. |
| Table | **TanStack Table** | Headless. Sorting/filtering/selection over the job list without inheriting someone's visual design. |
| Long lists | **TanStack Virtual** | A 400-reel profile should not render 400 DOM rows. |
| State | **Zustand** | One small store. The app's state is a job list and a settings object — Redux would be ceremony for nothing. |
| Routing | **React Router** | Main view / Settings / Transcript viewer. |
| Icons | **Lucide** | Clean, consistent, tree-shakes. |
| Toasts | **Sonner** | For "batch finished", "yt-dlp is out of date", errors. |
| Fonts | **Inter** (UI), **JetBrains Mono** (transcript text) | Bundle them locally; don't hit Google Fonts from a desktop app. |
| Animation | CSS transitions only | Restraint reads as native. Add Motion later only if something specifically needs it. |

### Making it look like an app and not a website

This is the part that actually decides whether the UI feels clean, and none of
it comes from the component library:

- **Base font size 13–14px, not 16px.** Web defaults are tuned for reading
  articles at arm's length. Desktop apps are denser. This single change does
  more for the "native" feel than anything else.
- **Tighten the spacing scale.** shadcn's defaults are generous. Halve the
  vertical padding on table rows, buttons, and inputs.
- **One accent colour, used sparingly.** Status is communicated by a small
  coloured dot plus a text label, never by colour alone.
- **Respect the system theme** via `prefers-color-scheme`. KDE Plasma's dark
  mode propagates into the webview correctly, so this works for free.
- **No window chrome mismatch.** Either use the native KDE titlebar or go fully
  custom with `decorations: false` and draw our own. Half-and-half always looks
  wrong.
- **Keyboard first.** `Ctrl+V` into the link box from anywhere, `Enter` to
  enqueue, `Ctrl+,` for settings, `/` to filter the queue.

### UX decisions that matter more than the visuals

- **Paste many links at once**, one per line, mixed YouTube and Instagram. Type
  is detected from the URL, not from a dropdown the user has to set.
- **Stream the transcript as it is produced.** Whisper emits segments
  progressively; show them appearing. A 40-second transcription that you can
  watch feels faster than a 15-second one behind a spinner.
- **Never redo finished work.** Dedupe on the platform's video ID in Postgres. Re-running
  a 300-video channel should skip straight to what's new.
- **A failure never stops the queue.** Mark the item failed, keep going, show a
  retry button on that row. Instagram *will* fail intermittently; the app has to
  shrug it off.
- **Per-job state is explicit**: `queued → fetching metadata → downloading →
  transcribing → writing → done`, or `failed` with the reason kept.
- **Write Obsidian-friendly output.** One `.md` per video with YAML frontmatter:

  ```yaml
  ---
  title: "..."
  url: https://...
  channel: "..."
  platform: youtube
  duration: 612
  upload_date: 2026-03-14
  transcribed: 2026-10-11
  source: captions   # or: whisper
  ---
  ```

  The vault is right there. Transcripts that arrive as proper notes with
  frontmatter are immediately searchable and linkable.
- **System notification when a batch finishes**, because a 300-video channel is
  something you walk away from.

---

## Backend (Rust)

| Concern | Choice | Notes |
| --- | --- | --- |
| Framework | **Tauri v2** | |
| Async | **tokio** | Already inside Tauri. |
| Queue | **`tokio::sync::mpsc` + `Semaphore`** | Hand-rolled worker pool. See the concurrency note below — this is the one piece worth writing ourselves. |
| Database | **sqlx** + local **Postgres 18** | The Postgres server already running on this machine (`localhost:5432`, database `shruti`). Async, compile-time-checked queries, `sqlx::migrate!` runs migrations on startup. Holds the queue, dedupe keys, raw and cleaned transcripts. |
| LLM | **reqwest** → any OpenAI-compatible `/chat/completions` endpoint | One client for every provider, which differ only by base URL + key: LM Studio (`http://localhost:1234/v1`), OpenAI (`https://api.openai.com/v1`), Gemini via Google's OpenAI-compatible endpoint (`https://generativelanguage.googleapis.com/v1beta/openai/`), or a custom URL. Models come from each provider's `/models`. Behind a `Cleaner` trait. Default: local Gemma 4 31B. |
| Folder watching | **notify** crate | Watches the `Shruti/` tree so folders created, renamed or deleted in Obsidian show up in the app live. The disk is the source of truth. |
| Secrets | **keyring** crate | API keys go in the Secret Service (KWallet on KDE), never in the store JSON or Postgres. |
| Subprocesses | **tauri-plugin-shell** sidecars | Resolves the correct bundled binary path per platform. Stream stdout line by line. |
| Progress → UI | **Tauri `ipc::Channel`** | Better than `emit()` for high-frequency streaming like whisper segments. |
| Errors | **thiserror** at the command boundary, **anyhow** internally | Command errors need `Serialize` to cross into the frontend. |
| Logging | **tracing** + **tauri-plugin-log** | Logs to a file. Essential, because most failures will be yt-dlp failures we need to read afterwards. |
| Settings | **tauri-plugin-store** | JSON in the app config dir. |

**Plugins:** `shell` (sidecars), `dialog` (folder picker), `fs` (write
transcripts), `opener` (reveal in file manager), `store` (settings), `log`,
`single-instance`, `notification`. `updater` later if we want it.

### Concurrency — the actual performance win

Downloading is network-bound. Transcribing is compute-bound. Running them in
one sequential loop wastes half the machine. Two separate limits:

- **Downloads: 2–3 concurrent.** Not more for Instagram — see the warning below.
- **Transcription: exactly 1.** Whisper already saturates every core or the GPU.
  A second concurrent job makes both slower.
- **LLM cleanup: exactly 1 when local** (1–3 min/video on Gemma 4 31B).
  Configurable for cloud providers (default 2) with an optional requests-per-minute
  cap, and backoff on 429s that honours `Retry-After`. Its own queue either way,
  so it never blocks downloads or Whisper.

With the download of video *n+1* overlapping the transcription of video *n*, a
large Instagram profile finishes in close to half the time of a naive loop. This
is the single biggest speed gain available and it costs almost nothing to build.

### Keep the transcriber swappable

```rust
trait Transcriber {
    async fn transcribe(&self, audio: &Path, opts: &Opts) -> Result<Transcript>;
}
```

Start with `SidecarWhisper`, which shells out to the `whisper-cli` binary — much
easier to get working. Later, `WhisperRs` using the **whisper-rs** crate
compiles whisper.cpp directly into our binary: no process spawn, real progress
callbacks, one less file to ship. Behind the trait, that swap touches one module.

---

## The three bundled binaries

| Binary | Role |
| --- | --- |
| **yt-dlp** | Metadata, channel/profile expansion, captions, audio download |
| **ffmpeg** | Audio → 16kHz mono WAV (whisper.cpp needs exactly this) |
| **whisper-cli** | whisper.cpp, compiled for the right backend |

ffmpeg is genuinely required — whisper.cpp reads WAV through `dr_wav` and nothing
else. yt-dlp can hand us the correct format in one step:

```
yt-dlp --js-runtimes node -f bestaudio/best -x --audio-format wav \
       --postprocessor-args "ffmpeg:-ar 16000 -ac 1" <url>
```

**Measured in step 1 (2026-10-11), and now encoded in `core/`:**

- **yt-dlp needs a JS runtime for YouTube.** Without one it warns that formats
  may be missing. `--js-runtimes node` uses the Node 22 already installed.
- **Request exactly one caption track** (`--sub-langs hi-orig`, never `en.*`).
  A pattern also pulls YouTube's machine translations and got a 429.
- **Caption choice:** the spoken language comes from metadata `language`
  (normalised: `en-US` → `en`), else the `<lang>-orig` auto track. Prefer
  manual subs, then `<lang>-orig` (YouTube's own speech recognition of the
  original audio). Unknown language → Whisper, never a guessed track.
- **Fetch captions as `json3`**, not VTT. It has no rolling-line duplication.
  Strip `[Music]`/`[संगीत]` markers.
- **yt-dlp exits 0 when a requested caption track doesn't exist.** A missing
  file is the failure signal.
- **Hindi: YouTube's auto-captions beat Whisper clearly.** On a real Hindi
  video, captions were accurate and punctuated, while Whisper dropped whole
  phrases and misspelled words. Hindi YouTube uses captions.
- **whisper-cli flags:** `--vad --vad-model ggml-silero-v5.1.2.bin` roughly
  doubles the speech captured on music-backed audio. `-nf` (no temperature
  fallback) is 5× faster on the same audio with the same output once VAD is
  on. `-dev N` is required (see GPU below).
- **whisper JSON can contain invalid UTF-8** (Devanagari split across tokens),
  so it's decoded lossily.
- **Browser cookies go to Instagram only.** YouTube answers a logged-in
  browser session's cookies with "The page needs to be reloaded" (measured
  in step 2), so `--cookies-from-browser` is never passed for YouTube URLs.

Tauri bundles these via `externalBin` in `tauri.conf.json`, with each file named
using its target triple suffix (e.g. `yt-dlp-x86_64-unknown-linux-gnu`).

### ⚠️ yt-dlp going stale is the #1 way this app breaks

Instagram and YouTube break extractors regularly, sometimes monthly. A pinned,
bundled yt-dlp will silently stop working and the error will look like a bug in
our code. Build the fix in from day one — pick one:

1. Use the **system** `yt-dlp` and show its version in Settings with a warning
   when it's old.
2. Bundle it **and** add an "Update yt-dlp" button that pulls the latest release
   binary into the app data dir, preferring that copy over the bundled one.

Option 2 is better UX, option 1 is less work. Either way, the version must be
visible in the UI.

---

## Whisper model

**`ggml-large-v3-turbo-q5_0.bin`** (~550 MB) is the default. Near large-v3
quality at several times the speed, and it holds up on Hindi where the
small/medium models fall apart. Turbo can't translate reliably, so Whisper
transcribes Hindi as Hindi and the LLM translates.

Download the model on first run rather than shipping it — it keeps the installer
at ~15 MB and lets us offer a couple of model choices in Settings.

### GPU backend — decided: Vulkan

whisper.cpp needs to be compiled for a specific backend, and the difference is
large:

| Backend | Rough speed on a 60s reel |
| --- | --- |
| CPU (quantized turbo) | 4–12 s |
| Vulkan (any modern AMD/Intel/NVIDIA GPU) | 1–3 s |
| CUDA (NVIDIA) | under 1 s |

**Decided:** this machine has an RTX 4060 Laptop GPU on the open-source
nouveau/NVK driver (no proprietary driver, so no CUDA). Vulkan works on it via
NVK, so whisper.cpp is built with `-DGGML_VULKAN=1`.

**LM Studio must load models with GPU offload off** (`lms load
google/gemma-4-31b -c 8192 --gpu off`). Measured in step 2b: its automatic
offload crashes llama-server (SIGABRT at 89% loaded) under NVK. CPU-only
loads in ~8 s and uses ~18.5 GB of RAM, and leaves the 4060 to Whisper. The
cost is speed: a one-word round trip takes ~25 s, and cleaning an 83 s reel
takes several minutes. A cloud provider (Gemini, OpenAI) is the fast path
for big batches.

**Vulkan device 0 is the Intel Arc iGPU, and whisper.cpp picks it by
default.** The core probes the device list once at startup and passes
`-dev` with the first discrete (`uma: 0`) GPU. Measured on a 19 s clip:

| Device | Time (warm) |
| --- | --- |
| RTX 4060 (Vulkan/NVK) | 1.2 s |
| Intel Arc iGPU (Vulkan) | 7.8 s |
| CPU, 22 threads | 18 s |

The first GPU run after a reboot takes ~4 s extra while shaders compile.

**Building whisper-cli** (sources in `vendor/`, gitignored). Fedora's
`spirv-headers-devel` isn't installed, so SPIRV-Headers is built into a local
prefix instead of system-wide:

```bash
git clone --depth 1 https://github.com/ggml-org/whisper.cpp vendor/whisper.cpp
git clone --depth 1 https://github.com/KhronosGroup/SPIRV-Headers vendor/SPIRV-Headers
cmake -S vendor/SPIRV-Headers -B vendor/SPIRV-Headers/build -DCMAKE_INSTALL_PREFIX=$PWD/vendor/prefix
cmake --install vendor/SPIRV-Headers/build
cmake -S vendor/whisper.cpp -B vendor/whisper.cpp/build -DGGML_VULKAN=1 \
  -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DWHISPER_BUILD_TESTS=OFF \
  -DCMAKE_PREFIX_PATH=$PWD/vendor/prefix \
  -DCMAKE_CXX_FLAGS=-I$PWD/vendor/prefix/include -DCMAKE_C_FLAGS=-I$PWD/vendor/prefix/include
cmake --build vendor/whisper.cpp/build -j --target whisper-cli
```

Models live in `models/` (gitignored): `ggml-large-v3-turbo-q5_0.bin` and
`ggml-silero-v5.1.2.bin`, both from Hugging Face (`ggerganov/whisper.cpp`,
`ggml-org/whisper-vad`).

---

## Instagram, realistically

- **Cookies are required** for anything beyond the occasional public reel.
  yt-dlp reads them straight from the browser:
  `--cookies-from-browser firefox`.
- **Rate limiting is the real constraint**, not transcription speed. Space
  requests a few seconds apart and keep download concurrency low.
- **Pulling a whole profile in one burst is the fastest way to get an account
  flagged.** Use a secondary account, and have the app process profiles in
  batches with a configurable delay rather than hammering straight through.
- Scraping is against Instagram's ToS. Fine for personal use on public content;
  think harder before this becomes anything public-facing.
- Extractor behaviour here changes constantly, so treat any specific finding as
  something to re-verify rather than a fixed fact.
- **As of yt-dlp 2026.08.19, `instagram:user` is marked "CURRENTLY BROKEN"**,
  so whole-profile expansion (PDA IG-1) can't rely on yt-dlp. Step 3 needs
  another way to list a profile's reels. Single reels still go through yt-dlp,
  and need cookies even for public ones.

---

## Postgres (step 2)

- **Connection:** the local server over its Unix socket, as your OS user
  (peer authentication, no password). sqlx 0.9 rejects an empty host and
  doesn't default the user, so the URL is
  `postgres://<user>@%2Fvar%2Frun%2Fpostgresql/shruti`. Override with
  `SHRUTI_DATABASE_URL`.
- **Schema:** `videos` (keyed `platform:id`, with caption track codes so a
  restart doesn't re-ask yt-dlp), `transcripts` (insert-only: raw text,
  timed segments as jsonb, caption fallback reason), `jobs` (`stage` = next
  stage to run, `status`, `reason`, one open job per video via a partial
  unique index). Migrations run on startup and are additive only.
- **Queue:** a dispatcher claims jobs with `FOR UPDATE SKIP LOCKED` and runs
  each through metadata → fetch → transcribe → clean → file → write.
  Downloads (3) and Whisper (1) have separate semaphores, so a download
  overlaps a transcription. Audio waits in `~/.cache/shruti/work/<key>/`
  between fetch and transcribe so a restart doesn't re-download it.
- **Tests:** `#[sqlx::test]` creates a throwaway database per test, using
  `DATABASE_URL` from `.cargo/config.toml` (the `shruti_test` base database).

## Project layout

```
shruti/
├── Cargo.toml                    # workspace: core (+ src-tauri later)
├── core/                         # shruti-core: link → transcript, no UI (step 1)
│   ├── src/
│   │   ├── source.rs             # URL → Source (IN-2)
│   │   ├── metadata.rs           # yt-dlp -J → VideoMeta, caption-track choice
│   │   ├── captions.rs           # json3 → segments
│   │   ├── ytdlp.rs              # Fetcher trait + yt-dlp
│   │   ├── whisper.rs            # Transcriber trait + whisper-cli, GPU probe
│   │   ├── pipeline.rs           # stages: metadata, fetch (captions or audio), transcribe
│   │   ├── db.rs                 # Postgres: videos, insert-only transcripts, jobs
│   │   ├── queue.rs              # resumable staged queue, download/whisper limits
│   │   ├── output.rs             # note filename, frontmatter, never-overwrite write
│   │   ├── config.rs             # SHRUTI_* env + repo defaults
│   │   └── bin/shruti.rs         # dev CLI: shruti [--out DIR] <link>…
│   ├── migrations/               # sqlx migrations (additive only)
│   └── tests/                    # fixtures from real yt-dlp/whisper output, live.rs (ignored)
├── vendor/                       # whisper.cpp + SPIRV-Headers sources (gitignored)
├── models/                       # Whisper + VAD models (gitignored)
├── src/                          # React frontend
│   ├── components/
│   │   ├── ui/                   # shadcn, owned by us
│   │   ├── LinkInput.tsx
│   │   ├── JobQueue.tsx
│   │   └── TranscriptView.tsx
│   ├── routes/
│   │   ├── Main.tsx
│   │   └── Settings.tsx
│   ├── store/                    # Zustand
│   └── lib/tauri.ts              # typed invoke + channel wrappers
├── src-tauri/
│   ├── src/
│   │   ├── main.rs
│   │   ├── commands.rs           # the frontend-facing API
│   │   ├── queue/                # worker pool, semaphores
│   │   ├── sources/              # URL parsing, channel/profile expansion
│   │   ├── transcribe/           # Transcriber trait + impls
│   │   ├── cleanup/              # Cleaner trait, OpenAI-compatible client, providers, chunking, guardrail
│   │   ├── output/               # markdown + frontmatter writer
│   │   ├── folders/              # auto-sorter, Shruti/ tree, .shruti descriptions, watcher, near-duplicate check
│   │   └── db.rs                 # sqlx + Postgres
│   ├── migrations/               # sqlx migrations
│   ├── binaries/                 # yt-dlp, ffmpeg, whisper-cli sidecars
│   └── tauri.conf.json
└── package.json
```

---

## Why Tauri

Ruling out the alternatives, since the reasoning matters later:

- **Web page / PWA: impossible.** A browser cannot download from YouTube or
  Instagram (they block it, and CORS blocks you first), and cannot run Whisper
  at a useful speed. Not a difficulty — an actual wall.
- **Local web app** (Python server + HTML at localhost): easiest to build, maybe
  a weekend. But it needs a terminal to start, has no app icon, and isn't the
  thing that was asked for.
- **Electron:** would work, ships ~150 MB, and Node spawning subprocesses is
  easier than Rust doing it. Rejected for the bundle size and because it teaches
  us nothing we want.
- **Tauri:** ~10 MB, a real app, and the Rust side is genuinely well suited to
  supervising subprocesses and a job queue. Rust is already installed on this
  machine.

One more reason: this is a much better first Tauri project than the
desktop-surface idea. It's an ordinary window, so none of the Wayland
window-layering problems apply. The Tauri knowledge transfers; the frustration
doesn't.

---

## Build order

1. ✅ **Rust core, no UI.** URL → transcript file, driven by a test. Prove yt-dlp
   captions, the ffmpeg conversion, and whisper all work on this machine.
   (Done 2026-10-11: `core/`, 72 unit tests + 4 live tests.)
2. ✅ **Postgres + the worker pool.** Resumability and the
   download/transcribe/cleanup overlap. (Done 2026-10-11: `db.rs`,
   `queue.rs`, migration `0001_init.sql`.)
2b. **LLM cleanup stage.** Prompt, chunking, length guardrail, re-clean.
3. **Channel and profile expansion.** The part most likely to be fiddly.
4. **UI over the working core.** Queue table, live progress, transcript viewer.
5. **Settings, notifications, the yt-dlp version check.**
6. **Packaging.** `.rpm` for Fedora, AppImage as the portable fallback.
