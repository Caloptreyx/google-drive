import { faCloudArrowUp } from '@fortawesome/free-solid-svg-icons';
import { Extension, ExtensionContext } from 'shared';
import ConfigurationPage from './ConfigurationPage.tsx';
import GoogleDriveAccountPage from './GoogleDriveAccountPage.tsx';
import { getExtTranslations } from './translations.ts';

class DevCaloptreyxGdriveExtension extends Extension {
  public cardConfigurationPage: React.FC | null = ConfigurationPage;
  public cardComponent: React.FC | null = null;

  public initialize(ctx: ExtensionContext): void {
    // `name` stays lazy: this runs before the TranslationProvider exists, and the sidebar
    // resolves the label on every render instead.
    ctx.extensionRegistry.enterRoutes((routes) =>
      routes.addAccountRoute({
        name: () => getExtTranslations().t('pages.account.googleDrive.title', {}),
        icon: faCloudArrowUp,
        path: '/google-drive',
        element: GoogleDriveAccountPage,
      }),
    );
  }
}

export default new DevCaloptreyxGdriveExtension();
