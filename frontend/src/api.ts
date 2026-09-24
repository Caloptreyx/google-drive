import { z } from 'zod';
import { axiosInstance } from '@/api/axios.ts';
import { parseFromApi, parsePaginationFromApi, serializeForApi } from '@/lib/serialization/api-transform.ts';
import {
  GDriveAdminSettings,
  GDriveBackup,
  GDriveConfig,
  GDriveProbe,
  GDrivePush,
  gdriveAdminSettingsSchema,
  gdriveBackupSchema,
  gdriveCancelledSchema,
  gdriveConfigSchema,
  gdriveConnectSchema,
  gdriveDeletedFileSchema,
  gdriveFilesSchema,
  gdriveProbeSchema,
  gdrivePushSchema,
  gdriveQueuedSchema,
  gdriveRestoredSchema,
  gdriveSavedSchema,
  gdriveStatsSchema,
  gdriveStatusSchema,
  gdriveUnlinkedSchema,
} from './schemas.ts';

/** Page size for both server-paged tables; core's default and small enough to stay quick. */
const PER_PAGE = 25;

/** Mounted by `routes::client::BASE` on `add_client_api_router`. */
const BASE = '/api/client/extensions/dev.caloptreyx.gdrive';

/** Mounted by `routes::admin::BASE` on `add_admin_api_router`. */
const ADMIN = '/api/admin/dev.caloptreyx.gdrive';

const pushRequestSchema = z.object({
  serverUuid: z.string(),
  backupUuid: z.string(),
});

const restoreRequestSchema = z.object({
  serverUuid: z.string(),
  /** Exactly one of these two: which entry point the click came from. */
  backupUuid: z.string().optional(),
  fileId: z.string().optional(),
});

export async function getStatus() {
  const { data } = await axiosInstance.get(`${BASE}/status`);

  return parseFromApi(gdriveStatusSchema, data);
}

/**
 * Mirrored totals plus Google's storage quota. Separate from `getStatus` on purpose:
 * status runs on the two-second poll while an upload is in flight, and this call may
 * reach out to `about.get` to answer.
 */
export async function getStats() {
  const { data } = await axiosInstance.get(`${BASE}/stats`);

  return parseFromApi(gdriveStatsSchema, data);
}

/**
 * Returns the Google consent URL instead of following it, so a failure (credentials not
 * configured, missing permission) surfaces as a toast rather than a dead redirect.
 */
export async function connect() {
  const { data } = await axiosInstance.get(`${BASE}/connect`);

  return parseFromApi(gdriveConnectSchema, data);
}

export async function unlink() {
  const { data } = await axiosInstance.delete(`${BASE}/connection`);

  return parseFromApi(gdriveUnlinkedSchema, data);
}

/**
 * One page of the finished backups this user can push. Signature matches the
 * `useSearchablePaginatedTable` fetcher: `(page, search)`.
 */
export async function listBackups(page: number, search: string): Promise<Pagination<GDriveBackup>> {
  const { data } = await axiosInstance.get(`${BASE}/backups`, {
    params: { page, search, per_page: PER_PAGE },
  });

  return parsePaginationFromApi(gdriveBackupSchema, data.backups);
}

/** One page of this user's push history, for the Uploads table. */
export async function listPushes(page: number, search: string): Promise<Pagination<GDrivePush>> {
  const { data } = await axiosInstance.get(`${BASE}/pushes`, {
    params: { page, search, per_page: PER_PAGE },
  });

  return parsePaginationFromApi(gdrivePushSchema, data.pushes);
}

/**
 * One page of the linked Drive folder; `pageToken` comes from the previous response.
 * `folderId` drills into a subfolder - absent means the account's backup root, and the
 * backend re-verifies any id given really is inside *this* user's tree.
 */
export async function listFiles(pageToken?: string, folderId?: string | null) {
  const { data } = await axiosInstance.get(`${BASE}/files`, {
    params: {
      ...(pageToken ? { page_token: pageToken } : {}),
      ...(folderId ? { folder_id: folderId } : {}),
    },
  });

  return parseFromApi(gdriveFilesSchema, data);
}

/** Pull a push out of the queue, or stop one that is already streaming. */
export async function cancelPush(backupUuid: string) {
  const { data } = await axiosInstance.delete(`${BASE}/push/${encodeURIComponent(backupUuid)}`);

  return parseFromApi(gdriveCancelledSchema, data);
}

/**
 * Trash one file in the linked folder. The backend re-checks the file really is in
 * this user's folder before it acts - `drive.file` only narrows visibility to files the
 * app created, and one OAuth client covers every linked panel user.
 */
export async function deleteDriveFile(fileId: string) {
  const { data } = await axiosInstance.delete(`${BASE}/files/${encodeURIComponent(fileId)}`);

  return parseFromApi(gdriveDeletedFileSchema, data);
}

export async function pushBackup(serverUuid: string, backupUuid: string) {
  const { data } = await axiosInstance.post(
    `${BASE}/push`,
    serializeForApi(pushRequestSchema, { serverUuid, backupUuid }),
  );

  return parseFromApi(gdriveQueuedSchema, data);
}

/**
 * Start a restore from Drive: the backend re-verifies everything (server ownership,
 * the file's identity tags, the backup's gates) and only then claims the server's
 * status and tells the node to fetch the archive. This call returns once the node has
 * *accepted* - the server's status then runs through "restoring" in core's own UI.
 */
export async function restoreFromDrive(payload: { serverUuid: string; backupUuid?: string; fileId?: string }) {
  const { data } = await axiosInstance.post(`${BASE}/restore`, serializeForApi(restoreRequestSchema, payload));

  return parseFromApi(gdriveRestoredSchema, data);
}

export async function getAdminSettings(): Promise<GDriveAdminSettings> {
  const { data } = await axiosInstance.get(ADMIN);

  return parseFromApi(gdriveAdminSettingsSchema, data);
}

export async function saveAdminSettings(config: GDriveConfig) {
  const { data } = await axiosInstance.put(ADMIN, serializeForApi(gdriveConfigSchema, config));

  return parseFromApi(gdriveSavedSchema, data);
}

/**
 * Ask Google about the *stored* credentials. No account is involved and no consent is
 * touched - the backend exchanges a code that can never be valid and classifies which
 * error comes back, so this answers "would a real link work" before anyone tries one.
 */
export async function testCredentials(): Promise<GDriveProbe> {
  const { data } = await axiosInstance.post(`${ADMIN}/test`);

  return parseFromApi(gdriveProbeSchema, data);
}
