-- Whether the file stage created the job's folder, for the "new folder"
-- badge and the batch-finished notification (PDA FO-11).
ALTER TABLE jobs ADD COLUMN folder_new boolean NOT NULL DEFAULT false;
