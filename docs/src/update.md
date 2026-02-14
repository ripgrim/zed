# Update Zed

Zed is designed to keep itself up to date automatically. You can always update this behavior in your settings.

## Auto-updates

By default, Zed checks for updates and installs them automatically the next time you restart the app. You’ll always be running the latest version with no extra steps.

If an update is available, Zed will download it in the background and apply it on restart.

## Update Notifications {#update-notifications}

> **Note:** In Zed v0.224.0 and above, update notifications appear in the title bar as described below.

When an update is available, Zed displays a notification in the title bar showing the update progress:

- **Checking for updates**: Shows when manually checking for updates
- **Downloading**: Appears while the update downloads in the background
- **Installing**: Shows briefly during installation
- **Ready to update**: When an update is ready, click the notification to restart Zed and apply the update

You can dismiss the update notification by clicking the X button. When dismissed, a badge appears on your user avatar, and "Restart to update Zed" appears at the top of the user menu.

If an update fails, an error notification appears. Click it to view the error log.

## How to check your current version

To check which version of Zed you're using:

Open the Command Palette (Cmd+Shift+P on macOS, Ctrl+Shift+P on Linux/Windows).

Type and select `zed: about`. A modal will appear with your version information.

## How to control update behavior

If you want to turn off auto-updates, open the Settings Editor (Cmd ,) and find `Auto Update` under General Settings.
