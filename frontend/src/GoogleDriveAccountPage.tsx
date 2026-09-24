import {
  faArrowLeft,
  faArrowsRotate,
  faBan,
  faCloudArrowUp,
  faFolder,
  faFolderOpen,
  faHardDrive,
  faLink,
  faRotateLeft,
  faTrash,
} from '@fortawesome/free-solid-svg-icons';
import { FontAwesomeIcon } from '@fortawesome/react-fontawesome';
import { Group, SimpleGrid, Stack } from '@mantine/core';
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useState } from 'react';
import { httpErrorToHuman } from '@/api/axios.ts';
import Button from '@/elements/buttons/Button.tsx';
import AccountContentContainer from '@/elements/containers/AccountContentContainer.tsx';
import Badge from '@/elements/data-display/Badge.tsx';
import Table, { TableData, TableRow } from '@/elements/data-display/Table.tsx';
import TitleCard from '@/elements/data-display/TitleCard.tsx';
import Alert from '@/elements/feedback/Alert.tsx';
import Progress from '@/elements/feedback/Progress.tsx';
import TextInput from '@/elements/input/TextInput.tsx';
import ConfirmationModal from '@/elements/modals/ConfirmationModal.tsx';
import FormattedTimestamp from '@/elements/time/FormattedTimestamp.tsx';
import Anchor from '@/elements/typography/Anchor.tsx';
import { bytesProgressString, bytesToString } from '@/lib/format/size.ts';
import { useSearchablePaginatedTable } from '@/plugins/resource/useSearchablePaginatedTable.ts';
import { useToast } from '@/providers/ToastProvider.tsx';
import { useTranslations } from '@/providers/TranslationProvider.tsx';
import {
  cancelPush,
  connect,
  deleteDriveFile,
  getStats,
  getStatus,
  listBackups,
  listFiles,
  listPushes,
  pushBackup,
  restoreFromDrive,
  unlink,
} from './api.ts';
import {
  GDRIVE_BACKUPS_KEY,
  GDRIVE_FILES_KEY,
  GDRIVE_PUSHES_KEY,
  GDRIVE_STATS_KEY,
  GDRIVE_STATUS_KEY,
  GDriveBackup,
  GDriveFile,
  GDrivePush,
  PushStatus,
} from './schemas.ts';
import { useExtTranslations } from './translations.ts';

/**
 * Keys sit under `pages.account.googleDrive` so an operator's translation files keep the
 * same shape core's do - `tExt` prepends this extension's own namespace on top of it.
 */
const K = 'pages.account.googleDrive';

/** Badge colour per `PushStatus`; the union comes from the schema, not from a guess. */
const STATUS_COLORS: Record<PushStatus, string> = {
  pending: 'gray',
  running: 'blue',
  completed: 'green',
  failed: 'red',
};

/** `Table` only renders its `children` once `pagination.total > 0`, so it needs one. */
function everything<T>(items: T[]): Pagination<T> {
  return { total: items.length, perPage: Math.max(items.length, 1), page: 1, data: items };
}

/**
 * What the restore modal is holding: exactly one of `backupUuid` (Backups table) or
 * `fileId` (Files table), plus the server both were verified against - the backend
 * refuses anything else, this is only which door the click came through.
 */
type PendingRestore =
  | { serverUuid: string; backupUuid: string; fileId?: never }
  | { serverUuid: string; backupUuid?: never; fileId: string };

export default function GoogleDriveAccountPage() {
  const { tItem } = useTranslations();
  const { t: tExt } = useExtTranslations();
  const { addToast } = useToast();
  const queryClient = useQueryClient();

  const [connecting, setConnecting] = useState(false);
  const [confirmUnlink, setConfirmUnlink] = useState(false);
  const [deletingFile, setDeletingFile] = useState<GDriveFile | null>(null);
  /** The restore waiting behind the confirm modal; null keeps it closed. */
  const [restoring, setRestoring] = useState<PendingRestore | null>(null);
  /** Which per-server subfolder the Files table is inside; `null` is the backup root. */
  const [openFolder, setOpenFolder] = useState<{ id: string; name: string } | null>(null);

  const status = useQuery({
    queryKey: GDRIVE_STATUS_KEY,
    queryFn: getStatus,
  });

  // Quota and mirrored totals, fetched on mount and refresh rather than on the status
  // poll: the quota half is a live `about.get` round trip against Google.
  const stats = useQuery({
    queryKey: GDRIVE_STATS_KEY,
    queryFn: getStats,
    enabled: status.data?.connection != null,
  });

  const connection = status.data?.connection;
  const configured = status.data?.configured ?? false;
  const connected = !!connection;

  // Both tables page server-side. `modifyParams` is off for each of them: this page
  // holds three lists, and a single `page`/`search` pair in the URL would be shared
  // between them. Nothing is requested until an account is linked - there is nothing to
  // list before that.
  const {
    data: backupPage,
    loading: backupsLoading,
    error: backupsError,
    search,
    setSearch,
    setPage: setBackupsPage,
    refetch: refetchBackups,
  } = useSearchablePaginatedTable<Pagination<GDriveBackup>>({
    queryKey: GDRIVE_BACKUPS_KEY,
    fetcher: listBackups,
    canRequest: connected,
    modifyParams: false,
  });

  const backups = backupPage?.data ?? [];

  const {
    data: pushPage,
    loading: pushesLoading,
    error: pushesError,
    setPage: setPushesPage,
    refetch: refetchPushes,
  } = useSearchablePaginatedTable<Pagination<GDrivePush>>({
    queryKey: GDRIVE_PUSHES_KEY,
    fetcher: listPushes,
    canRequest: connected,
    modifyParams: false,
    // The uploads list polls itself instead of the status call doing it: a page that
    // still holds an in-flight push refreshes every couple of seconds so the progress
    // bar moves, and the first page that comes back settled stops it on its own.
    refetchInterval: (data) =>
      data?.data.some((push) => push.status === 'pending' || push.status === 'running') ? 2000 : false,
  });

  const pushes = pushPage?.data ?? [];

  // Drive pages the folder rather than handing it over whole: `undefined` starts the
  // walk and each later token comes from the response before it, which is exactly the
  // shape `useInfiniteQuery` wants. Flattened below so the table stays a single list.
  //
  // The open folder joins the key on purpose: switching folders must restart the walk
  // at page one, not append the new folder's rows onto the old folder's tokens.
  const files = useInfiniteQuery({
    queryKey: [...GDRIVE_FILES_KEY, openFolder?.id ?? null],
    queryFn: ({ pageParam }) => listFiles(pageParam, openFolder?.id),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.nextPageToken ?? undefined,
    enabled: connected,
  });
  const driveFiles = files.data?.pages.flatMap((page) => page.files) ?? [];

  const push = useMutation({
    mutationFn: ({ serverUuid, backupUuid }: { serverUuid: string; backupUuid: string }) =>
      pushBackup(serverUuid, backupUuid),
    onSuccess: () => {
      addToast(tExt(`${K}.toast.queued`, {}), 'success');
      queryClient.invalidateQueries({ queryKey: GDRIVE_STATUS_KEY });
      queryClient.invalidateQueries({ queryKey: GDRIVE_BACKUPS_KEY });
      queryClient.invalidateQueries({ queryKey: GDRIVE_PUSHES_KEY });
      queryClient.invalidateQueries({ queryKey: GDRIVE_FILES_KEY });
    },
    onError: (error) => addToast(httpErrorToHuman(error), 'error'),
  });

  const statusText = (value: PushStatus) => tExt(`${K}.status.${value}`, {});

  const cancelUpload = useMutation({
    mutationFn: (backupUuid: string) => cancelPush(backupUuid),
    onSuccess: () => {
      addToast(tExt(`${K}.toast.cancelled`, {}), 'success');
      // The uploads table and the per-backup Drive badge are both driven by the same row.
      queryClient.invalidateQueries({ queryKey: GDRIVE_STATUS_KEY });
      queryClient.invalidateQueries({ queryKey: GDRIVE_BACKUPS_KEY });
      queryClient.invalidateQueries({ queryKey: GDRIVE_PUSHES_KEY });
    },
    onError: (error) => addToast(httpErrorToHuman(error), 'error'),
  });

  // Deliberately a plain then-chain rather than a mutation: `ConfirmationModal` already
  // turns a rejected promise into its own error toast and leaves the modal open, so an
  // `onError` here would show the same failure twice. Same reason for the restore below.
  const doDeleteFile = () => {
    const file = deletingFile;
    if (!file) return;

    return deleteDriveFile(file.fileId).then(() => {
      setDeletingFile(null);
      queryClient.invalidateQueries({ queryKey: GDRIVE_FILES_KEY });
      addToast(tExt(`${K}.files.delete.toast`, {}), 'success');
    });
  };

  // Accepted means "the node took it": from there the server's own status runs through
  // restoring in core's UI, so there is nothing on this page to poll - just the lists
  // (a row may have been imported) and the success line.
  const doRestore = () => {
    const pending = restoring;
    if (!pending) return;

    return restoreFromDrive(pending).then(() => {
      setRestoring(null);
      addToast(tExt(`${K}.toast.restored`, {}), 'success');
      queryClient.invalidateQueries({ queryKey: GDRIVE_BACKUPS_KEY });
      queryClient.invalidateQueries({ queryKey: GDRIVE_PUSHES_KEY });
      queryClient.invalidateQueries({ queryKey: GDRIVE_FILES_KEY });
    });
  };
  const doConnect = async () => {
    setConnecting(true);

    try {
      // Navigate away deliberately: the OAuth dance leaves the panel and comes back.
      window.location.assign((await connect()).url);
    } catch (error) {
      addToast(httpErrorToHuman(error), 'error');
      setConnecting(false);
    }
  };

  const doUnlink = () =>
    unlink().then(() => {
      setConfirmUnlink(false);
      queryClient.invalidateQueries({ queryKey: GDRIVE_STATUS_KEY });
      queryClient.invalidateQueries({ queryKey: GDRIVE_BACKUPS_KEY });
      queryClient.invalidateQueries({ queryKey: GDRIVE_PUSHES_KEY });
      queryClient.invalidateQueries({ queryKey: GDRIVE_FILES_KEY });
      addToast(tExt(`${K}.unlink.toast`, {}), 'success');
    });

  const doRefresh = () => {
    void status.refetch();
    void stats.refetch();
    void files.refetch();
    void refetchBackups();
    void refetchPushes();
  };

  return (
    <AccountContentContainer
      title={tExt(`${K}.title`, {})}
      subtitle={tExt(`${K}.subtitle`, {})}
      contentRight={
        <Button
          variant='default'
          leftSection={<FontAwesomeIcon icon={faArrowsRotate} />}
          loading={status.isFetching}
          onClick={doRefresh}
        >
          {tExt(`${K}.button.refresh`, {})}
        </Button>
      }
    >
      <Stack gap='lg'>
        {!configured && (
          <Alert color='yellow' title={tExt(`${K}.notConfiguredTitle`, {})}>
            {tExt(`${K}.notConfigured`, {})}
          </Alert>
        )}

        {/* Google refused the stored grant: nothing self-heals from this but the user,
          so the banner says exactly that and hands them the button that fixes it.
          Reconnecting is the same OAuth flow as linking - the callback's upsert is what
          clears the flag on the backend. */}
        {connection?.needsReauth && (
          <Alert color='red' title={tExt(`${K}.connection.reauth.title`, {})}>
            <p className='text-sm'>{tExt(`${K}.connection.reauth.content`, {})}</p>
            <Button
              className='mt-3'
              leftSection={<FontAwesomeIcon icon={faLink} />}
              loading={connecting}
              onClick={doConnect}
            >
              {tExt(`${K}.button.connect`, {})}
            </Button>
          </Alert>
        )}

        <TitleCard
          title={tExt(`${K}.connection.title`, {})}
          rightSection={
            connection ? (
              <Group gap='xs'>
                <Badge color={connection.needsReauth ? 'red' : 'green'} variant='light'>
                  {connection.needsReauth
                    ? tExt(`${K}.connection.status.reauth`, {})
                    : tExt(`${K}.connection.status.linked`, {})}
                </Badge>
                <Button
                  color='red'
                  variant='light'
                  leftSection={<FontAwesomeIcon icon={faTrash} />}
                  onClick={() => setConfirmUnlink(true)}
                >
                  {tExt(`${K}.button.unlink`, {})}
                </Button>
              </Group>
            ) : (
              <Button
                leftSection={<FontAwesomeIcon icon={faLink} />}
                disabled={!configured}
                loading={connecting}
                onClick={doConnect}
              >
                {tExt(`${K}.button.connect`, {})}
              </Button>
            )
          }
        >
          <ConfirmationModal
            opened={confirmUnlink}
            onClose={() => setConfirmUnlink(false)}
            title={tExt(`${K}.unlink.modal.title`, {})}
            confirm={tExt(`${K}.button.unlink`, {})}
            onConfirmed={doUnlink}
          >
            <p className='text-sm'>{tExt(`${K}.unlink.modal.content`, {})}</p>
          </ConfirmationModal>

          {connection ? (
            <>
              <SimpleGrid cols={{ base: 1, sm: 2, md: 4 }} spacing='md'>
                <div>
                  <div className='text-xs text-(--mantine-color-dimmed)'>{tExt(`${K}.connection.account`, {})}</div>
                  <div className='truncate'>{connection.accountEmail}</div>
                </div>
                <div>
                  <div className='text-xs text-(--mantine-color-dimmed)'>{tExt(`${K}.connection.folder`, {})}</div>
                  <div className='truncate'>{connection.folderName}</div>
                </div>
                <div>
                  <div className='text-xs text-(--mantine-color-dimmed)'>{tExt(`${K}.connection.connectedAt`, {})}</div>
                  <FormattedTimestamp timestamp={connection.connectedAt} />
                </div>
                <div>
                  <div className='text-xs text-(--mantine-color-dimmed)'>{tExt(`${K}.connection.lastUsedAt`, {})}</div>
                  {connection.lastUsedAt ? (
                    <FormattedTimestamp timestamp={connection.lastUsedAt} />
                  ) : (
                    tExt(`${K}.connection.never`, {})
                  )}
                </div>
              </SimpleGrid>

              {/* What the grant actually covers, spelled out rather than assumed: the
                scopes come straight from Google's token response. */}
              <div className='mt-3 border-t border-(--mantine-color-default-border) pt-3'>
                <div className='text-xs text-(--mantine-color-dimmed)'>{tExt(`${K}.connection.scope`, {})}</div>
                <div className='break-all font-mono text-xs'>{connection.scope}</div>
              </div>
            </>
          ) : (
            <p className='text-sm text-(--mantine-color-dimmed)'>{tExt(`${K}.connection.empty`, {})}</p>
          )}
        </TitleCard>

        {/* Quota badge and mirrored totals. Rendered once stats land; `quota: null`
          degrades to an "unavailable" line rather than hiding the card, because the
          panel-side totals next to it are still true when Google won't talk. */}
        {connected && stats.data && (
          <TitleCard title={tExt(`${K}.stats.title`, {})} icon={<FontAwesomeIcon icon={faHardDrive} />}>
            <Stack gap={4}>
              {stats.data.quota && stats.data.quota.usage !== null ? (
                <>
                  <div className='text-sm'>
                    {stats.data.quota.limit !== null
                      ? tExt(`${K}.stats.usage`, {
                          used: bytesToString(stats.data.quota.usage, 2, true),
                          total: bytesToString(stats.data.quota.limit, 2, true),
                        })
                      : tExt(`${K}.stats.usageOnly`, {
                          used: bytesToString(stats.data.quota.usage, 2, true),
                        })}
                  </div>
                  {stats.data.quota.limit !== null && stats.data.quota.limit > 0 && (
                    <Progress value={(stats.data.quota.usage / stats.data.quota.limit) * 100} hourglass={false} />
                  )}
                </>
              ) : (
                <div className='text-sm text-(--mantine-color-dimmed)'>{tExt(`${K}.stats.unavailable`, {})}</div>
              )}

              <div className='text-xs text-(--mantine-color-dimmed)'>
                {tExt(`${K}.stats.mirrored`, {
                  count: stats.data.completedPushes,
                  size: bytesToString(stats.data.bytes, 2, true),
                })}
              </div>
            </Stack>
          </TitleCard>
        )}

        {/* One restore modal for both tables: it names neither file nor backup in the
          copy - the confirmation is about the *server* being overwritten, which is
          the same act from either door. */}
        <ConfirmationModal
          opened={restoring !== null}
          onClose={() => setRestoring(null)}
          title={tExt(`${K}.restore.modal.title`, {})}
          confirm={tExt(`${K}.button.restore`, {})}
          onConfirmed={doRestore}
        >
          <p className='text-sm'>{tExt(`${K}.restore.modal.content`, {})}</p>
        </ConfirmationModal>

        <TitleCard
          title={tExt(`${K}.files.title`, {})}
          icon={<FontAwesomeIcon icon={faFolderOpen} />}
          rightSection={
            <Group gap='xs'>
              {openFolder && (
                <Button
                  variant='default'
                  size='xs'
                  leftSection={<FontAwesomeIcon icon={faArrowLeft} />}
                  onClick={() => setOpenFolder(null)}
                >
                  {tExt(`${K}.files.back`, {})}
                </Button>
              )}
              {files.hasNextPage ? (
                <Button
                  variant='default'
                  size='xs'
                  loading={files.isFetchingNextPage}
                  onClick={() => void files.fetchNextPage()}
                >
                  {tExt(`${K}.files.loadMore`, {})}
                </Button>
              ) : undefined}
            </Group>
          }
        >
          <ConfirmationModal
            opened={deletingFile !== null}
            onClose={() => setDeletingFile(null)}
            title={tExt(`${K}.files.delete.modal.title`, {})}
            confirm={tExt(`${K}.button.delete`, {})}
            onConfirmed={doDeleteFile}
          >
            <p className='text-sm'>{tExt(`${K}.files.delete.modal.content`, {})}</p>
          </ConfirmationModal>

          <Table
            columns={[
              tExt(`${K}.files.columns.name`, {}),
              tExt(`${K}.files.columns.size`, {}),
              tExt(`${K}.files.columns.modified`, {}),
              tExt(`${K}.files.columns.open`, {}),
              tExt(`${K}.files.columns.action`, {}),
            ]}
            loading={files.isFetching && !files.isFetchingNextPage}
            error={files.error ? httpErrorToHuman(files.error) : null}
            pagination={everything(driveFiles)}
            empty={
              <p className='p-4 text-sm text-(--mantine-color-dimmed)'>
                {openFolder ? tExt(`${K}.files.emptyFolder`, {}) : tExt(`${K}.files.empty`, {})}
              </p>
            }
          >
            {driveFiles.map((file) => (
              <TableRow key={file.fileId}>
                <TableData>
                  {file.isFolder ? (
                    // Folders drill the table rather than download: the name is the
                    // navigation, and the row deliberately offers no delete - trashing
                    // a folder would take every tracked file inside it. A plain button
                    // rather than `Anchor`: core's wrapper drops Mantine's `component`
                    // prop, and this navigates state, not a URL.
                    <button
                      type='button'
                      className='cursor-pointer text-left text-(--mantine-color-blue) hover:underline'
                      onClick={() => setOpenFolder({ id: file.fileId, name: file.name })}
                    >
                      <FontAwesomeIcon icon={faFolder} className='mr-1.5' />
                      {file.name}
                    </button>
                  ) : (
                    file.name
                  )}
                </TableData>
                <TableData>
                  {file.isFolder ? tExt(`${K}.files.noAction`, {}) : bytesToString(file.bytes, 2, true)}
                </TableData>
                <TableData>
                  {file.modifiedTime ? (
                    <FormattedTimestamp timestamp={file.modifiedTime} />
                  ) : (
                    tExt(`${K}.files.noDate`, {})
                  )}
                </TableData>
                <TableData>
                  <Anchor href={file.viewUrl} target='_blank' rel='noopener noreferrer'>
                    {file.isFolder ? tExt(`${K}.files.openFolder`, {}) : tExt(`${K}.files.columns.open`, {})}
                  </Anchor>
                </TableData>
                <TableData>
                  {file.isFolder ? (
                    <span className='text-xs text-(--mantine-color-dimmed)'>{tExt(`${K}.files.noAction`, {})}</span>
                  ) : (
                    <Group gap='xs'>
                      {/* Restore only for archives the extension pushed: the tags are
                        what tell us which server and backup this file *is*, and
                        without them the backend would refuse anyway. */}
                      {file.backupUuid && file.serverUuid && (
                        <Button
                          size='xs'
                          variant='light'
                          leftSection={<FontAwesomeIcon icon={faRotateLeft} />}
                          onClick={() =>
                            setRestoring({
                              serverUuid: file.serverUuid as string,
                              // Only the file id: the backend reads the backup uuid off
                              // the file's own tags, and sending both is a 400.
                              fileId: file.fileId,
                            })
                          }
                        >
                          {tExt(`${K}.button.restore`, {})}
                        </Button>
                      )}
                      <Button
                        size='xs'
                        variant='light'
                        color='red'
                        leftSection={<FontAwesomeIcon icon={faTrash} />}
                        onClick={() => setDeletingFile(file)}
                      >
                        {tExt(`${K}.button.delete`, {})}
                      </Button>
                    </Group>
                  )}
                </TableData>
              </TableRow>
            ))}
          </Table>
        </TitleCard>

        <TitleCard
          title={tExt(`${K}.backups.title`, {})}
          icon={<FontAwesomeIcon icon={faCloudArrowUp} />}
          rightSection={
            <TextInput
              value={search}
              onChange={(event) => setSearch(event.currentTarget.value)}
              placeholder={tExt(`${K}.backups.search`, {})}
              className='w-64'
            />
          }
        >
          <Table
            columns={[
              tExt(`${K}.backups.columns.server`, {}),
              tExt(`${K}.backups.columns.backup`, {}),
              tExt(`${K}.backups.columns.size`, {}),
              tExt(`${K}.backups.columns.created`, {}),
              tExt(`${K}.backups.columns.status`, {}),
              tExt(`${K}.backups.columns.action`, {}),
            ]}
            loading={backupsLoading}
            error={backupsError}
            pagination={backupPage ?? everything(backups)}
            onPageSelect={setBackupsPage}
            empty={<p className='p-4 text-sm text-(--mantine-color-dimmed)'>{tExt(`${K}.backups.empty`, {})}</p>}
          >
            {backups.map((backup) => (
              <TableRow key={backup.backupUuid}>
                <TableData>{backup.serverName}</TableData>
                <TableData>{backup.backupName}</TableData>
                <TableData>
                  {bytesToString(backup.bytes, 2, true)}
                  <span className='ml-2 text-xs text-(--mantine-color-dimmed)'>{tItem('file', backup.files)}</span>
                </TableData>
                <TableData>
                  <FormattedTimestamp timestamp={backup.created} />
                </TableData>
                <TableData>
                  <Badge color={backup.pushStatus ? STATUS_COLORS[backup.pushStatus] : 'gray'} variant='light'>
                    {backup.pushStatus ? statusText(backup.pushStatus) : tExt(`${K}.notPushed`, {})}
                  </Badge>
                </TableData>
                <TableData>
                  <Group gap='xs'>
                    <Button
                      size='xs'
                      variant='light'
                      // Nothing to do while the worker owns the row - re-queueing a
                      // running push is a no-op, and a queued one is already queued.
                      disabled={!connected || backup.pushStatus === 'pending' || backup.pushStatus === 'running'}
                      loading={push.isPending && push.variables?.backupUuid === backup.backupUuid}
                      onClick={() => push.mutate({ serverUuid: backup.serverUuid, backupUuid: backup.backupUuid })}
                    >
                      {backup.pushStatus === 'failed'
                        ? tExt(`${K}.button.retry`, {})
                        : backup.pushStatus === 'completed'
                          ? tExt(`${K}.button.pushAgain`, {})
                          : tExt(`${K}.button.push`, {})}
                    </Button>
                    {/* Only once *this user's* copy actually exists in Drive: a restore
                      with no finished push behind it is a guaranteed dead end. */}
                    {backup.pushStatus === 'completed' && (
                      <Button
                        size='xs'
                        variant='light'
                        leftSection={<FontAwesomeIcon icon={faRotateLeft} />}
                        onClick={() => setRestoring({ serverUuid: backup.serverUuid, backupUuid: backup.backupUuid })}
                      >
                        {tExt(`${K}.button.restore`, {})}
                      </Button>
                    )}
                  </Group>
                </TableData>
              </TableRow>
            ))}
          </Table>
        </TitleCard>

        <TitleCard title={tExt(`${K}.pushes.title`, {})}>
          <Table
            columns={[
              tExt(`${K}.pushes.columns.upload`, {}),
              tExt(`${K}.pushes.columns.status`, {}),
              tExt(`${K}.pushes.columns.progress`, {}),
              tExt(`${K}.pushes.columns.requested`, {}),
              tExt(`${K}.pushes.columns.error`, {}),
              tExt(`${K}.pushes.columns.action`, {}),
            ]}
            loading={pushesLoading}
            error={pushesError}
            pagination={pushPage ?? everything(pushes)}
            onPageSelect={setPushesPage}
            empty={<p className='p-4 text-sm text-(--mantine-color-dimmed)'>{tExt(`${K}.pushes.empty`, {})}</p>}
          >
            {pushes.map((row) => (
              <TableRow key={row.backupUuid}>
                <TableData>{row.backupName ?? tExt(`${K}.pushes.deletedBackup`, {})}</TableData>
                <TableData>
                  <Badge color={STATUS_COLORS[row.status]} variant='light'>
                    {statusText(row.status)}
                  </Badge>
                  {row.attempts > 1 && (
                    <span className='ml-2 text-xs text-(--mantine-color-dimmed)'>
                      {tExt(`${K}.pushes.attempts`, { attempt: row.attempts })}
                    </span>
                  )}
                </TableData>
                <TableData>
                  {row.status === 'running' ? (
                    <Progress
                      className='min-w-40'
                      value={row.totalBytes > 0 ? (row.bytesSent / row.totalBytes) * 100 : Number.NaN}
                      hourglass={false}
                    />
                  ) : (
                    bytesProgressString(row.bytesSent, row.totalBytes)
                  )}
                </TableData>
                <TableData>
                  <FormattedTimestamp timestamp={row.requestedAt} />
                </TableData>
                <TableData>
                  <span className='text-xs text-(--mantine-color-red)'>
                    {row.lastError ?? tExt(`${K}.pushes.noError`, {})}
                  </span>
                </TableData>
                <TableData>
                  {row.status === 'completed' ? (
                    // Nothing to cancel once it has landed; the Files table above is where
                    // an uploaded copy gets removed.
                    <span className='text-xs text-(--mantine-color-dimmed)'>{tExt(`${K}.pushes.noError`, {})}</span>
                  ) : (
                    <Button
                      size='xs'
                      variant='light'
                      color='red'
                      leftSection={<FontAwesomeIcon icon={faBan} />}
                      loading={cancelUpload.isPending && cancelUpload.variables === row.backupUuid}
                      onClick={() => cancelUpload.mutate(row.backupUuid)}
                    >
                      {tExt(`${K}.button.cancel`, {})}
                    </Button>
                  )}
                </TableData>
              </TableRow>
            ))}
          </Table>
        </TitleCard>
      </Stack>
    </AccountContentContainer>
  );
}
