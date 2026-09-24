# Google Drive Backups - development notes

The user-facing project documentation lives in [README.md](README.md); this file is the
technical reference for working on the extension itself.

A Panel extension that mirrors server backups into Google Drive. One account-level page
(`/google-drive`) is where a user links their own Drive, pushes backups, and browses what
already landed there - rclone-style, from the panel.

- Extension-only: the Panel side does all the work, Wings is untouched.
- Admin configures **one** Google OAuth client; every user then just clicks *Connect*.
- Backups are pulled from the node and pushed to Drive with a resumable upload, so an
  interrupted transfer resumes rather than restarting. The download goes straight to the
  node's own address (not the browser-facing public or wings-proxy URL), and staged
  bytes are checked to actually *be* the gzip archive - a download URL that answers
  `200` with an error page fails the push with the bytes it got instead of uploading
  them.
- Optional automatic push when a backup finishes, and optional cleanup of the Drive copy
  when the backup it came from is deleted.
- **Keep-last-N retention**: after each upload only a server's newest copies stay in
  Drive; older ones go to the Drive trash (the freshly uploaded file is never pruned).
- **Push rules**: include/exclude globs on the server name gate *automatic* pushes -
  the Push button always works. Database-instance backups can be pushed alongside file
  backups behind one switch.
- **A folder per server**: each server's uploads land in a subfolder named after it,
  created on first push, browsable from the Files table.
- **Reconcile sweep**: a completed backup that never got queued (extension re-enabled,
  panel was down when it finished) is picked up automatically within the lookback
  window, so auto-push doesn't silently miss backups.
- **Quota badge + mirrored stats**: Drive storage usage (from `about.get`, no scope
  bump needed) next to panel-side upload totals.
- **Shared drives**: paste the id of a folder inside a shared drive and the backup
  root is nested under it (enumerating drives would require a broader OAuth scope).
- **Credential probe**: the Configure page exchanges a throwaway code against Google
  and classifies the answer - a bad client id/secret or a redirect-URI mismatch is
  diagnosed with the exact redirect URI this panel sends, before anyone tries to link.
- **Reconnect banner**: when Google invalidates a stored grant (`invalid_grant`), that
  account's uploads pause and the account page says so, with the button that fixes it.
  Queued uploads resume on their own once the account reconnects.
- **Restore from Drive**: both the Files and Backups tables carry a Restore button that
  streams the archive back out of Drive - panel-only, no Wings changes. See
  [Restoring](#restoring) for how it works and what it does to the server.
- **Update checks**: `check_for_updates` folds the GitHub releases feed (cached for an
  hour, 404 treated as an empty feed) into the panel's Updates tab - tags must be
  `vX.Y.Z`; pre-releases and non-semver tags are skipped rather than failed.

## Compatibility

Requires panel **1.2.2 or newer**. Every panel API this extension uses exists in the
1.2.2 release; the single 1.2.3-only symbol (`shared::models::search_sql`) is
reimplemented locally in `src/models.rs`, because a panel compiles an extension against
*its own* copy of `shared` and would fail the build. Frontend dependency ranges are held
at 1.2.2's minima (`@mantine/core ^9.6.0`, `@tanstack/react-query ^5.102.8`,
`react ^19.2.8`, `zod ^4.5.4`) so a 1.2.2 panel's lockfile resolves them to the copies
core already has, instead of installing a second React Query or Mantine beside it.

## Install

Extensions need the `:heavy`/`:nightly-heavy` image, or a development environment.

1. Build the package:

   ```bash
   cargo fmt && cargo clippy
   cd frontend && pnpm biome:fix-unsafe && pnpm build:ci && cd ..
   PATH="$HOME/.cargo/bin:$PATH" SQLX_OFFLINE=true ./target/debug/panel-rs extensions export dev.caloptreyx.gdrive
   ```

2. Install `exported-extensions/dev_caloptreyx_gdrive.c7s.zip` either through
   **Admin → Extensions → Upload**, or by dropping it into the Panel's `extensions/`
   data directory and restarting the Panel.

## Google Cloud setup (once, by the admin)

1. Create a project at <https://console.cloud.google.com>.
2. **APIs & Services → Library** → enable the **Google Drive API**.
3. **APIs & Services → OAuth consent screen**
   - User type: **External** (unless the whole team is inside one Workspace domain).
   - Add yourself as a **test user** while the screen is in *Testing* - otherwise only
     Workspace accounts in your domain can finish the consent flow.
4. **APIs & Services → Credentials → Create credentials → OAuth client ID**
   - Application type: **Web application**.
   - Add the extension's **Redirect URI** exactly as shown on the Configure page:
     `{APP_URL}/api/auth/gdrive/callback`
   - Copy the client ID and client secret.
5. In the Panel: **Admin → Extensions → Google Drive Backups → Configure** → paste the
   client ID and secret, set the backup folder name, choose whether pushes are automatic
   and whether the Drive copy is deleted with the backup, set retention (`keep newest
   copies per server`), the include/exclude push rules, database-backup pushing,
   folder-per-server, and optionally the shared-drive folder id → **Save**.
6. Click **Test credentials** - it talks to Google using the *saved* settings and tells
   you whether a real link would work, including the exact redirect URI to register.

Scope used: `openid email https://www.googleapis.com/auth/drive.file`. The `drive.file`
scope is deliberate - Drive only exposes files this app created, so one OAuth client
covering every panel user still can't read the rest of their Drive.

## Per user

Open **Account → Google Drive** and click **Connect Google Drive**. The page then lists:

- **Connection** - the linked account, folder, and the scopes actually granted. A red
  banner appears here when Google rejected the stored grant, with a Connect button that
  re-links and resumes the paused uploads.
- **Storage & uploads** - Drive storage used (with a bar against the quota, or
  "unavailable" if Google won't say) and panel-side mirrored totals.
- **Files in Drive** - what is already in the folder (live files only; copies that
  retention moved to the Drive trash stay hidden here), paged from Google; folders drill
  into subfolders (with a back button), files are deletable here, and archives the
  extension pushed carry a **Restore** button.
- **Backups** - finished server backups this user can push, searchable and paged, each
  with its Drive status and a Push / Retry / Push again button; once a push has
  completed, also a **Restore** button.
- **Uploads** - push history with live progress, cancel, and errors.

## Restoring

Restore is entirely panel-side: the extension tells Wings to run its normal restore
with `adapter: S3` and a `download_url` pointing back at the panel, and the panel
streams the archive bytes from Drive (with the linking user's OAuth grant) straight
through to Wings. Files stay private - no link-sharing, no public URL.

What a restore does, and what it expects:

- **The server must be stoppable.** Wings stops the container before extracting,
  exactly like core's own restore. A server that is installing or already restoring is
  refused with `417` before anything is touched.
- **The server's status is claimed first** (core's handshake), so two restores cannot
  race. A refused restore rolls the claim back.
- **The archive's identity comes from the file's `appProperties` tags**, not from the
  click: file restores are re-verified (untrashed, not a folder, inside *your* backup
  tree, stamped for *this* server). Restoring someone else's file id 404s.
- **Files are replaced.** `truncate_directory` (off by default, like core) wipes the
  directory before extracting; whatever was there is overwritten either way.
- **A Drive-only archive gets a panel row.** If the backup row was deleted (or never
  existed), the restore registers it first - the same raw insert core's
  `backups s3 import` uses - so the backups list shows it and Wings' completion
  callback finds it. Soft-deleted rows are resurrected instead. The import also books a
  completed push row against the file, so the reconcile sweep knows the archive is
  already in Drive rather than re-queueing a backup nothing can download.
- **Database-instance backups are refused** (`412`): their restore needs the db-agent
  status claims that live on core's database route, which this feature does not take
  on. File backups only.
- **Core's own Restore/Download buttons do not work on Drive rows.** They compute an
  S3-presigned URL from the row's `disk`/configuration and will 412 on a
  configuration-less imported row (or hand Wings a URL that 404s). Use the extension's
  Restore button - the reverse also holds: core's buttons still work normally on
  backups whose local copy exists.
- The download fetches through `{APP_URL}`; Wings reads the archive format from the
  URL's last path segment, which is why the archive URL ends in `/backup.tar.gz`.
  The single-use fetch token expires after 15 minutes and is consumed on first use.
- **Which copy gets restored is Wings' call, and the bytes are identical either way.**
  Wings caches backups for 10 minutes after it touches one (creating it, serving its
  download), and a cache hit short-circuits the request's adapter: the local file on
  the node is extracted and `download_url` is ignored. On a cache miss - or once the
  local file is gone - `adapter: S3` resolves unconditionally and the archive really is
  streamed from Drive. The two are byte-for-byte the same archive (the push path stages
  from the node's own copy), so a hot-cache restore is not a different result, just a
  local one; the Drive copy is what makes the restore possible after node data loss.

## Permissions

| Permission   | Meaning                                                        |
| ------------ | -------------------------------------------------------------- |
| `gdrive.read`   | View the connection, backups, and push status               |
| `gdrive.connect`| Link or unlink the Google account                            |
| `gdrive.manage` | Delete files this extension pushed to Drive                 |
| `gdrive.push` (server) | Push this server's backups (server-scoped, for subusers) |
| `gdrive.restore` (server) | Restore this server's files from Drive (server-scoped) |

Sessions always hold every user permission, which is why deletion is gated separately:
an API key scoped to `gdrive.read` alone must not be able to trash archives. Restore is
its own server-scoped permission for the same reason: it overwrites the server's files,
so a key that may push must not thereby be able to restore.

## API surface

- Auth: `GET /api/auth/gdrive/callback` (OAuth return);
  `GET /archive/{token}/backup.tar.gz` (unauthenticated archive stream for Wings -
  single-use token, consumed on first fetch).
- Client: `add_client_api_router`, base `/extensions/dev.caloptreyx.gdrive` -
  `status` (includes `needs_reauth`), `stats`, `connect`, `connection`, `backups`,
  `pushes`, `push`, `files` (with `folder_id` drill-down, validated against the
  caller's tree), `DELETE /files/{file_id}` (refuses folders, walks the parent chain),
  `DELETE /push/{backup_uuid}`, `POST /restore` (payload: `server_uuid` plus exactly
  one of `backup_uuid` or `file_id`, plus `truncate_directory` / `restore_startup`).
- Admin: `add_admin_api_router`, base `/dev.caloptreyx.gdrive` (GET/PUT settings,
  `POST /test` credential probe).
  Deliberately *not* `/extensions/<pkg>`: that path collides with core's
  `PATCH /api/admin/extensions/{extension}` toggle and would 405 it.

## Known limitations

- Shared drives work through the pasted folder id only; drives are not enumerated
  (`drives.list` is outside the `drive.file` scope, by design).
- Restore covers server (file) backups only - database-instance backups cannot be
  restored from Drive yet, and the panel-side streaming means large archives consume
  panel bandwidth while Wings pulls them.
- *Push again* on a Drive-imported row fails with core's `no backup configuration`
  message: core's download URL requires the row's backup configuration, and an imported
  row has none. The archive is already in Drive, which is the point - use Restore.
- Retention only prunes files carrying this extension's upload tag, so files pushed by
  older versions (before tags existed) are not counted against keep-last-N.
- Translations are English only; other locales fall back to the source strings.