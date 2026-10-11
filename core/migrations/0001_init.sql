-- Shruti schema v1 (PDA §4.4). Additive migrations only from here on.

-- One row per video, keyed by `platform:id` (DB-2: the dedupe key).
CREATE TABLE videos (
    key           text PRIMARY KEY,
    platform      text NOT NULL CHECK (platform IN ('youtube', 'instagram')),
    video_id      text NOT NULL,
    url           text NOT NULL,
    title         text NOT NULL,
    channel       text,
    upload_date   date,
    duration_secs integer CHECK (duration_secs >= 0),
    language      text,
    -- Caption track codes, so the fetch stage can pick a track after a
    -- restart without asking yt-dlp again.
    subtitles     text[] NOT NULL DEFAULT '{}',
    auto_captions text[] NOT NULL DEFAULT '{}',
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    UNIQUE (platform, video_id)
);

-- The raw transcript, written once and never overwritten or deleted (DB-5).
-- Segments keep timestamps even though notes don't show them (TR-6).
CREATE TABLE transcripts (
    video_key     text PRIMARY KEY REFERENCES videos (key),
    source        text NOT NULL CHECK (source IN ('captions', 'whisper')),
    caption_kind  text CHECK (caption_kind IN ('manual', 'auto')),
    language      text,
    raw_text      text NOT NULL,
    segments      jsonb NOT NULL,
    -- Why captions were skipped when Whisper was used despite a track.
    caption_error text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    CHECK ((source = 'captions') = (caption_kind IS NOT NULL))
);

-- One row per requested link. `stage` is the next stage to run, so a
-- restart or a retry resumes there instead of starting over (Q-4, Q-5).
CREATE TABLE jobs (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    batch_id      uuid NOT NULL,
    input         text NOT NULL,
    url           text NOT NULL,
    video_key     text NOT NULL,
    stage         text NOT NULL DEFAULT 'metadata'
                  CHECK (stage IN ('metadata', 'fetch', 'transcribe', 'clean', 'file', 'write', 'done')),
    status        text NOT NULL DEFAULT 'queued'
                  CHECK (status IN ('queued', 'running', 'waiting', 'failed', 'done', 'cancelled')),
    -- Why a job is waiting (LLM down, quota, Instagram pause) or failed.
    reason        text,
    -- Set when captions failed and the job fell back to Whisper; copied to
    -- the transcript once it exists.
    caption_error text,
    attempts      integer NOT NULL DEFAULT 0,
    review        boolean NOT NULL DEFAULT false,
    note_path     text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    finished_at   timestamptz,
    CHECK ((status = 'done') = (stage = 'done'))
);

-- At most one unfinished job per video: re-pasting a link already in the
-- queue doesn't duplicate it, and re-pasting a failed one retries it.
CREATE UNIQUE INDEX jobs_one_open_per_video ON jobs (video_key)
    WHERE status IN ('queued', 'running', 'waiting', 'failed');

-- The dispatcher's "next job" lookup.
CREATE INDEX jobs_queued_order ON jobs (id) WHERE status = 'queued';

CREATE INDEX jobs_batch ON jobs (batch_id);
