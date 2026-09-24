import { z } from 'zod';

/**
 * Schemas are declared in camelCase: `parseFromApi` walks the schema shape and remaps
 * the backend's snake_case keys onto it before validating, so writing them the way the
 * API actually sends them would make every field fail to match.
 */

/** Mirrors `PushStatus` in `src/models.rs` - and the `gdrive_push_status` enum values. */
export const pushStatusSchema = z.enum(['pending', 'running', 'completed', 'failed']);
export type PushStatus = z.infer<typeof pushStatusSchema>;

export const gdriveConnectionSchema = z.object({
  accountEmail: z.string(),
  folderName: z.string(),
  folderId: z.string(),
  scope: z.string(),
  connectedAt: z.coerce.date(),
  lastUsedAt: z.coerce.date().nullable(),
  /**
   * Google refused the stored refresh token, so uploads are paused until this user
   * reconnects. Drives the reconnect banner - and the reason a queued push that simply
   * sits at `pending` is not a mystery.
   */
  needsReauth: z.boolean(),
});
export type GDriveConnection = z.infer<typeof gdriveConnectionSchema>;

export const gdrivePushSchema = z.object({
  backupUuid: z.string(),
  status: pushStatusSchema,
  driveFileId: z.string().nullable(),
  attempts: z.number(),
  bytesSent: z.number(),
  totalBytes: z.number(),
  lastError: z.string().nullable(),
  requestedAt: z.coerce.date(),
  updatedAt: z.coerce.date(),
  backupName: z.string().nullable(),
  serverUuid: z.string().nullable(),
});
export type GDrivePush = z.infer<typeof gdrivePushSchema>;

/**
 * The poll that decides whether the page is linked and configured. The push history
 * deliberately lives at `/pushes` instead - this call runs while an upload is in flight,
 * and a list that grows forever is not something to re-fetch every two seconds.
 */
export const gdriveStatusSchema = z.object({
  configured: z.boolean(),
  connection: gdriveConnectionSchema.nullable(),
});
export type GDriveStatus = z.infer<typeof gdriveStatusSchema>;

/**
 * `/stats`: panel-side totals plus Google's storage quota. Fetched on mount and manual
 * refresh rather than polled - the quota half is a live `about.get` round trip, and
 * `quota: null` means "Google wouldn't say", which the badge renders as unavailable
 * without failing the panel-side numbers next to it.
 */
export const gdriveStatsSchema = z.object({
  completedPushes: z.number(),
  bytes: z.number(),
  quota: z
    .object({
      /** `null` for unlimited (Workspace) accounts - usage is shown on its own. */
      limit: z.number().nullable(),
      usage: z.number().nullable(),
      usageInDrive: z.number().nullable(),
    })
    .nullable(),
});
export type GDriveStats = z.infer<typeof gdriveStatsSchema>;

export const gdriveBackupSchema = z.object({
  backupUuid: z.string(),
  serverUuid: z.string(),
  serverName: z.string(),
  backupName: z.string(),
  bytes: z.number(),
  files: z.number(),
  created: z.coerce.date(),
  /** The viewer's own push status for this backup, or null when they've never queued it. */
  pushStatus: pushStatusSchema.nullable(),
});
export type GDriveBackup = z.infer<typeof gdriveBackupSchema>;

/** One file already sitting in the linked Drive folder, as `/files` flattens it. */
export const gdriveFileSchema = z.object({
  fileId: z.string(),
  name: z.string(),
  bytes: z.number(),
  mimeType: z.string(),
  /**
   * True for folders, which drill the table instead of downloading, and which never
   * offer a delete: trashing one would take every tracked file inside it.
   */
  isFolder: z.boolean(),
  createdTime: z.coerce.date().nullable(),
  modifiedTime: z.coerce.date().nullable(),
  /** Google's own viewer URL, opened in a new tab rather than proxied through the panel. */
  viewUrl: z.string(),
  /**
   * The server and backup the extension stamped on the file when it pushed it. Both
   * are null for folders and hand-dropped files - which is exactly when the row gets
   * no Restore button.
   */
  serverUuid: z.string().nullable(),
  backupUuid: z.string().nullable(),
});
export type GDriveFile = z.infer<typeof gdriveFileSchema>;

/** Drive hands back pages, not the whole folder, so this carries the continuation token. */
export const gdriveFilesSchema = z.object({
  files: z.array(gdriveFileSchema),
  nextPageToken: z.string().nullable(),
});
export type GDriveFiles = z.infer<typeof gdriveFilesSchema>;

/** The OAuth redirect the panel hands Google; shown to the operator so they can paste it. */
export const REDIRECT_URI_PATH = '/api/auth/gdrive/callback';

export const gdriveConnectSchema = z.object({ url: z.string() });
export const gdriveQueuedSchema = z.object({ queued: z.boolean() });
export const gdriveUnlinkedSchema = z.object({});
export const gdriveCancelledSchema = z.object({ cancelled: z.boolean() });
export const gdriveDeletedFileSchema = z.object({ deleted: z.boolean() });

/** Admin configuration form - the shape `serializeForApi` turns into the PUT body. */
export const gdriveConfigSchema = z.object({
  clientId: z.string().min(1).max(512),
  /** Empty means "keep whatever is stored", which is what the form sends by default. */
  clientSecret: z.string().max(512),
  folderName: z.string().min(1).max(255),
  /** Whether finished backups are queued the moment they complete, panel-wide. */
  autoPush: z.boolean(),
  /** Whether the Drive copy is trashed when the backup it came from is deleted. */
  deleteCopy: z.boolean(),
  /** Newest copies per server to keep in Drive; 0 keeps everything. */
  keepCopies: z.number().min(0),
  /** Server-name globs opting servers in to auto-push; empty means all. */
  pushInclude: z.string().max(4000),
  /** Server-name globs opting servers out; wins over include. */
  pushExclude: z.string().max(4000),
  /** Whether database-instance backups are pushed and listed like file backups. */
  pushDatabaseBackups: z.boolean(),
  /** Whether each server's backups land in a subfolder named after it. */
  folderPerServer: z.boolean(),
  /** Folder id inside a shared drive to nest the backup folder under; empty = My Drive. */
  sharedFolderId: z.string().max(255),
});
export type GDriveConfig = z.infer<typeof gdriveConfigSchema>;

/** What GET hands back: the secret is never echoed, only whether one is stored. */
export const gdriveAdminSettingsSchema = z.object({
  clientId: z.string(),
  clientSecretSet: z.boolean(),
  folderName: z.string(),
  autoPush: z.boolean(),
  deleteCopy: z.boolean(),
  keepCopies: z.number(),
  pushInclude: z.string(),
  pushExclude: z.string(),
  pushDatabaseBackups: z.boolean(),
  folderPerServer: z.boolean(),
  sharedFolderId: z.string(),
});
export type GDriveAdminSettings = z.infer<typeof gdriveAdminSettingsSchema>;

/** The credential probe's diagnosis of the *stored* client id/secret/redirect URI. */
export const gdriveProbeSchema = z.object({
  valid: z.boolean(),
  detail: z.string(),
  redirectUri: z.string(),
});
export type GDriveProbe = z.infer<typeof gdriveProbeSchema>;

export const gdriveSavedSchema = z.object({ configured: z.boolean() });

/** Restore accepted: the node now owns the operation and the server status flips. */
export const gdriveRestoredSchema = z.object({ restored: z.boolean() });

/**
 * Query keys. Kept local rather than added to core's `queryKeys`, which the panel owns.
 */
export const GDRIVE_STATUS_KEY = ['gdrive', 'status'];
export const GDRIVE_STATS_KEY = ['gdrive', 'stats'];
export const GDRIVE_BACKUPS_KEY = ['gdrive', 'backups'];
export const GDRIVE_PUSHES_KEY = ['gdrive', 'pushes'];
export const GDRIVE_FILES_KEY = ['gdrive', 'files'];
