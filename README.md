# ♻️ Google Drive Backups

A [Calagopus Panel](https://calagopus.com) extension that mirrors server backups into Google Drive — push finished backups from the panel, browse them rclone-style, and restore a server from an off-site copy, all without leaving the panel. Feature inspired by Aternos.

This extension requires a panel version of `>=1.2.2`, and is currently on beta. Do not expect this product to work on the first try.

## ⚒️ Features

- **One account page** (`Account → Google Drive`): connect Google, push backups, browse the backup folder with drill-down like a file manager, and restore — all from the panel.
- **One OAuth client for the whole panel.** The admin configures Google once; every user just clicks *Connect*. The `drive.file` scope means Drive only exposes files this app created — it can never read the rest of anyone's Drive.
- **Manual and automatic pushes**: a Push button on every finished backup, plus an optional listener that queues each backup as it completes. A reconcile sweep picks up backups missed while the panel was down, so auto-push never silently skips one.
- **Resumable, verified uploads**: the archive is pulled from the node's own address and uploaded with a resumable session — an interrupted transfer resumes instead of restarting, and the staged bytes are checked to actually *be* the gzip archive, so a download URL that answers `200` with an error page fails the push with the bytes it got instead of shipping them.
- **Push rules**: include/exclude globs on the server name gate *automatic* pushes — the manual Push button always works. Database backups can be pushed behind one switch.
- **A folder per server**, created on first push and browsable from the Files table.
- **Keep-last-N retention**: after each upload only a server's newest N copies stay in Drive; older ones go to the Drive trash (the freshly uploaded file is never pruned).
- **Cleanup with the backup**: when a backup is deleted from the panel, its Drive copy is trashed too (optional, on by default).
- **Visibility**: live progress and cancel in the Uploads table, a Drive storage quota badge next to panel-side totals, and Google's own error reasons surfaced verbatim, so a failed upload is actually diagnosable.
- **Reconnect banner**: when Google invalidates a stored grant, that account's uploads pause and the page says so; re-linking resumes them automatically.
- **Shared drives**: paste the id of a folder inside a shared drive and uploads nest under it.
- **Update checks**: the panel's Updates tab shows when a newer release is published on GitHub, with its changelog, next to panel and node updates — checked at startup, every 12 hours, and on demand. Releases are tagged `vX.Y.Z`; pre-releases are never offered.
- **Restore from Drive**: the Files and Backups tables carry a Restore button that streams the archive back out of Drive, through the panel, into Wings — single-use 15-minute token, no link-sharing, no public URL, no Wings changes. A Drive-only archive whose panel row was deleted is imported as a backup row first, so it shows up in the backups list like any other.

## ➕ Installation

Download `dev_caloptreyx_gdrive.c7s.zip` from the [latest release](https://github.com/Caloptreyx/google-drive/releases/latest) and either upload it under **Admin → Extensions** or drop it into your heavy image's `build/extensions/` directory and `docker compose restart web`. Extensions require the `:heavy` panel image (or a dev environment) — see the [Calagopus docs](https://calagopus.com/docs/panel/extensions/installing-extensions).

## ⚒️ Configuration

**Admin → Extensions → Google Drive Backups → Configure**

Google Cloud setup (once, by the admin):

1. Create a project at <https://console.cloud.google.com> and enable the **Google Drive API** under **APIs & Services → Library**.
2. Create an **OAuth consent screen** (External, unless the whole team is inside one Workspace domain) and add yourself as a **test user** while it is in *Testing*.
3. Create an **OAuth client ID** (Web application) and add the **Redirect URI** exactly as the Configure page shows it: `{APP_URL}/api/auth/gdrive/callback`.
4. Paste the client ID and secret into Configure and click **Test credentials** — it exchanges a throwaway code against Google and diagnoses a bad client id/secret or a redirect-URI mismatch, naming the exact redirect URI this panel sends.

Then the settings:

- **Backup folder name**, **push backups automatically**, **delete the Drive copy when its backup is deleted**, and **keep the newest N copies per server** (retention).
- **Include / exclude rules** — globs on the server name that gate automatic pushes.
- **Push database backups**, **folder per server**, and an optional **shared-drive folder id**.

Scope used: `openid email https://www.googleapis.com/auth/drive.file` — deliberately narrow: one OAuth client covers every panel user, and `drive.file` still keeps it out of the rest of their Drive.

Permissions: user `gdrive.read|connect|manage`, server `gdrive.push|restore`.

## ❓ Usage

Open **Account → Google Drive** and click **Connect Google Drive**. The page then shows the connection (with a red banner and re-link button when Google rejects the grant), Drive storage against the panel's mirrored upload totals, the folder's files with drill-down and per-file **Restore**/**Delete**, finished server backups with Push / Retry / **Restore**, and the push history with live progress and cancel.

## ⌨️ API

- `GET|PUT /api/admin/dev.caloptreyx.gdrive/` (settings), `POST .../test` (credential probe)
- `GET /api/client/extensions/dev.caloptreyx.gdrive/{status,stats,backups,pushes,files}`,
  `GET .../connect`, `DELETE .../connection`, `POST .../push`,
  `DELETE .../push/{backup_uuid}`, `DELETE .../files/{file_id}`, `POST .../restore`
  (`server_uuid` plus exactly one of `backup_uuid` or `file_id`)
- `GET /api/auth/gdrive/callback` (OAuth return);
  `GET /api/auth/gdrive/archive/{token}/backup.tar.gz` (single-use archive stream for Wings)

Full schemas are in the panel's OpenAPI document once installed.

## 💭 Roadmap

- Restore for database-instance backups — file backups only today; database restores are refused with `412` until they can claim the db-agent's status handshake.
- Enumerating shared drives directly — currently limited to a pasted folder id because `drives.list` sits outside the `drive.file` scope.
- Translations (English only for now).

## 📃 License

This extension is currently licensed under the [MIT](https://github.com/Caloptreyx/google-drive/blob/main/LICENSE) license.

---

Working on the extension itself? Architecture, routes, restore internals and the E2E
harnesses live in [DEVELOPMENT.md](DEVELOPMENT.md).