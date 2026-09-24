import { defineTranslations } from 'shared';

const translations = defineTranslations({
  items: {},
  translations: {
    pages: {
      account: {
        googleDrive: {
          title: 'Google Drive',
          subtitle: 'Link your own Google Drive and push server backups to it.',
          notConfiguredTitle: 'Not configured yet',
          notConfigured:
            'Google Drive is not set up on this panel yet. An administrator needs to enter Google OAuth credentials on the extension before you can link an account.',
          connection: {
            title: 'Connection',
            status: {
              linked: 'Linked',
              reauth: 'Reconnect needed',
            },
            reauth: {
              title: 'Google rejected the stored connection',
              content:
                'Google refused this account’s refresh grant - it was revoked, expired, or access was removed - so uploads are paused until you connect again. Queued uploads resume on their own after a reconnect.',
            },
            account: 'Google account',
            folder: 'Backup folder',
            connectedAt: 'Linked',
            lastUsedAt: 'Last used',
            never: 'Never',
            empty: 'Nothing linked yet. Connect an account to start pushing backups to Drive.',
            scope: 'Granted access',
          },
          stats: {
            title: 'Storage & uploads',
            usage: '{used} of {total} used',
            usageOnly: '{used} used',
            unavailable: 'Storage usage is unavailable right now.',
            mirrored: '{count} uploads mirrored · {size} total',
          },
          button: {
            connect: 'Connect Google Drive',
            unlink: 'Unlink',
            push: 'Push to Drive',
            pushAgain: 'Push again',
            retry: 'Retry',
            cancel: 'Cancel',
            delete: 'Delete',
            restore: 'Restore',
            refresh: 'Refresh',
          },
          unlink: {
            modal: {
              title: 'Unlink this Google account?',
              content:
                'Backups already uploaded to Drive stay where they are. This panel stops being able to upload or browse until you link again.',
            },
            toast: 'Google Drive account unlinked.',
          },
          toast: {
            queued: 'Backup queued for upload to Google Drive.',
            cancelled: 'Upload removed from the queue.',
            restored: 'Restore started - the server is stopping and the archive will stream in from Google Drive.',
          },
          restore: {
            modal: {
              title: 'Restore this server from Google Drive?',
              content:
                'The server is stopped and its files are replaced with this archive. The copy downloads from your Google Drive through the panel while the restore runs, so keep the panel and Drive reachable. Whatever is on the server right now is overwritten.',
            },
          },
          files: {
            title: 'Files in Drive',
            columns: {
              name: 'File',
              size: 'Size',
              modified: 'Modified',
              open: 'Open',
              action: 'Action',
            },
            empty: 'Nothing in Drive yet. Push a backup and it will appear here.',
            emptyFolder: 'This folder is empty.',
            back: 'Back to the backup folder',
            openFolder: 'Open folder',
            noAction: '—',
            loadMore: 'Load more',
            noDate: '—',
            delete: {
              modal: {
                title: 'Delete this file from Drive?',
                content:
                  'The copy in Google Drive is sent to the trash. The backup itself stays in the panel and can be pushed again.',
              },
              toast: 'File moved to the Drive trash.',
            },
          },
          backups: {
            title: 'Backups',
            columns: {
              server: 'Server',
              backup: 'Backup',
              size: 'Size',
              created: 'Created',
              status: 'Drive',
              action: 'Action',
            },
            empty: 'No finished backups are available yet. Create a backup on one of your servers first.',
            search: 'Search servers or backups',
          },
          pushes: {
            title: 'Uploads',
            columns: {
              upload: 'Backup',
              status: 'Status',
              progress: 'Progress',
              requested: 'Requested',
              error: 'Error',
              action: 'Action',
            },
            empty: 'Nothing has been pushed to Google Drive yet.',
            deletedBackup: 'Deleted backup',
            attempts: 'Attempt {attempt}',
            noError: '—',
          },
          notPushed: 'Not pushed',
          status: {
            pending: 'Queued',
            running: 'Uploading',
            completed: 'Uploaded',
            failed: 'Failed',
          },
        },
      },
      admin: {
        googleDrive: {
          title: 'Google Drive Backups',
          description:
            'Credentials for the single Google OAuth client your users link their own Drive accounts with. Create them in the Google Cloud console as a Web application, then paste the redirect URI below.',
          redirectUri: 'Redirect URI',
          redirectUriHint: 'Add this exact URI to the OAuth client in the Google Cloud console.',
          clientId: 'Client ID',
          clientIdPlaceholder: '1234567890-abcdefg.apps.googleusercontent.com',
          clientSecret: 'Client secret',
          clientSecretPlaceholder: 'Leave blank to keep the stored secret',
          clientSecretStored: 'A secret is already stored. Type a new one only to rotate it.',
          clientSecretMissing: 'No secret stored yet - the extension cannot work until you save one.',
          folderName: 'Backup folder name',
          folderNameHint: "The folder created inside each user's Drive to collect their backups in.",
          autoPush: 'Push backups automatically',
          autoPushHint:
            "When a server backup finishes, queue it straight to the owner's linked Drive. Users can still push by hand either way.",
          deleteCopy: 'Delete the Drive copy with the backup',
          deleteCopyHint:
            'Deleting a backup in the panel - including retention pruning - also moves its copy in Google Drive to the trash. Turn this off to keep Drive as an archive the panel can no longer reach.',
          keepCopies: 'Keep newest copies per server',
          keepCopiesHint:
            'After each upload, only this many of a server’s newest copies stay in Drive; older ones go to the Drive trash. 0 keeps everything.',
          pushInclude: 'Only push these servers',
          pushIncludeHint:
            'Newline or comma separated globs on the server name, e.g. prod-* or staging?. Leave empty to include every server. Manual pushes ignore this list.',
          pushExclude: 'Never push these servers',
          pushExcludeHint: 'Same glob syntax, and it wins over the include list. Manual pushes ignore this list too.',
          pushDatabaseBackups: 'Push database backups too',
          pushDatabaseBackupsHint:
            'Database-instance backups queue and upload alongside file backups, and show up in the pushable list on users’ account pages.',
          folderPerServer: 'A folder per server',
          folderPerServerHint:
            'Each server gets a subfolder named after it instead of one flat folder. Applies to new uploads; files already in Drive stay where they are.',
          sharedFolderId: 'Shared drive folder ID',
          sharedFolderIdHint:
            'Optional. Folder ID inside a shared drive to nest the backup folder under (empty = each user’s own My Drive). Applies the next time an account connects - unlink and reconnect to move an existing account. Enumerating shared drives would need a broader OAuth scope, which is why the ID is pasted by hand.',
          testCredentials: 'Test credentials',
          testHint:
            'Exchanges a throwaway code against Google using the saved credentials - save first, then test. No user account is involved.',
          testValid: 'Google accepted the saved credentials - a real link would work.',
          testInvalid: 'Google rejected the saved credentials.',
          button: {
            save: 'Save',
          },
          toast: {
            saved: 'Google Drive settings saved.',
            configured: 'Google Drive is now configured.',
          },
        },
      },
    },
  },
});

export const useExtTranslations = translations.useTranslations.bind(translations);
export const getExtTranslations = translations.getTranslations.bind(translations);

export default translations;
