# Shruti — Product Requirements

| | |
| --- | --- |
| Status | **Final v1.3**: approved 2026-10-11 |
| Date | 2026-10-11 |
| User | Just me, on this Fedora laptop |
| Companion doc | [TECH-STACK.md](TECH-STACK.md) covers *how* it is built. This doc covers *what* it must do. |

---

## 1. What Shruti is

A desktop app that turns YouTube videos and Instagram reels into clean,
information-only English notes in my Obsidian vault.

I paste links (single videos, whole channels, whole Instagram profiles). Shruti
gets a transcript for each one, strips out the talking-head fluff (the "hey
what's up guys", the sponsor read, the "smash that like button", the outro),
translates Hindi to English, and writes one Markdown note per video. The full,
uncut transcript is kept in Postgres so nothing is ever lost.

### The problem it solves

Most of what I want from a video is a few minutes of actual information wrapped
in intros, plugs, and filler. I want to read that information, search it in
Obsidian, and link it to my other notes, without watching the video or reading
a raw auto-caption dump.

### Success looks like

- I paste a 300-video channel, walk away, and come back to 300 clean notes,
  with failures clearly marked and retryable.
- A note reads like the speaker's content with the packaging removed, in plain
  English paragraphs.
- Re-running the same channel next month only processes the new videos.
- If the cleanup ever cut something real, I can get the original back from the
  database.

---

## 2. Decisions made

| Topic | Decision |
| --- | --- |
| Audience | Personal tool. No installer polish, auto-updater, or cross-platform builds. |
| Sources | YouTube (videos, Shorts, playlists, channels) and Instagram (single reels, whole profiles). |
| Languages in | English and Hindi (including Hindi mixed with English). |
| Language out | **Always English.** Hindi is translated. |
| Note format | Plain paragraphs. No timestamps in the note. |
| Cleanup | An LLM cuts the fluff and keeps the speaker's wording. |
| LLM provider | **Pluggable, local or cloud.** LM Studio (local), OpenAI, Google Gemini (a free-tier API key works), or any OpenAI-compatible endpoint. Provider and model are picked in Settings. Default is local Gemma 4 31B, since it needs no key and nothing leaves the machine. |
| Raw transcript | Always stored in Postgres, never shown in the note. Can be pulled out or re-cleaned anytime. |
| Database | The local **Postgres 18** server already running, used for everything (queue, dedupe, transcripts). Replaces SQLite. |
| Output location | A root `Shruti/` folder in the Obsidian vault. **Auto-sorted:** the LLM files each video into an `Area/Topic` folder (e.g. `Design/UX`) by what it's about, and creates the folder if none fits. No manual folder creation, no destination picker, no per-channel subfolders. |
| yt-dlp updates | Bundled, version shown in Settings, plus an in-app "Update yt-dlp" button. |
| Whisper GPU | Vulkan on the RTX 4060 through the NVK driver. No proprietary driver or CUDA. |
| UI copy | **None.** Built by me, for me. Short labels only: no descriptions, helper text, tooltips, subtitles or onboarding. |

---

## 3. How a link becomes a note

```
link ──► detect platform + type ──► expand (channel / playlist / profile → videos)
                                          │
                     for each video, skip if already done in Postgres
                                          │
              ┌───────────────────────────┴──────────────────────────┐
     YouTube has captions?                                 no captions / Instagram
              │                                                       │
     fetch captions (en or hi)                         download audio → 16 kHz WAV
              │                                                       │
              │                                          Whisper transcribes (en or hi)
              └───────────────────────────┬──────────────────────────┘
                                          │
                         store RAW transcript + segments in Postgres
                                          │
                     LLM: strip fluff  (+ translate if Hindi)  → English
                                          │
                     store CLEANED text in Postgres
                                          │
              auto-sort: pick the best existing Area/Topic folder,
              or create one if nothing fits (Inbox only if topic unclear)
                                          │
                              write .md into that folder
```

Every stage records its state, so a crash or app restart resumes where it left
off and never repeats finished stages.

---

## 4. Requirements

Priority: **Must** means v1 is not done without it. **Should** means v1 wants it
but can ship without it. **Later** means explicitly deferred.

### 4.1 Input

| ID | Requirement | Priority |
| --- | --- | --- |
| IN-1 | Paste one or many links at once, one per line, mixed YouTube and Instagram. | Must |
| IN-2 | Detect platform and type from the URL alone: YouTube video, Short, playlist, channel (`/@handle`, `/channel/…`, `/c/…`); Instagram reel, profile. No dropdowns. | Must |
| IN-3 | Lines that aren't supported links are flagged inline with a reason and are not enqueued. Valid lines in the same paste still go through. | Must |
| IN-4 | Channels, playlists and profiles expand into individual videos before processing. Show the count ("Channel X: 312 videos, 40 new") before starting. | Must |
| IN-5 | `Ctrl+V` anywhere pastes into the link box. `Enter` enqueues. | Should |
| IN-6 | Import links from a `.txt` file. | Later |

**Acceptance:** pasting a channel URL, a reel URL, a Short URL and a line of junk
gives three enqueued jobs (the channel expanded) and one inline error.

### 4.2 Getting the transcript

| ID | Requirement | Priority |
| --- | --- | --- |
| TR-1 | For YouTube, prefer existing captions (manual first, then auto-generated) in the video's spoken language. Only fall back to Whisper when none exist. | Must |
| TR-2 | Every Instagram reel goes through Whisper (reels never have captions). | Must |
| TR-3 | Whisper runs on the GPU (Vulkan). The app shows which device it's using in Settings and warns if it fell back to CPU. | Must |
| TR-4 | The spoken language (English or Hindi) is detected from metadata, caption language, or Whisper's own detection. No manual setting per video. | Must |
| TR-5 | Hindi is transcribed *as Hindi*. Translation happens in the cleanup step, not in Whisper (the turbo Whisper model can't translate reliably). | Must |
| TR-6 | Timestamped segments are kept in the database even though notes don't show them. | Must |
| TR-7 | While Whisper is running, segments stream into the transcript view live. | Should |
| TR-8 | The Whisper model downloads on first run, with progress, rather than shipping in the app. | Must |

**Acceptance:** a captioned English YouTube video reaches "transcribed" in under
5 seconds without Whisper running. A 60-second Hindi reel produces a Hindi raw
transcript in the database.

### 4.3 Cleanup (fluff removal and translation)

This is the feature that makes Shruti more than a transcriber.

**Cut:**
- Greetings and openers: "hey what's up guys", "welcome back to the channel",
  "before we start…"
- Channel housekeeping: like/subscribe/bell requests, "link in the description",
  "comment below", Patreon and merch plugs
- Sponsor reads and ad segments
- Outros and sign-offs: "that's it for today", "see you in the next one"
- Filler and repetition: "um", "you know", "basically", false starts, the same
  point said twice in a row

**Keep:**
- Every piece of actual information, in the order the speaker gave it
- The speaker's own wording. This is editing by deletion, not rewriting or
  summarising.

| ID | Requirement | Priority |
| --- | --- | --- |
| CL-1 | Every transcript goes through the LLM cleanup above before a note is written. | Must |
| CL-2 | Hindi transcripts are translated to natural English in the same pass, with the same cut and keep rules. | Must |
| CL-3 | Output is plain paragraphs: no bullet points, headings, timestamps, or commentary from the model ("Here is the cleaned transcript:"). | Must |
| CL-4 | The model must not add information that isn't in the transcript. The prompt says so explicitly, and the app checks it (CL-6). | Must |
| CL-5 | Long videos are cleaned in chunks sized to the model's context window. Chunk boundaries must not show in the output (no dropped or doubled sentences at seams). | Must |
| CL-6 | **Guardrail:** if the cleaned text is suspiciously short or long compared with the raw text (thresholds in Settings, default under 25% or over 110% of raw length), the note is still written but marked `cleanup: review` and the row shows a warning. | Must |
| CL-7 | The provider and model doing the cleanup are chosen in Settings. See *LLM providers* below. | Must |
| CL-8 | If the provider is unavailable (LM Studio not running, model not loaded, no network, bad API key), jobs wait in a `waiting for LLM` state instead of failing, and Settings shows the reason. The rest of the queue (downloads, Whisper) keeps going. | Must |
| CL-9 | **Re-clean:** rerun cleanup on any video, or all videos from a channel, from the stored raw transcript, without downloading or transcribing again. The re-clean can use a different provider or model than the default, e.g. redo a batch done locally with a stronger cloud model. | Must |
| CL-10 | The cleanup prompt can be edited in Settings, with a "reset to default" button. | Should |
| CL-11 | Start LM Studio's server and load the model automatically (via `lms`) when it's needed. | Should |

#### LLM providers

| ID | Requirement | Priority |
| --- | --- | --- |
| LLM-1 | Supported providers: **LM Studio** (local), **OpenAI**, **Google Gemini**, and **Custom**: any OpenAI-compatible endpoint given a base URL and key (covers OpenRouter, Groq, Ollama and similar). | Must |
| LLM-2 | Adding another OpenAI-compatible provider needs only a base URL and key. No code changes. | Must |
| LLM-3 | Model names are never hardcoded. The model dropdown is filled from the provider's own model list, so a newer model (e.g. the current Gemini Flash-Lite) shows up without an app update. | Must |
| LLM-4 | A **Test** button per provider sends a tiny prompt and reports success, the model's reply time, or the exact error (bad key, no quota, model not found). | Must |
| LLM-5 | API keys are stored in the system keyring (KWallet on this machine), never in the settings file, Postgres, logs, or notes. | Must |
| LLM-6 | **Rate limits and quota:** on a rate-limit or quota error, back off and retry, honouring the provider's retry-after hint. Jobs show `waiting for quota` instead of failing. When a daily free-tier quota runs out, the remaining cleanup jobs pause until it resets (or until I switch provider), and downloads and Whisper carry on. | Must |
| LLM-7 | Concurrency per provider: local is always 1. Cloud is configurable (default 2), with an optional requests-per-minute cap to stay inside free-tier limits. | Must |
| LLM-8 | Chunk size follows the selected model's context window. Cloud models usually take a whole transcript in one request. | Must |
| LLM-9 | Only transcript text is ever sent to a cloud provider, never audio. | Must |
| LLM-10 | **Fallback provider:** optionally name a second provider to use when the primary is down or out of quota (e.g. Gemini free tier first, then local Gemma) instead of waiting. | Should |
| LLM-11 | Record input and output token counts per video. Show token totals per batch, and an estimated cost for paid providers using a per-million-token price I enter in Settings (no hardcoded prices). | Should |

**Acceptance:** with a Gemini free-tier key added, switching the provider to
Gemini and re-cleaning a video produces a note with `cleaned_by: gemini/<model>`
without re-downloading or re-transcribing. Pulling the key out of the keyring
shows `waiting for LLM` with "invalid API key" as the reason, not a crash.

**Acceptance:** for a typical YouTube video with a 20-second intro, a mid-roll
sponsor and an outro, the note starts at the first real sentence of content,
contains no sponsor text, ends at the last real sentence, and no sentence in it
is absent from (or a translation of something absent from) the raw transcript.

### 4.4 Storage (Postgres)

| ID | Requirement | Priority |
| --- | --- | --- |
| DB-1 | Uses the local Postgres server. The connection string is in Settings, defaulting to a `shruti` database on `localhost:5432`. Schema is created and migrated automatically on startup. | Must |
| DB-2 | Every video is stored once, keyed by platform plus the platform's own video ID. That key is the dedupe key: a video already done is skipped on re-runs. | Must |
| DB-3 | Store per video: the folder(s) its note was written to, URL, title, channel or profile, upload date, duration, detected language, transcript source (captions or Whisper), raw text, raw segments with timestamps, cleaned text, provider and model used, token counts, and when each stage happened. | Must |
| DB-4 | Store per job: current state, attempt count, last error message, and timestamps. | Must |
| DB-5 | Raw transcripts are never overwritten or deleted by the app. Re-cleaning only replaces the cleaned text, keeping the previous cleaned version. | Must |
| DB-6 | The schema is simple and documented so I can query it directly with `psql` (e.g. "give me the raw transcript for video X"). | Must |
| DB-7 | "Copy raw transcript" and "Export raw transcript" actions in the transcript view. | Should |

### 4.5 Output (Obsidian notes)

| ID | Requirement | Priority |
| --- | --- | --- |
| OUT-1 | One `.md` note per video, written into the folder the auto-sorter picks (see 4.6 Folders). | Must |
| OUT-2 | Notes go straight into that folder, with no per-channel subfolders. The channel stays in the frontmatter, so Obsidian search and Dataview can still group by it. | Must |
| OUT-3 | File name: `<YYYY-MM-DD> <Title>.md`, with characters that aren't safe in file names removed, and truncated to a sensible length. Reels with no title use the first few words of the cleaned text. | Must |
| OUT-4 | The note body is the cleaned English text as plain paragraphs. Nothing else in the body. | Must |
| OUT-5 | YAML frontmatter as in §5. | Must |
| OUT-6 | Writing a note never overwrites a file I've edited. If a note exists and its content differs from what Shruti last wrote, write `… (2).md` and flag it instead. | Must |
| OUT-7 | "Reveal in file manager" and "Open in Obsidian" actions per job. | Should |

### 4.6 Folders (auto-sorted)

I never create or pick folders. Shruti reads what each video is about and files
it into a topic folder, creating the folder if no existing one fits. Folders are
by topic, not by channel, because one channel can post about several things (a
UX channel's occasional vlog goes to a different folder from its UX videos).

```
Obsidian Vault/
└── Shruti/
    ├── Inbox/                  ← only when the topic can't be determined
    ├── Design/
    │   ├── UX/
    │   └── Typography/
    ├── Engineering/
    │   └── Backend/
    └── Travel/
        └── Vlogs/
```

| ID | Requirement | Priority |
| --- | --- | --- |
| FO-1 | All notes live under one root folder in the vault. Default: `~/Documents/Obsidian Vault/Shruti/`, created on first run. The location is set in Settings. | Must |
| FO-2 | **Auto-sort every video.** After cleanup, the LLM gets the cleaned text (or its opening part) plus the current folder tree, and picks where the note belongs. There is no destination picker and no manual folder creation in the app. | Must |
| FO-3 | **Reuse before create.** The sorter must pick an existing folder whenever one reasonably fits. It creates a new folder only when nothing does. | Must |
| FO-4 | **No near-duplicate folders.** Before a new folder is created, its name is checked against existing ones, ignoring case, plurals, punctuation and close synonyms ("UX" / "UX Design" / "User Experience"). A near match is reused instead. | Must |
| FO-5 | **Fixed shape:** exactly two levels, `Area/Topic` (e.g. `Design/UX`, `Engineering/Backend`). Names are in English, Title Case, 1–3 words, no dates, channel names or emoji. This keeps the tree browsable after hundreds of videos. | Must |
| FO-6 | When the sorter creates a folder, it also writes a one-line description of what belongs there. Later sorting uses the descriptions, which keeps decisions consistent over time. The description is stored in a hidden `.shruti` file inside the folder, so it survives renaming the folder in Obsidian. | Must |
| FO-7 | `Shruti/Inbox/` is only for videos whose topic can't be determined (too short, music only, empty transcript). It always exists and is recreated if deleted. | Must |
| FO-8 | **The disk is the source of truth.** Folders I rename, merge, move or delete in Obsidian are reflected in the app automatically, and the sorter works from the current tree. If I merge `UX` into `Design/UX`, new UX videos go to the merged folder. | Must |
| FO-9 | The app finds its notes by the `shruti_id` in their frontmatter, not by a remembered path. **A note I've moved in Obsidian is never moved back.** It's still recognised for duplicate checks, Re-clean and "reveal". | Must |
| FO-10 | Videos already transcribed are skipped on re-runs, and the row shows which folder the note is in. | Must |
| FO-11 | Each row shows where the note was filed, with a "new folder" badge when the sorter created that folder. The batch-finished notification lists any new folders created. | Must |
| FO-12 | A read-only folder tree sidebar with note counts. Clicking a folder filters the list to it. | Should |
| FO-13 | **Re-sort:** rerun the sorter on one note or a whole folder, e.g. after reorganising folders in Obsidian. It only touches notes I haven't moved myself (FO-9). | Should |
| FO-14 | Use the notes I move in Obsidian as examples for future sorting ("this kind of video goes here"). | Later |

**Acceptance:** pasting a UX channel that includes one travel vlog, into an
empty `Shruti/`, creates `Design/UX/` and `Travel/Vlogs/` (or similar) with
descriptions, and files each video accordingly. A second UX channel pasted
afterwards reuses `Design/UX/` and creates no `UX Design` or `User Experience`
folder. Renaming `Design/UX` to `Design/UX Research` in Obsidian keeps its
description, and the next UX video goes there.

### 4.7 Queue and reliability

| ID | Requirement | Priority |
| --- | --- | --- |
| Q-1 | Job states: `queued → fetching metadata → downloading → transcribing → cleaning → filing → writing → done`, plus `waiting for LLM`, `failed` (with reason) and `done (review)`. | Must |
| Q-2 | Stages run in parallel across jobs with separate limits: downloads 2–3 at once, Whisper exactly 1, LLM exactly 1. Video N+1 downloads while video N transcribes and video N−1 is being cleaned. | Must |
| Q-3 | One failure never stops the queue. The job is marked failed with a readable reason, and the queue moves on. | Must |
| Q-4 | Retry a single failed job, or "retry all failed". A retry resumes from the failed stage, not from scratch. | Must |
| Q-5 | The queue survives closing the app or a crash. On next launch, unfinished jobs resume. | Must |
| Q-6 | Pause and resume the whole queue. | Must |
| Q-7 | Cancel a job, or every job from one channel or profile. | Should |
| Q-8 | Desktop notification when a batch finishes, with counts of done, review and failed. | Should |
| Q-9 | Every yt-dlp, Whisper and LLM error is written to a log file I can open from Settings. | Must |

### 4.8 Instagram

| ID | Requirement | Priority |
| --- | --- | --- |
| IG-1 | Single reels and whole profiles (all reels on the profile). | Must |
| IG-2 | Use browser cookies for logged-in access. The browser to read them from (Firefox, Chrome…) is set in Settings. | Must |
| IG-3 | Profiles are processed in batches with a configurable delay between requests (default: a few seconds between reels, a longer pause every N reels). | Must |
| IG-4 | Instagram download concurrency is capped lower than YouTube's (default 1). | Must |
| IG-5 | If Instagram starts rate-limiting or blocking (login walls, 429s), pause the remaining Instagram jobs automatically and say why, instead of failing them all. YouTube jobs carry on. | Must |

### 4.9 yt-dlp freshness

| ID | Requirement | Priority |
| --- | --- | --- |
| YT-1 | yt-dlp is bundled with the app. | Must |
| YT-2 | Settings shows the yt-dlp version in use and its age. | Must |
| YT-3 | An "Update yt-dlp" button downloads the latest release into the app's data folder. That copy is preferred over the bundled one from then on. | Must |
| YT-4 | When a run of failures looks like an extractor break (the same extractor error across several videos), highlight the Update yt-dlp button. | Should |

### 4.10 Interface

| ID | Requirement | Priority |
| --- | --- | --- |
| UI-1 | **Main view:** link box at the top, read-only folder tree on the left (FO-12), job queue table below with title, source, filed-to folder, state (dot plus label, never colour alone), progress, and row actions (retry, re-clean, reveal, open). | Must |
| UI-2 | The queue table stays smooth with thousands of rows (virtualised). | Must |
| UI-3 | Filter and sort the queue by state, platform and channel. `/` focuses the filter. | Should |
| UI-4 | **Transcript view:** cleaned note, with a toggle to see the raw transcript side by side. For Hindi, the raw view shows the original Hindi. | Must |
| UI-5 | **Settings:** Shruti root folder, Postgres connection, LLM provider, model, API key, fallback and status, cleanup prompt and guardrail thresholds, Whisper model and GPU status, Instagram cookie browser and delays, yt-dlp version and update button, log file. `Ctrl+,` opens it. | Must |
| UI-6 | Follows the system light or dark theme. Dense, desktop-style sizing (13–14px base). | Must |
| UI-7 | No first-run wizard. On first launch, create `Shruti/` and `Shruti/Inbox/` in the default vault and start the Whisper model download. Settings shows one status line per dependency (Postgres, LLM, GPU, Whisper model): ok, or the raw error. | Must |
| UI-8 | **No explanatory copy anywhere:** no descriptions under buttons, settings or headers, no tooltips, no empty-state text or illustrations, no confirmation prose. Labels are one or two words, or just an icon where it's obvious. Errors show the raw reason. | Must |

---

## 5. Note format

```markdown
---
title: "How Index Funds Actually Work"
url: https://www.youtube.com/watch?v=abc123
platform: youtube
channel: "Some Channel"
upload_date: 2026-03-14
duration: 612
language: hi            # language spoken in the video
translated: true        # true when language != en
source: captions        # captions | whisper
cleaned_by: lmstudio/gemma-4-31b   # provider/model
cleanup: ok             # ok | review
transcribed: 2026-10-11
shruti_id: youtube:abc123
---

First paragraph of actual content, in plain English, starting at the first
real sentence the speaker says…

Second paragraph…
```

---

## 6. Non-functional requirements

| Area | Requirement |
| --- | --- |
| Speed: captions path | Captioned YouTube video to stored raw transcript in under 5 s. |
| Speed: Whisper path | 60-second reel transcribed in under 3 s on the 4060 (Vulkan). |
| Speed: cleanup | Local Gemma 4 31B: expect 1–3 min per video, since it mostly runs on CPU (it doesn't fit in 8 GB of VRAM). Cloud models: a few seconds per video, limited mainly by rate limits. Either way, cleanup has its own queue and never blocks downloads or Whisper. |
| Privacy | Local by default: Whisper, Postgres and (by default) the LLM all run on this machine. Transcript text leaves the machine only when a cloud LLM provider is selected (LLM-9). Audio never does. |
| Secrets | API keys live only in the system keyring (LLM-5) and are redacted from logs. |
| Resources | Whisper and the LLM must not fight over the GPU. Whisper keeps at least ~2 GB of VRAM. Total RAM use stays well inside 30 GB while Gemma is loaded (~20 GB). |
| Reliability | Restart-safe at every stage (Q-5). No stage is ever redone once it has finished. |
| Recoverability | Raw transcripts are permanent (DB-5). Any note can be regenerated from the database. |
| Debuggability | Every external tool failure is logged with the command and its output. |

---

## 7. Non-goals for v1

These are out of scope for v1. Each could be added later without redesigning
anything.

- **Searching transcripts inside the app.** Obsidian's search covers this.
- **Any manual folder management inside the app:** creating, picking, renaming,
  moving or deleting folders, or moving notes. The auto-sorter creates folders,
  and anything manual happens in Obsidian, which the app follows (FO-8, FO-9).
- **Watching channels** to auto-transcribe new uploads. A manual re-run already
  only processes new videos (DB-2).
- **Local audio or video files** from disk.
- **AI summaries or key points.** Cleanup is deletion only, not condensing.
- **Timestamps in notes.** They are stored in the database (TR-6) if ever wanted.
- **Languages other than English and Hindi**, and output in any language other
  than English.
- **SponsorBlock.** Decided against: the LLM handles all fluff removal.
- **Speaker labels** (who said what).
- **Other platforms** (TikTok, X, podcasts).
- **Multiple users, cross-platform builds, auto-updater, public release.**

---

## 8. Risks and open questions

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Gemma 4 31B is slow on this laptop. | A 300-reel profile could take 5–15 hours of cleanup locally. | Switch to a cloud provider for big batches (seconds per video). Cleanup has its own queue either way, and Re-clean (CL-9) can redo notes with a different model later. |
| The sorter creates too many folders, or near-duplicates. | The tree turns into a mess, which is exactly what we're avoiding. | Reuse-before-create (FO-3), near-duplicate check (FO-4), fixed two-level shape and naming rules (FO-5), folder descriptions for consistency (FO-6), and new folders surfaced in every batch (FO-11). Merging folders in Obsidian is respected (FO-8). |
| The sorter misfiles a video. | A note lands in the wrong folder. | Dragging it in Obsidian fixes it for good, since the app never moves it back (FO-9). Re-sort (FO-13) cleans up after a reorganisation. |
| Sorting adds an LLM call per video. | Slightly slower, and uses more of a free-tier quota. | It's a short call: the folder tree, descriptions and the start of the cleaned text. |
| A free-tier quota runs out mid-batch. | Cleanup stalls partway through a big channel. | Jobs wait instead of failing (LLM-6), an RPM cap keeps requests under the limit (LLM-7), and a fallback provider takes over (LLM-10). |
| Cloud providers rename or retire models. | The saved model stops working. | Model list comes from the provider (LLM-3). An unknown model shows up as a clear `waiting for LLM` reason, and the Test button confirms the fix. |
| Transcripts sent to a cloud provider. | Privacy. Free tiers may train on inputs. | Local is the default and cloud is opt-in. Only text is sent, never audio (LLM-9). |
| The LLM cuts real content or invents some. | Wrong or missing information in notes. | Strict deletion-only prompt (CL-4), length guardrail (CL-6), raw text always kept (DB-5), raw/cleaned side-by-side view (UI-4). |
| LM Studio and Whisper both want the 4060. | Out-of-memory errors or slow Whisper. | **Resolved in step 2b:** LM Studio's GPU offload crashes under the NVK driver anyway, so Gemma runs CPU-only (`--gpu off`, ~18.5 GB RAM) and the 4060 is Whisper's alone. The cost is ~1.7 tokens/s, so a cloud provider is the fast path for big batches. |
| yt-dlp breaks when YouTube or Instagram change. | Every job fails. | Update button (YT-3) plus failure-pattern hint (YT-4). |
| Instagram flags or blocks the account. | Profiles stop working. | Cookies from a secondary account, low concurrency, delays, auto-pause on rate limits (IG-3 to IG-5). |
| Hindi auto-captions on YouTube are poor quality. | Bad source text feeds the translation. | **Resolved in step 1:** on a real Hindi video, YouTube's `hi-orig` captions were accurate and punctuated, while Whisper dropped phrases and misspelled words. Hindi YouTube uses captions. Whisper remains the fallback. |
| yt-dlp's Instagram profile extractor is broken (`instagram:user`, "CURRENTLY BROKEN" as of 2026.08.19). | IG-1 whole profiles can't be listed through yt-dlp. | **Open:** Instagram has also closed its REST listing endpoints to the web app (measured 2026-10-12); only its private GraphQL queries still list a profile. Single reels work through yt-dlp with cookies. Profile links go through yt-dlp's extractor, so they start working when an update fixes it. |
| Scraping Instagram is against its terms. | Account risk only, since this is a personal tool. | Personal use, public content, never distributed. |

---

## 9. Definition of done for v1

All **Must** requirements pass their acceptance criteria. In particular:

1. A mixed paste of an English YouTube channel, a Hindi YouTube video, and an
   Instagram profile produces English, fluff-free notes, auto-sorted into
   sensible `Area/Topic` folders with no near-duplicate folder names.
2. Quitting the app mid-batch and reopening it resumes without redoing anything.
3. Re-running the same channel processes only new videos.
4. The raw transcript for any video can be pulled out with one `psql` query.
