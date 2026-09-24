import { Stack } from '@mantine/core';
import { useForm } from '@mantine/form';
import { zod4Resolver } from 'mantine-form-zod-resolver';
import { useEffect, useState } from 'react';
import { httpErrorToHuman } from '@/api/axios.ts';
import Button from '@/elements/buttons/Button.tsx';
import TitleCard from '@/elements/data-display/TitleCard.tsx';
import Alert from '@/elements/feedback/Alert.tsx';
import NumberInput from '@/elements/input/NumberInput.tsx';
import PasswordInput from '@/elements/input/PasswordInput.tsx';
import Switch from '@/elements/input/Switch.tsx';
import TextArea from '@/elements/input/TextArea.tsx';
import TextInput from '@/elements/input/TextInput.tsx';
import Group from '@/elements/layout/Group.tsx';
import Code from '@/elements/typography/Code.tsx';
import { useToast } from '@/providers/ToastProvider.tsx';
import { useGlobalStore } from '@/stores/global.ts';
import { getAdminSettings, saveAdminSettings, testCredentials } from './api.ts';
import { GDriveConfig, GDriveProbe, gdriveConfigSchema, REDIRECT_URI_PATH } from './schemas.ts';
import { useExtTranslations } from './translations.ts';

/**
 * Keys sit under `pages.admin.googleDrive` so an operator's translation files keep the
 * same shape core's do - `tExt` prepends this extension's own namespace on top of it.
 */
const K = 'pages.admin.googleDrive';

export default function ConfigurationPage() {
  const { t: tExt } = useExtTranslations();
  const { addToast } = useToast();
  const appUrl = useGlobalStore((state) => state.settings.app.url);

  const [saving, setSaving] = useState(false);
  const [secretSet, setSecretSet] = useState(false);
  const [probing, setProbing] = useState(false);
  const [probe, setProbe] = useState<GDriveProbe | null>(null);

  const form = useForm<GDriveConfig>({
    // The secret starts blank on purpose: an empty PUT means "keep what is stored", so
    // this field only ever carries a value the operator has just typed.
    initialValues: {
      clientId: '',
      clientSecret: '',
      folderName: 'Calagopus Backups',
      autoPush: true,
      deleteCopy: true,
      keepCopies: 0,
      pushInclude: '',
      pushExclude: '',
      pushDatabaseBackups: true,
      folderPerServer: true,
      sharedFolderId: '',
    },
    validateInputOnBlur: true,
    validate: zod4Resolver(gdriveConfigSchema),
  });

  const load = async () => {
    const settings = await getAdminSettings().catch((error) => {
      addToast(httpErrorToHuman(error), 'error');
      return null;
    });

    if (!settings) return;

    setSecretSet(settings.clientSecretSet);
    form.setValues({
      clientId: settings.clientId,
      clientSecret: '',
      folderName: settings.folderName,
      autoPush: settings.autoPush,
      deleteCopy: settings.deleteCopy,
      keepCopies: settings.keepCopies,
      pushInclude: settings.pushInclude,
      pushExclude: settings.pushExclude,
      pushDatabaseBackups: settings.pushDatabaseBackups,
      folderPerServer: settings.folderPerServer,
      sharedFolderId: settings.sharedFolderId,
    });
  };

  // Load once on mount. `form` stays usable before the response lands - it holds the
  // "not configured yet" values - and re-running would clobber what the operator types.
  useEffect(() => {
    void load();
  }, []);

  const doSave = () => {
    setSaving(true);

    saveAdminSettings(form.values)
      .then((result) => {
        setSecretSet(result.configured);
        form.setFieldValue('clientSecret', '');
        // The probe reads *stored* settings, so a result against the previous save
        // would be answering about credentials the operator just changed.
        setProbe(null);
        addToast(result.configured ? tExt(`${K}.toast.configured`, {}) : tExt(`${K}.toast.saved`, {}), 'success');
      })
      .catch((error) => addToast(httpErrorToHuman(error), 'error'))
      .finally(() => setSaving(false));
  };

  // Deliberately a plain then-chain against the *saved* credentials: the endpoint
  // reports Google's classification in `detail`, which is the diagnosis - showing a
  // second toast on failure would bury it under a generic one.
  const doTest = () => {
    setProbing(true);
    setProbe(null);

    testCredentials()
      .then(setProbe)
      .catch((error) => addToast(httpErrorToHuman(error), 'error'))
      .finally(() => setProbing(false));
  };

  return (
    <TitleCard title={tExt(`${K}.title`, {})}>
      <Stack gap='md' p='md'>
        <Alert color='blue' title={tExt(`${K}.redirectUri`, {})}>
          <Code block>{`${appUrl.replace(/\/+$/, '')}${REDIRECT_URI_PATH}`}</Code>
          <p className='mt-2 text-sm'>{tExt(`${K}.redirectUriHint`, {})}</p>
        </Alert>

        <p className='text-sm text-(--mantine-color-dimmed)'>{tExt(`${K}.description`, {})}</p>

        <form onSubmit={form.onSubmit(doSave)}>
          <Stack gap='md'>
            <TextInput
              label={tExt(`${K}.clientId`, {})}
              placeholder={tExt(`${K}.clientIdPlaceholder`, {})}
              {...form.getInputProps('clientId')}
            />

            <PasswordInput
              label={tExt(`${K}.clientSecret`, {})}
              placeholder={tExt(`${K}.clientSecretPlaceholder`, {})}
              description={secretSet ? tExt(`${K}.clientSecretStored`, {}) : tExt(`${K}.clientSecretMissing`, {})}
              {...form.getInputProps('clientSecret')}
            />

            <TextInput
              label={tExt(`${K}.folderName`, {})}
              description={tExt(`${K}.folderNameHint`, {})}
              {...form.getInputProps('folderName')}
            />

            <Switch
              label={tExt(`${K}.autoPush`, {})}
              description={tExt(`${K}.autoPushHint`, {})}
              {...form.getInputProps('autoPush', { type: 'checkbox' })}
            />

            <Switch
              label={tExt(`${K}.deleteCopy`, {})}
              description={tExt(`${K}.deleteCopyHint`, {})}
              {...form.getInputProps('deleteCopy', { type: 'checkbox' })}
            />

            <NumberInput
              label={tExt(`${K}.keepCopies`, {})}
              description={tExt(`${K}.keepCopiesHint`, {})}
              min={0}
              max={1000000}
              {...form.getInputProps('keepCopies')}
            />

            <Switch
              label={tExt(`${K}.pushDatabaseBackups`, {})}
              description={tExt(`${K}.pushDatabaseBackupsHint`, {})}
              {...form.getInputProps('pushDatabaseBackups', { type: 'checkbox' })}
            />

            <Switch
              label={tExt(`${K}.folderPerServer`, {})}
              description={tExt(`${K}.folderPerServerHint`, {})}
              {...form.getInputProps('folderPerServer', { type: 'checkbox' })}
            />

            <TextArea
              label={tExt(`${K}.pushInclude`, {})}
              description={tExt(`${K}.pushIncludeHint`, {})}
              rows={3}
              {...form.getInputProps('pushInclude')}
            />

            <TextArea
              label={tExt(`${K}.pushExclude`, {})}
              description={tExt(`${K}.pushExcludeHint`, {})}
              rows={3}
              {...form.getInputProps('pushExclude')}
            />

            <TextInput
              label={tExt(`${K}.sharedFolderId`, {})}
              description={tExt(`${K}.sharedFolderIdHint`, {})}
              placeholder='1a2B3c4D5e6F7g8H9i0J'
              {...form.getInputProps('sharedFolderId')}
            />
          </Stack>

          {probe && (
            <Alert
              className='mt-4'
              color={probe.valid ? 'green' : 'red'}
              title={probe.valid ? tExt(`${K}.testValid`, {}) : tExt(`${K}.testInvalid`, {})}
            >
              <p className='text-sm'>{probe.detail}</p>
              <p className='mt-2 font-mono text-xs break-all'>{probe.redirectUri}</p>
            </Alert>
          )}

          <Group mt='md'>
            <Button type='submit' disabled={!form.isValid()} loading={saving}>
              {tExt(`${K}.button.save`, {})}
            </Button>
            <Button
              type='button'
              variant='default'
              loading={probing}
              // Reads the stored settings, so there has to be something stored to test.
              disabled={!secretSet}
              onClick={doTest}
            >
              {tExt(`${K}.testCredentials`, {})}
            </Button>
          </Group>

          <p className='mt-2 text-xs text-(--mantine-color-dimmed)'>{tExt(`${K}.testHint`, {})}</p>
        </form>
      </Stack>
    </TitleCard>
  );
}
