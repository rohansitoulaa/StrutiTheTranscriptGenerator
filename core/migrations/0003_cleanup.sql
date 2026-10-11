-- LLM cleanup and folder sorting (PDA §4.3, §4.6).

-- Cleaned text. Every run is kept: re-cleaning adds a row and the newest
-- one is used (DB-5).
CREATE TABLE cleanings (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    video_key    text NOT NULL REFERENCES transcripts (video_key),
    model        text NOT NULL,
    cleaned_text text NOT NULL,
    translated   boolean NOT NULL,
    review       boolean NOT NULL,
    raw_words    integer NOT NULL CHECK (raw_words >= 0),
    clean_words  integer NOT NULL CHECK (clean_words >= 0),
    tokens_in    bigint NOT NULL DEFAULT 0 CHECK (tokens_in >= 0),
    tokens_out   bigint NOT NULL DEFAULT 0 CHECK (tokens_out >= 0),
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX cleanings_latest ON cleanings (video_key, id DESC);

-- Notes written into the vault, with a fingerprint of exactly what Shruti
-- wrote, so a re-clean rewrites a note only if nobody has edited it since
-- (OUT-6, FO-10).
CREATE TABLE notes (
    video_key      text NOT NULL REFERENCES videos (key),
    path           text NOT NULL,
    content_sha256 text NOT NULL,
    written_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (video_key, path)
);

-- When a job waiting on the LLM (down, rate limited, out of quota) may
-- run again (CL-8, LLM-6).
ALTER TABLE jobs ADD COLUMN retry_at timestamptz;
-- Where the file stage placed the note: `Area/Topic` or `Inbox`.
ALTER TABLE jobs ADD COLUMN folder text;

CREATE INDEX jobs_waiting_due ON jobs (retry_at) WHERE status = 'waiting';
