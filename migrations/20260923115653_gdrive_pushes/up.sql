CREATE TYPE "gdrive_push_status" AS ENUM (
    'pending',
    'running',
    'completed',
    'failed'
);

CREATE TABLE "gdrive_pushes" (
    "backup_uuid" uuid NOT NULL,
    "user_uuid" uuid NOT NULL,
    "drive_file_id" varchar(255),
    "status" "gdrive_push_status" NOT NULL DEFAULT 'pending',
    "attempts" integer NOT NULL DEFAULT 0,
    "bytes_sent" bigint NOT NULL DEFAULT 0,
    "total_bytes" bigint NOT NULL DEFAULT 0,
    "session_uri" text,
    "last_error" text,
    "requested_at" timestamptz NOT NULL DEFAULT now(),
    "updated_at" timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY ("backup_uuid", "user_uuid")
);

ALTER TABLE "gdrive_pushes"
    ADD CONSTRAINT "gdrive_pushes_backup_uuid_server_backups_uuid_fk"
    FOREIGN KEY ("backup_uuid") REFERENCES "server_backups"("uuid")
    ON DELETE CASCADE ON UPDATE no action;

ALTER TABLE "gdrive_pushes"
    ADD CONSTRAINT "gdrive_pushes_user_uuid_users_uuid_fk"
    FOREIGN KEY ("user_uuid") REFERENCES "users"("uuid")
    ON DELETE CASCADE ON UPDATE no action;

CREATE INDEX "gdrive_pushes_status_idx" ON "gdrive_pushes" ("status");
