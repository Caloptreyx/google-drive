ALTER TABLE "gdrive_connections"
    DROP CONSTRAINT IF EXISTS "gdrive_connections_user_uuid_users_uuid_fk";

DROP TABLE IF EXISTS "gdrive_connections";
