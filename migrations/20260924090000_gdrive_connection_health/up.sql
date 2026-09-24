-- Set when Google rejects the stored refresh token (invalid_grant): the grant is dead
-- and only the user reconnecting can revive it. The worker stops claiming pushes for
-- such connections so no attempts are burned, and the account page shows a reconnect
-- banner. Cleared automatically by a successful refresh or by re-linking.
ALTER TABLE "gdrive_connections" ADD COLUMN IF NOT EXISTS "needs_reauth_at" timestamp with time zone;
