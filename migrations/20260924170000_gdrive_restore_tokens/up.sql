-- Short-lived, single-use capabilities that let Wings fetch one archive through the
-- panel without any session: the restore route mints a row, builds a URL carrying the
-- token, and the archive route deletes the row on first fetch. Expiry is swept
-- opportunistically on every mint, so no background cleanup task is needed.
CREATE TABLE IF NOT EXISTS "gdrive_restore_tokens" (
    "token" uuid PRIMARY KEY,
    "user_uuid" uuid NOT NULL,
    "backup_uuid" uuid NOT NULL,
    "server_uuid" uuid NOT NULL,
    "file_id" text NOT NULL,
    "created" timestamp with time zone NOT NULL DEFAULT now(),
    "expires_at" timestamp with time zone NOT NULL
);

CREATE INDEX IF NOT EXISTS "gdrive_restore_tokens_expires_at"
    ON "gdrive_restore_tokens" ("expires_at");
