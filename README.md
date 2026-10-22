# Omarchy Quick Share

**Android Quick Share, right in your Omarchy desktop.**

Send photos, videos and files between your Android phone and Omarchy using the phone's existing **Quick Share**. Use a local network for file sharing or Bluetooth when a shared network is unavailable. No companion Android app is required.

![Omarchy Quick Share — Android file sharing for your desktop](preview.png)

- **Native desktop experience:** a theme-integrated bar widget, transfer panel and incoming notifications.
- **One file or a whole selection:** send from the panel or Nautilus's **Send via Quick Share** menu.
- **Stay in control:** compare the code, approve incoming files, follow progress and cancel explicitly.
- **English or Español:** choose your language in the panel.
- **Optional transfer effects:** in **Settings → Transfer effects**, enable **Animated transfer companion** (off by default). Try an eight-second visual preview without sending files. See the [user guide](docs/USER_GUIDE.md#transfer-effects).

See the [pictured user guide](docs/USER_GUIDE.md) for the full walkthrough.

## Install

On an Omarchy desktop:

```bash
omarchy plugin add https://github.com/PuroDelphi/omarchy-quickshare.git --enable
```

Open **Quick Share** from the bar and select **Install service**. A terminal opens to set up the Rust service and file-manager integration. Read its messages and authorize dependency installation if requested. Wait for **Ready** before sharing files.

Requirements: Omarchy/Quickshell, BlueZ for Bluetooth, a working desktop file chooser and Nautilus for the context-menu action. Source setup also needs Rust/Cargo and build dependencies. See the [installation guide](docs/INSTALLATION.md) for the complete dependency list and package-mode details.

After first setup, save work and restart Nautilus to load its extension.

## Receive on Omarchy

1. In the PC's Quick Share panel, select **Receive for 120 s**.
2. On the phone, share a file through **Quick Share** and select your computer.
3. Compare the code on both devices.
4. Accept the request on the PC only if the code matches.

![Incoming Quick Share request with verification code and Accept/Reject controls](docs/assets/screenshots/02-incoming-pin.png)

*Compare the code on both devices before accepting.*

Received files go to your configured download folder. Change it with **Settings → Choose folder…**.

## Send to Android

1. Put the phone's **Quick Share** in **Receive** mode.
2. On the PC, select files in Nautilus and choose **Send via Quick Share**, or choose files in the panel.
3. Select the phone if several destinations are available; a sole destination is selected automatically.
4. Press **Send**, compare the code and approve the transfer on both devices.

![Android destination selected with two files ready to send](docs/assets/screenshots/03-send-files.png)

*Check the destination and file selection, then press Send.*

Opening the panel never sends a file automatically. Closing it does not stop an active transfer; use **Cancel** when you want to stop.

Choose regular local files through their real folder paths. Symbolic links, including linked parent folders, are rejected; select the actual target instead.

In **stable 0.3.0**, you can also select files and choose **Show QR**. Scan it with Android Quick Share on the same local network, then compare and confirm the verification code when requested. Update both the plugin and backend for this feature; see the [QR instructions](docs/USER_GUIDE.md#send-with-an-android-quick-share-qr-code).

## Update or remove

Finish or cancel active transfers first.

```bash
omarchy plugin update macondo.quickshare
```

Open **Settings** in the panel to reveal the **Service** controls, then select **Update service**. This updates the Rust backend from the current plugin checkout; system-package changes require terminal authorization. Backend **0.3.6** is required for the empty-UDP mDNS reception fix and includes the earlier DNS resource limits; updating the panel or reloading the shell alone does not update an older daemon.

To uninstall, open **Settings**, select **Remove service** first, then run:

```bash
omarchy plugin remove macondo.quickshare
```

Service removal preserves configuration and received files. The terminal explains whether an installed system package is being removed or retained. Omarchy's native plugin removal controls the bar entry and its inline settings.

## Documentation

[User guide](docs/USER_GUIDE.md) · [Installation and maintenance](docs/INSTALLATION.md) · [Architecture and CLI](docs/ARCHITECTURE.md) · [Recorded verification](docs/VERIFICATION.md)

[Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) · [Code of conduct](CODE_OF_CONDUCT.md)

## License and provenance

Original project code is [MIT](LICENSE). The vendored rquickshare core remains **GPL-3.0-only**, with separately licensed schemas; the combined daemon is **not MIT-only**. See the [component licenses and source obligations](docs/ARCHITECTURE.md#licenses-and-corresponding-source).

This is an independent project, not an official Google, Android or Omarchy product. Product names belong to their respective owners.

Powered by JhonnySuarez - PuroDelphi. Support the project through [GitHub Sponsors](https://github.com/sponsors/PuroDelphi) or [PayPal](https://www.paypal.com/donate/?hosted_button_id=KBAUBYYDNHQNQ).

[![PayPal donation QR](docs/assets/paypal-qr.png)](https://www.paypal.com/donate/?hosted_button_id=KBAUBYYDNHQNQ)
