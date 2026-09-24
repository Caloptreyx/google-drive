CREATE TABLE "gdrive_connections" (
    "user_uuid" uuid PRIMARY KEY,
    "account_email" varchar(255) NOT NULL,
    "folder_id" varchar(255) NOT NULL,
    "folder_name" varchar(255) NOT NULL,
    "refresh_token" text NOT NULL,
    "scope" text NOT NULL,
    "connected_at" timestamptz NOT NULL DEFAULT now(),
    "last_used_at" timestamptz
);

ALTER TABLE "gdrive_connections"
    ADD CONSTRAINT "gdrive_connections_user_uuid_users_uuid_fk"
    FOREIGN KEY ("user_uuid") REFERENCES "users"("uuid")
    ON DELETE CASCADE ON UPDATE no action;
