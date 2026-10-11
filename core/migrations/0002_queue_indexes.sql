-- Indexes for queue lookups that would otherwise scan every finished job.
-- (jobs has no foreign key to videos on purpose: a job exists before its
-- video row, which the metadata stage creates.)

-- "Already done?" on every pasted link, and video history.
CREATE INDEX jobs_video_key ON jobs (video_key);

-- Startup recovery and "retry all failed" without scanning finished jobs.
CREATE INDEX jobs_unfinished ON jobs (status) WHERE status IN ('running', 'waiting', 'failed');
