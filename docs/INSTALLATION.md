# Installation and maintenance

[Back to Quick Share](../README.md) · [User guide](USER_GUIDE.md) · [Architecture](ARCHITECTURE.md)

## Native Omarchy plugin lifecycle

Quick Share uses Omarchy's normal Git plugin commands. Its stable ID is **`macondo.quickshare`** and the repository has one root manifest.

```bash
omarchy plugin add https://github.com/PuroDelphi/omarchy-quickshare.git --enable
omarchy plugin update macondo.quickshare
omarchy plugin remove macondo.quickshare
```

Omarchy clones the repository, validates the manifest, enables the widget and manages later Git updates/removal. The service installer does **not** replace that checkout with a copied QML folder. Keep local code modifications separate from the installed checkout so native updates can fast-forward safely.

The native commands manage the plugin, not external Rust packages. **Install service**, **Update service** and **Remove service** in the Quick Share panel open a dedicated terminal for the backend and Nautilus integration. This follows the native-plugin-plus-service pattern used by other Omarchy applications.

When the service is ready, open **Settings** to reveal the **Service** controls near the top of the panel. Setup controls are already visible if the service is missing, incompatible or disabled.

Do not run these operations as root. Any necessary system-package authorization happens locally inside the setup terminal. Read the plan and prompts before approving.

## Dependencies

| Purpose | Requirements |
| --- | --- |
| Desktop | Omarchy, Quickshell, session D-Bus, systemd user manager, `jq` |
| Secure outgoing files | Linux `openat2` support (kernel 5.6 or newer), normal read permission, and source paths without symlinks |
| Bluetooth | BlueZ with a Classic/BLE-capable adapter and normal user permissions |
| Local networking | A usable local interface; NetworkManager for read-only upgrade selection and managed network changes |
| Shared IPv4/AP support | `dnsmasq`, invoked by NetworkManager when needed |
| File picker | `xdg-desktop-portal` and a FileChooser backend, such as `xdg-desktop-portal-gtk` |
| Nautilus action | `nautilus`, `nautilus-python`, `python-gobject`, `gtk4` |
| Source compilation | Rust/Cargo, `protobuf`/`protoc`, `pkgconf`, D-Bus development files and a C toolchain |
| Local Arch package build | `base-devel`, `makepkg`, `git` and the source compilation dependencies |

NetworkManager starts its own `dnsmasq` for shared IPv4. Do **not** enable `dnsmasq.service` just for Quick Share. Setup must not globally disable a firewall or overwrite Omarchy's package-owned desktop files.

LAN needs mDNS UDP **5353** and the configured TCP listener, normally **47593**, on the trusted local network. Backend **0.2.3** uses that same listener for existing-LAN bandwidth upgrades instead of opening per-session TCP ports. Firewall and access-point policy must permit peer access to the configured listener; setup does not add firewall rules, enable router forwarding or weaken network policy.

## First setup

1. Add and enable the plugin with the native command above.
2. Open **Quick Share** from the bar.
3. Select **Install service**.
4. Follow the terminal's dependency/build/authorization messages.
5. Wait for the panel's **Ready** state.

Restart Nautilus after saving work to load a newly installed Python extension. Setup does not silently close file-manager windows.

### Source-managed service

Without an installed system package, setup builds the locked Rust source from the plugin checkout and installs an owned user executable, user unit and file-manager registration. It retains source/license material for the distributed daemon. The current native Git checkout remains untouched.

The graphical session must have its normal user executable path available for CLI use. The plugin and extension resolve the maintained backend explicitly rather than choosing an arbitrary executable from a modified PATH.

### System-package service

An existing `/usr/bin/omarchy-quickshare` package is used rather than silently shadowed by another user binary. The original `0.1.0` integration used a copied-plugin ownership marker; native-Git-compatible setup upgrades that backend through the visible package-build/install flow when required.

Package changes use the real package manager and require local authorization. Package generation uses source from the current plugin checkout, not an unrelated remote build script or a promised AUR package.

## Update

Finish or explicitly cancel active transfers first.

```bash
omarchy plugin update macondo.quickshare
```

The native updater follows the repository's stable `main` channel. Stable **0.3.5** bounds raw mDNS cache/storage and derived work inside the vendored dependency, while retaining 0.3.4's securely opened outgoing-file descriptors. **This is a backend/client security update, not a QML-only update:** finish or cancel transfers, update the native plugin, then choose **Settings → Service → Update service** and review the terminal's requested changes. Source mode rebuilds the current locked checkout; package mode installs the corresponding Arch package with local authorization. Use **Install service** if setup never completed. No silent update hook installs or restarts the daemon.

Backend **0.1.2 or newer** supports the private file-selection handoff and the UI-only transfer effects; effects alone do not require a backend upgrade. A 0.1.1 or older backend displays **Service update required**; update the service as well as the native plugin before sharing files. Restart Nautilus after saving work so its process loads the updated extension.

Backend **0.2.4** includes the bounded discovery lifecycle plus the incoming LAN-first/shared-port improvements from alpha. Updating QML or reloading the shell alone does not update the running daemon. Update the native plugin, then use its explicit service update action to apply these backend changes. An older marketplace snapshot does not verify the newer upstream code.

Backend **0.2.5** adds the native folder-picker mode. The panel no longer creates an editable download-path field, including when Settings is collapsed. Updating the panel removes that shared-shell clipboard-reading path immediately; **Update service** supplies folder selection. Older backends can continue their existing supported transfer actions, but the new folder button stays disabled until directory-picker support is reported.

Backend **0.3.1** introduced a visibility-owned advertisement daemon and 60-second recycling. That bounded retention lifetime, not peak storage under a DNS flood. **0.3.5 supersedes that mitigation:** both browser and advertisement daemons enforce intrinsic raw-record/charged-byte and derived-work ceilings; visible advertisements no longer recycle periodically, while invisible actors still own no listener. Browser re-browse remains only for observation freshness. The shared TCP listener, active transfers, endpoint IDs, LAN priority and QR authentication are unchanged.

Backend **0.3.2** shares a **128-address / 45-second** lease across Bluetooth observations, legacy-header results and peers, replacing retained per-address subscriptions. Before each new automatic GATT connection it checks the live adapter inventory, including BlueZ-retained objects, and requires fewer than **128 Device1 objects**. It protects paired/bonded/trusted/blocked or already-connected devices and does not remove devices or delete BlueZ storage. This limits the application's automatic admission, not global BlueZ memory/disk or concurrent activity by other clients. Both the native plugin update and explicit **Update service** action are needed; shell reload alone does not apply this fix.

Backend/client **0.3.4** open each selected regular file once, reject symlinks throughout its path, and retain the descriptor through normal queues, transport fallback and pending QR discovery. QR claim does not reopen the path. Linux `openat2` must be available and permitted; failures do not fall back to following paths. Choose real target files/folders rather than symlink aliases. Updating QML alone cannot apply this fix to a running **0.3.3 or older** daemon.

Backend **0.3.5** vendors the compatible mDNS fork locally with its original licenses and modification notice. Each daemon admits at most **4,096 raw DNS records / 1 MiB charged cache storage**, including unrelated names, with bounded resolution/retry state and DNS decoding. Charged storage is not an exact process-RSS/allocator ceiling; see the [resource boundary](ARCHITECTURE.md#mdns-resource-boundary). Existing-entry TTL renewal works at capacity, excess new records are dropped, and discovery under saturation is best effort. No new dependency or privilege grant is required. Update the backend executable: older daemons do not gain these limits from a shell reload.

If Omarchy keeps a previously loaded component after a plugin update, use `omarchy restart shell` before opening its updated controls. Shell reload does not itself cancel the independent daemon's transfers, but service maintenance still requires them to end first.

## Remove

1. Finish or cancel transfers.
2. Open **Settings** and select **Remove service** in the **Service** section.
3. Read and confirm the setup terminal's removal plan.
4. Remove the plugin with `omarchy plugin remove macondo.quickshare`.

The backend remover handles only owned executable/unit/integration resources and preserves received files and project configuration. For a system package, the terminal explicitly asks whether to remove the package or retain it with the user's service disabled. Retaining a package is reported as retained, not as removal of its system files. Shared dependencies are not automatically removed.

Native Omarchy removal owns the widget's bar entry and inline shell settings. The language preference is also retained in the project's user configuration so reinstall can restore English/Español without editing unrelated shell entries.

## Ownership, data and troubleshooting

Setup records ownership outside the disposable Git plugin checkout and refuses unrelated destination collisions. Updates/removal stop at an active-transfer guard rather than interrupt someone else's files. Backend configuration, language and received files survive maintenance; deleting personal data is a separate user decision.

### First installation: “Unit omarchy-quickshare.service not loaded”

Versions through **0.2.5** could compile successfully, then abort before creating the user unit because maintenance tried to stop a unit that did not exist yet. **0.2.6** skips stopping only when systemd reports `LoadState=not-found`; failures stopping an existing unit still abort maintenance.

Update the native plugin, reopen its panel and choose **Install service** again:

```bash
omarchy plugin update macondo.quickshare
```

If the panel retains the previous version, run `omarchy restart shell` before reopening it. On a first installation, use **Install service**, not **Update service**: the source backend may not have been installed yet. The compiler's deprecated-field warnings are separate from this failure.

No uninstall, manual unit creation or removal of preferences/downloads is needed. If you applied a local workaround, preserve it before updating a modified Git checkout; the upstream fix already includes the absent-unit guard.


Useful diagnostics:

```bash
systemctl --user status omarchy-quickshare.service
journalctl --user -u omarchy-quickshare.service
omarchy-quickshare doctor
omarchy-quickshare status
```

Do not launch a second daemon with the same session bus name. When troubleshooting a retained/disabled service, use the panel's install action to restore the maintained service rather than bypassing its ownership state.

Redact paths, device addresses, SSIDs, credentials and verification codes before reporting errors. Private security reports belong in the repository's [Security](../SECURITY.md) channel.

## Marketplace packaging

The repository root supplies the manifest, README, license and `preview.png`. The plugin keeps the stable `macondo.quickshare` ID across installation and updates. Follow the [official publishing guide](https://plugins.omarchy.org/publish.html) and [submission contract](https://github.com/omacom/omarchy-plugin-marketplace/blob/main/SUBMISSION.md).

The listing's setup classification must describe the separate backend setup accurately. Marketplace compatibility validation and any automated baseline are not a security audit or endorsement; a curator controls listing approval.

## Existing copied-plugin installations

An older setup may have copied QML into the plugin directory instead of leaving a Git checkout. Native `omarchy plugin update` cannot update such a directory.

Finish transfers before migrating. Preserve your download folder and project configuration. If the old language preference exists only in the shell entry, save it before native removal:

```bash
config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
mkdir -p "$config_home/omarchy-quickshare"
language=$(jq -r '[((.bar.layout.left // []) + (.bar.layout.center // []) + (.bar.layout.right // []) + (.plugins // []))[] | select(.id == "macondo.quickshare") | .language | select(. == "en" or . == "es")][0] // "en"' \
  "$HOME/.config/omarchy/shell.json") &&
printf '%s\n' "$language" > "$config_home/omarchy-quickshare/language" &&
omarchy plugin remove macondo.quickshare &&
omarchy plugin add https://github.com/PuroDelphi/omarchy-quickshare.git --enable
```

Omarchy backs up a non-Git plugin directory when removing it. The native commands do not delete your Quick Share download/configuration folders. Open the new panel and use **Install service** to complete the backend upgrade and registration; an older system package requires the terminal's explicit authorization.

Native plugin and shell-registry locations follow **`$HOME/.config/omarchy`**, including when backend XDG paths are customized. Backend configuration, source/notices and maintenance receipts use `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `XDG_STATE_HOME`, respectively.

For terminal-only maintenance, the panel runs this same helper:

```bash
bash "$HOME/.config/omarchy/plugins/macondo.quickshare/scripts/setup-service.sh" install en
```

Actions are `install`, `update` and `remove`; language can be `en` or `es`. This is backend maintenance, not another plugin installer. Source builds use Arch's `/usr/bin/cargo` and `/usr/bin/rustc` so an inactive toolchain shim does not select a different compiler; the owned build cache is outside the watched plugin checkout.

Package builds use the user's cache rather than memory-backed `/tmp` by default and remove their temporary work directory when finished. An explicit `TMPDIR` takes precedence; choose a filesystem with enough space for the Rust build.

## Stable 0.3.5 and matching backend

Stable **0.3.5** on `main`, the channel used for the marketplace submission, fixes raw-DNS retention and derived scheduling while retaining the outgoing-file descriptor correction, contrast-aware bar icon, funding footer, native QR and Bluetooth limits. Update through the [native lifecycle](#update), finish or cancel transfers, then choose **Settings → Service → Update service** to replace the backend/client executable and review the requested changes. Use **Install service** if backend setup has never completed. A shell reload alone does not apply these security corrections.

The plugin checks optional backend capabilities rather than assuming a QML checkout replaced the daemon. Check the panel's service information for the installed backend version. Stable application/source version and Arch `pkgver` are all **0.3.5**; there is no prerelease `_sourcever` distinction. Apply the explicit backend update even when a newer panel already displays 0.3.5.

**Post-0.3.5 source correction:** `main` also fixes empty UDP datagrams disabling mDNS reception; see the [recorded regression and real-socket smoke](VERIFICATION.md#empty-udp-datagrams-and-mdns-reception--post-035-correction). The original v0.3.5 release assets predate this fix. The version string alone does not identify it: the backend must be rebuilt from the fixing commit or later. With that native plugin checkout selected, **Update service** builds its current source in both user-executable and system-package modes; it does not download the old release executable. Finish/cancel transfers and authorize the terminal's requested changes as usual.

## Returning an existing alpha checkout to main

Stable **0.3.0** promotes the QR flow and lower-left companion as well as the prior fixes. An existing native installation checked out on `alpha` can return to `main` through the same native plugin lifecycle. Preserve local modifications by committing them or making a separate backup before switching:

```bash
plugin="$HOME/.config/omarchy/plugins/macondo.quickshare"
git -C "$plugin" fetch origin main
git -C "$plugin" switch main
omarchy plugin update macondo.quickshare
omarchy restart shell
```

These commands do not force a checkout or discard modifications; if Git reports conflicting or divergent local changes, preserve and resolve them before continuing rather than resetting the checkout. A plugin update requires a checkout that can advance safely to upstream `main`.

The plugin ID remains `macondo.quickshare`. Configuration, language, the saved transfer-effects choice, download destination and received files remain in their existing locations. Older supported backends do not gain DNS limits or outgoing-file corrections from a shell reload. To install stable **0.3.5**, finish or cancel transfers, choose **Update service** and review the setup terminal's requested changes. Use **Install service** if backend setup never completed.
