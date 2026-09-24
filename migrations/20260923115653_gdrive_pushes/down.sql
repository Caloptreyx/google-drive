DROP INDEX IF EXISTS "gdrive_pushes_status_idx";

ALTER TABLE "gdrive_pushes"
    DROP CONSTRAINT IF EXISTS "gdrive_pushes_user_uuid_users_uuid_fk";

ALTER TABLE "gdrive_pushes"
    DROP CONSTRAINT IF EXISTS "gdrive_pushes_backup_uuid_server_backups_uuid_fk";

DROP TABLE IF EXISTS "gdrive_pushes";

DROP TYPE IF EXISTS "gdrive_push_status";
