# User guide

[Back to Quick Share](../README.md) · [Installation](INSTALLATION.md) · [Architecture and CLI](ARCHITECTURE.md)

Omarchy Quick Share connects your desktop to Android's existing Quick Share. The phone does not need a companion application. The desktop panel uses Omarchy's theme and supports English and Español.

The panel header reads the installed plugin version from `manifest.json`, including prerelease suffixes when present. It does not infer a version from the Git branch or substitute the backend executable's version.

## Before sharing

- Complete **Install service** from the panel and wait for **Ready**.
- Keep Bluetooth enabled for Bluetooth discovery and transfers. A compatible shared local network provides the LAN file route.
- Unlock the phone so you can handle its Quick Share prompt.
- The **receiving** device must be visible. The **sending** device chooses files and a destination.

Transport availability depends on the radio, network and permissions. The transfer card reports the medium actually in use; a capability label alone is not a completed transfer.

## Phone sends, Omarchy receives

1. On the PC, open the Quick Share panel.
2. Select **Receive for 120 s**. The bar shows the remaining visibility time.

![PC control for temporary Quick Share visibility](assets/screenshots/01-receive-visibility.png)

*Use Receive for 120 s when the phone will send to this PC. The image shows the button before activation; once enabled, it shows the remaining visibility time.*

3. On the phone, open the file and choose **Share**.
4. Choose **Quick Share** in Android's share menu.
5. Select the displayed computer.
6. Compare the code shown on both devices.
7. On the PC, accept only if the code matches and the offered files are expected.

![Incoming file offer, verification code and consent buttons](assets/screenshots/02-incoming-pin.png)

*The incoming card is where you compare the code and choose PIN matches · accept or Reject. Accept only the request you expect, using the code currently shown on both devices.*

You can reject an unwanted offer. Incoming notifications work with the panel closed; open the request to inspect it and handle consent.

Visibility is temporary. Its expiry stops new discovery offers, not an already established file transfer.

## Omarchy sends, phone receives

1. On the phone, open Quick Share in **Receive** mode.
2. On the PC, open the panel and choose one or more files.
3. Select the phone from nearby devices. If exactly one destination is available it is preselected; multiple destinations require a choice.
4. Press **Send**.
5. Compare the verification code on both devices.
6. Approve the request on each device when prompted.

![Two files selected and an Android destination ready for sending](assets/screenshots/03-send-files.png)

*The selected device is highlighted. Review both the destination and the file list before using Send; choosing files alone does not start a transfer.*

A manually selected destination that disappears is cleared, not replaced with another device. A preloaded file selection never starts sending by itself.

### Send with an Android Quick Share QR code

Available in stable plugin and backend **0.3.0**:

1. Connect the PC and Android device to the same local network. Keep Wi-Fi and Bluetooth enabled on Android.
2. Select one or more files in the panel, or open a selection from Nautilus.
3. Choose **Show QR**. You do not need to select a nearby destination.
4. Scan the code with Android's Quick Share QR scanner, or its supported camera/Quick Settings QR scanner.
5. When the transfer request appears, compare the verification code and confirm it on the PC. Handle any request shown on Android as well.

The QR contains a temporary Quick Share public key, not file paths, Wi-Fi credentials or a web-download link. Only one invitation waits at a time; it expires after **120 seconds** and is consumed by the first matching receiver. **Close QR**, changing the selection or closing the panel withdraws a pending invitation. These actions do not cancel an already-created transfer: use its **Cancel** control.

This QR flow discovers receivers over **LAN**; it does not implement Bluetooth-only QR discovery or permanent Bluetooth pairing. Existing Bluetooth sending and receiving remain available through the ordinary workflow. If **Show QR** asks you to update the service, updating QML alone is not sufficient; update the native plugin to stable **0.3.0**, then use **Settings → Service → Update service**. See the [stable upgrade instructions](INSTALLATION.md#update), including how to return an alpha checkout to `main`.

### From Nautilus

Select one or multiple local files, right-click and choose **Send via Quick Share**. This opens the panel with the exact selection; you still control the destination, Send and consent.

![Two local files selected in Nautilus with Send via Quick Share in the context menu](assets/screenshots/06-nautilus-menu.png)

*Select the files first, then right-click the selection. Send via Quick Share hands that selection to the desktop panel; it does not send immediately.*

Folders, remote URLs and mixed unsupported selections do not offer this action. Spaces and Unicode names are supported. The extension accepts up to 1,000 local files per selection.

After installing or updating the Python extension, save work in all Nautilus windows and restart Nautilus. Opening another window in the same process does not reload extensions.

## Progress, cancellation and results

Cards show byte progress, the active transport and the terminal result. Use **Cancel** to stop an active transfer. Closing the panel does not cancel it, and the Rust service continues independently of the shell.

![Transfer card showing byte progress, transport and Cancel](assets/screenshots/04-transfer-progress.png)

*The card identifies the active medium and the bytes transferred. Cancel is the explicit stop action; closing the panel is not.*

**Cancelling…** indicates a request in progress, not fabricated completion. A disconnected peer produces a failed transfer rather than a completed file.

Omarchy stages incoming files privately and publishes them only after completion. Existing files are not overwritten: a duplicate name receives a numeric prefix. Local staged partials are removed on cancellation or failure. Android manages its own destination/cache cleanup; sender cancellation does not provide a remote-file deletion command.

## Service controls and maintenance

If the service is missing or incompatible, setup controls appear automatically. When the panel is **Ready**, the service controls are hidden until you expand settings:

1. Scroll toward the bottom of the panel and open **Settings**.
2. Scroll back toward the top to find the **Service** section.
3. Choose **Install service**, **Update service** or **Remove service** as appropriate. A dedicated terminal explains the action and any authorization it needs.

![Expanded service controls for installation, update and removal](assets/screenshots/05-service-controls.png)

*These buttons maintain the Rust backend and Nautilus registration. Omarchy's native plugin commands maintain the Git plugin itself; see [Installation and maintenance](INSTALLATION.md) for the complete lifecycle.*

Finish or cancel active transfers before service maintenance. If a plugin update leaves the previous panel controls on screen, run `omarchy restart shell` to reload Quickshell; this does not restart the independent transfer service.

## Transfer effects

Since **v0.2.0**, open **Settings → Transfer effects** and enable **Animated transfer companion**. The preference is off by default; your saved choice persists across shell reloads and plugin updates.

- Preparation/connection uses orbiting signal beacons; active file sending uses outgoing paper packets; receiving uses inward packets and rings.
- Confirmed **Wi-Fi / LAN**, **Bluetooth** and **Direct Wi-Fi / hotspot** have different motifs. An unconfirmed transport stays neutral rather than guessing from radio availability.
- With the panel open, the halo stays outside the card and its safety margin. Closing the panel moves the small courier companion to the **lower-left corner**, away from the upper-right notification stack and clear of a left or bottom bar. Notifications keep their normal location and behavior.
- The overlay does not accept pointer or keyboard input. Turning it off immediately removes the overlay and animation clock.

To try the visuals without a phone, choose **Preview activity** and **Preview transport**, then **Preview · 8 s**. **Stop** ends it immediately. These eight-second previews perform no file or network transfer, accept no PIN, and do not modify the real transfer model or network permission. Real active transfers take priority.

Transfer effects are a UI feature compatible with backend **0.1.2 or newer**; enabling them does not install or restart a service. See [Installation and maintenance](INSTALLATION.md#update) for native plugin updates and explicit service maintenance.


## Language and destination

Choose **English** or **Español** in the panel. This controls the panel, bar, file-manager action and new project notifications. File names, device names and technical errors remain unchanged.

In **Settings**, select **Choose folder… / Elegir carpeta…** to open the system's folder chooser. The panel closes so it does not cover that dialog. Select an existing writable directory; cancelling keeps the current destination. Reopen the panel to see the selected path. The initial destination is Downloads.

The path is displayed as non-editable text, not an embedded text input. Folder selection requires backend **0.2.5 or newer** with directory-picker support; an older backend shows an update hint rather than offering an unsupported action. Do not choose a destination that lacks space for the offered files.

## Network permissions

Local LAN data uses TCP and mDNS discovery. Bluetooth data uses Classic RFCOMM, with BLE/GATT metadata for discovery.

With backend **0.2.3**, an incoming request may initially connect over Bluetooth, then negotiate the existing LAN after you confirm the code and accept. This reuses the normal LAN TCP port and current network without enabling Wi-Fi-interruption permission or opening additional ports. The card changes to **LAN** only after the replacement connection is authenticated. If that connection cannot be established before cutover, healthy Bluetooth remains the file route; after cutover starts, a failed migration ends the transfer rather than silently resuming it.

The advanced **Allow interrupting my Wi-Fi for a direct connection** setting grants permission for a temporary network change. Leave it off unless you intend that interruption. Permission does not itself establish a direct route. This project does not advertise Wi-Fi Direct/P2P, Google contacts-only visibility, account integration or lossless transfer resume.

Use trusted local networks. Do not expose the service with router port forwarding or disable the firewall globally.

## Troubleshooting

### The computer does not appear on the phone

Make sure the PC's **Receive for 120 s** visibility is active. On the phone, use the file's **Share → Quick Share** flow when sending; phone Receive mode is for the opposite direction.

Keep the required radio enabled and grant Android's nearby-device permissions. Check the panel's service state and any explicit error before changing network settings.

### The phone does not appear on the PC

Open Quick Share **Receive** mode on the phone. Refresh the PC's nearby-device list. Do not confuse Bluetooth discovery with file data: the completed card identifies the transport actually used.

### The Nautilus menu is absent

Ensure service setup installed the extension and `nautilus-python`. Restart Nautilus after saving work. The action is offered for local regular files, not folders or remote/mixed selections.

### A zero-byte file is rejected

Android Quick Share can reject an empty file offer. The project does not silently ZIP it, add bytes or alter its contents to pretend that an unchanged empty-file transfer succeeded.

### Service setup or maintenance fails

Read the setup terminal's error, then consult [Installation and maintenance](INSTALLATION.md). Do not start a second daemon alongside the installed session service. Finish or explicitly cancel transfers before updating or removing it.

For diagnostics, use the panel's service information, `omarchy-quickshare doctor`, `omarchy-quickshare status` and the systemd user journal. Redact codes, credentials, addresses, personal paths and location data before sharing reports. See [Security](../SECURITY.md) for private vulnerability reporting.
