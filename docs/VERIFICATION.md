# Recorded verification

[Back to Quick Share](../README.md) · [User guide](USER_GUIDE.md) · [Architecture](ARCHITECTURE.md)

This record describes observed native Android and desktop behavior. Radio/backend availability and local simulated peers are distinguished from real file transfers. It contains no verification codes, credentials, private device identifiers or personal paths.

## Native file sharing

Recorded desktop environment: Omarchy 4.0.4, Quickshell 0.3.1, BlueZ 5.87 and Linux 7.2.5. The native Android peer was a Pixel-family phone using its installed Quick Share. Numeric Android versions are diagnostic metadata, not an admission rule.

| Case | Direction and transport | Observed outcome |
| --- | --- | --- |
| Small files and Unicode names | Omarchy ↔ Android, LAN | Native completion; returned content/names and hashes matched |
| Multi-file selection | Omarchy ↔ Android, LAN | Native grouped files; safe duplicate-name prefixes preserved original PC files |
| Large file | Omarchy ↔ Android, LAN | 67,108,864 bytes each direction; SHA-256/byte comparison matched; Android stored copy independently read over USB/MTP |
| Active shell reload | Omarchy → Android, LAN | 64 MiB completed without replacing the daemon instance; destination matched source |
| Native video receive | Android → Omarchy, Bluetooth | 44,501,859 bytes completed; Classic RFCOMM observed; full received video/audio decoding succeeded |
| Native photo receive | Android → Omarchy, Bluetooth | 392,114 bytes completed; independent Android SHA-256 matched |
| Native text batch | Android → Omarchy, Bluetooth | Two files, 309 bytes, completed; user confirmed correct content |
| Both Wi-Fi radios off, large send | Omarchy → Android, Bluetooth | 67,108,864 bytes completed, no error; new Android copy independently matched SHA-256 and bytes |
| Both Wi-Fi radios off, large receive | Android → Omarchy, Bluetooth | 67,108,864 bytes completed; new `1_grande-64MiB.bin` matched independently read Android source and preserved the PC original |
| Incoming active-data cancellation | Android → Omarchy, Bluetooth | Cancelled at 574,200 of 7,561,800 bytes; no published video or leftover local staging |
| Incoming physical Bluetooth cutoff | Android → Omarchy, Bluetooth | Failed at 2,035,440 of 36,372,062 bytes with `Connection reset by peer (os error 104)`; no publication or leftover local staging |

The independently verified 64 MiB payload SHA-256 is `b9e838f0fa17dd4b381a64b6ccc2056d9956e4f0046048f10abb58186cde9b8e`. Source/destination comparisons returned `cmp` exit 0.

For the both-Wi-Fi-off cases, the phone's radio state was confirmed by the user and the computer's off state was recorded at completion. The engine ran `serve --no-lan --no-hotspot`; USB provided Internet for coordination, not Quick Share file data. The new Android receive's timestamp followed that transfer's completion rather than identifying an earlier LAN copy.

For the physical incoming Bluetooth cutoff, the user confirmed disabling phone Bluetooth while data was arriving. The terminal snapshot, nonzero partial byte count, isolated execution and filesystem checks are the evidence; a continuous pre-cut observer and a new HCI capture were not used for that attempt. Acceptance was not automated. Explicit PIN comparison is required by the usage guide but is not claimed as independently observed for every recorded run.

## Desktop integration

- A native incoming request produced a notification with the panel closed; the user handled consent.
- Nautilus single/multi-file selections preloaded the native panel and completed real sends. Opening the menu did not automatically send or accept.
- A sole discovered destination was preselected; multiple destinations and disappearing manual selections were exercised without redirecting a selection.
- English/Español, theme-integrated controls and real QML/Wayland rendering were exercised. Progress cards retain their delegates by transfer ID so pointer presses are not discarded when snapshots update.
- Incoming rejection and cancellation did not publish files. Local staged files were removed by storage rollback, not by manually deleting evidence.

## Service and installation observations

Source/package setup, package update, service removal/reinstallation and automatic startup after a real reboot/new graphical login were exercised. Configuration and recorded received-file metadata were preserved. Service startup was observed before invoking a D-Bus client, avoiding a false autostart result caused by client activation.

After isolated Bluetooth checks, the owned runtime overrides were removed, normal installed execution restored, all three backends returned ready with no engine errors, visibility was off, and computer-Wi-Fi-bound HTTPS returned 200. Original/reference files and the configuration hash remained intact.

Local NetworkManager checks created an AP and restored the previous connection while removing owned profiles. This observation describes local network-resource management, not a separate Android transfer result.

## Native Git lifecycle and service controls — 0.1.1

The native Omarchy add/enable, fast-forward update and remove commands were exercised against a local Git origin and the actual SDK plugin registry. The checkout retained `.git` through backend maintenance; native removal unloaded the widget and removed its checkout.

Source service installation, repeated installation, update and removal ran against the real 0.1.1 executable and a private session bus. Executable, unit/drop-in, source, notices and Nautilus registration were inspected. Foreign or modified resources were refused without replacement or stopping the running daemon. Declined removal was inert; authorized removal removed owned resources while preserving configuration, language and download witnesses.

The actual QML panel was rendered in English/Español with missing, older, compatible and disabled backends. An older 0.1.0 executable displayed **Service update required**, not **Ready**. Spanish persisted through native removal/re-addition and component reload even after the inline shell entry had been removed. Maintenance controls and scrolling were inspected at 1366×768.

The 0.1.1-1 Arch package was built using the disk-backed temporary workspace, extracted and executed on a private D-Bus session. Its embedded corresponding source matched the separately generated source archive byte-for-byte; licensing and detailed documentation were present. Obsolete copied-plugin setup/removal wrappers were absent.

These local lifecycle scenarios used isolated homes and network namespaces. Process-control adapters managed real daemons without modifying the host systemd manager; package authorization boundaries used adapters rather than host package changes. Isolated NetworkManager errors remained visible. The observations establish lifecycle and UI behavior, not a new Android transfer or transport-readiness result.

## Private file-selection handoff — 0.1.2

The locked workspace build and deterministic tests completed with **74 tests passed** and one consenting physical SDP test ignored. Regression cases cover stdin JSON type/count/byte limits, rejected positional file-path arguments, isolated one-shot selections, monotonic expiry, capacity and preparation without transfer actions.

An isolated home, session D-Bus and network namespace exercised the actual Rust client/daemon, Nautilus provider activation with `Gio.Subprocess`, Quickshell processes and native QML/Wayland panel. Unicode, quotation marks and newline-containing names arrived as exact canonical paths through stdin and the token handoff. Send and destination-folder commands reached the real D-Bus methods; an unavailable synthetic peer was rejected without starting a transfer. **369 process command-line samples** contained no selected path/name, including JSON-escaped forms.

Single/multiple synthetic device models retained their selection rules. Delayed process adapters exercised overlapping imports, stale picker results, picker cancellation and an earlier send completing after a newer selection; they are not file-transfer evidence. Invalid tokens cleared stale selections. Shell spawn, nonzero exit, invalid UTF-8 and unexpected replies discarded prepared selections and preserved all available slots.

The new panel was also rendered against the actually installed 0.1.1 executable on a private bus: it showed **Service update required**, rejected file-selection retrieval with an update message and kept an empty selection without starting a transfer.

No phone transfer, global procfs permission change, production-service maintenance or preview/screenshot modification was part of these privacy checks. The same-user session and root remain trusted boundaries, not a process sandbox.

## Historical alpha layer-shell presentation

Before promotion to stable **v0.2.0**, the alpha QML service/panel and new effects modules were exercised in Quickshell on Wayland with private preferences and UI-only synthetic activities. All three visual stages were exercised with LAN, Bluetooth, hotspot and unconfirmed transport. Captured rendered frames changed; confirmed LAN/Bluetooth motifs differed in each stage.

That alpha run's layer-shell trace showed the `omarchy-quickshare-alpha-effects` surface at overlay layer with keyboard interactivity `0` and an explicitly empty input region. Native toggle keyboard activation still worked underneath it. Halo textures excluded the card and safety margin; closing the panel changed to the anchored compact courier. This records the historical alpha namespace, not a trace of the promoted stable surface.

Default-off, atomic preference persistence, reload without replay, eight-second preview expiry, concurrent directions/media, stable transfer identity through progress, cancellation stopping, disconnect/terminal cleanup and immediate opt-out were exercised. Previews did not send files, accept verification codes, mutate real snapshots or change network consent. These presentation checks do not establish new Android-transfer results.

## LE bearer coordination — 0.2.1

The ordinary-user read-only HCI connection query succeeded on the recorded Linux host with UID 1000 and no added Bluetooth capabilities. The native ABI probe observed 16-byte connection entries and a four-byte list header. The corrected daemon also started under the existing service's `NoNewPrivileges=yes` restriction.

The locked workspace suite passed **75 tests**, with the consenting physical SDP regression ignored. New behavior regressions cover a live LE bearer without any metadata read, multiple/disconnecting LE bearers, release while a Classic audio ACL remains connected, and refusing a potentially truncated connection snapshot.

A live 45-second visibility cycle exercised the corrected daemon's real BlueZ advertisement/scan schedule, expired normally, retained Bluetooth availability and reported no engine errors. Network consent stayed off and the configuration checksum was unchanged. No LE peer connected during this local smoke; it is not evidence of a new Android file transfer or a universal Realtek firmware fix.

## Existing-LAN priority — 0.2.2 alpha

The locked workspace suite passed **81 tests**, with one consenting physical SDP regression ignored. Consumer-visible regressions cover active physical-interface eligibility, IPv4/subnet boundaries, link health, replacement-channel key proof, withholding file bytes before that proof, and refusing Bluetooth rollback after old-channel drain.

A private runtime peer exercised the real session/UKEY2/sharing implementation. Its bootstrap emulated RFCOMM with a duplex stream; its replacement used actual kernel TCP sockets. The production selector offered the active physical Wi-Fi address on a real interface-bound port without changing the NetworkManager snapshot. A pre-drain failure retained the same bootstrap channel and committed the exact received file. TCP cutover preserved cipher sequences and committed 57 bytes; unrelated old-channel keys, wrong new keys, plaintext and replayed records were rejected without false LAN reporting or file publication. Post-`LAST_WRITE` failure did not send fallback acceptance.

The positive TCP candidate was explicitly staged on loopback because production admission rejects self-address peers. This proves engine negotiation/authentication behavior and actual TCP data handling, not a new native Android or Bluetooth-radio transfer. Temporary runtime modules and runners were removed after observation.

## Shared LAN listener — 0.2.3 alpha

The locked suite passed **91 tests**, with the consenting physical SDP regression ignored. Additional regressions exercise duplicate/closed admissions, queue bounds, ownership-safe removal, immutable candidate selection, canceled partial-frame reads, visibility expiry, preserved back-to-back connection/UKEY2 frames, and routing upgrades when file-session capacity is saturated.

Eight private runtime scenarios passed under **NoNewPrivileges**. The complete Manager actor used one owned TCP listener: a registered introduction still routed while receive visibility was off, its following encrypted record remained intact, and no new transfer event, consent or receive file was produced by routing. An invisible ordinary request was closed; a visible connection request followed by UKEY2 ClientInit in one write yielded real ServerInit and exactly one connection-request event.

The production selector advertised all eligible physical addresses at the existing host port without opening per-upgrade listeners or changing its NetworkManager snapshot. The authentication, pre-drain fallback, 57-byte TCP file, wrong-key/plaintext/replay rejection and irreversible-drain scenarios also passed. Probe listeners used private test ports, not the installed service's live port **47593**; positive remote admission was staged on loopback and Bluetooth bootstrap was emulated. These observations do not certify a new native Android migration or external firewall reachability. Temporary modules and runners were removed after the smoke.

## Bounded discovery lifecycle — 0.2.4

The locked workspace suite passed **111 tests**, with the consenting physical SDP regression ignored. Added behavior regressions cover address/port replacement, route preferences/fallbacks after replacement and expiry, old-owner withdrawals, monotonic lease boundaries and renewal, count pressure, invisible snapshots, stale Send refusal and discovery-channel loss.

A private executable exercised production mDNS record decoding, real TCP connections on loopback, the source peer cache, aggregate peer/routing state and actual daemon Snapshot JSON under **NoNewPrivileges**. It rotated one service through **512 address/port changes**, introduced **160 distinct reachable peers**, and observed **1,344 snapshots** with visibility off. No snapshot exceeded **128 devices**; withdrawal and explicit monotonic expiry left zero retained devices or routes. Private lease fields were absent from JSON and no transfer was created.

These records were synthetic and lease deadlines were explicit test instants, not a claimed 120-second wall-clock wait or native Android run. No multicast advertisement, radio change, network configuration change or host-service update was performed. The temporary runtime modules and executable were removed after observation.

## Historical companion placement — alpha

The closed-panel companion was moved to the lower-left corner without changing artwork, notification logic, activity captions, input handling or the panel halo. The actual Quickshell layer-shell component was exercised both from the installed checkout and the alpha source. Sending over LAN, receiving over Bluetooth and preparation remained visible with bars at the top, left, bottom and right; the overlay stayed inside the screen, clear of the bar and input-transparent. Both checkouts produced identical settled geometry, and disabling effects hid the surface.

A native Omarchy toast and the companion were observed together in a live desktop capture: the toast remained legible in the upper-right corner and the companion occupied the lower-left. Panel-open projection retained its protected card/safety rectangle. The local plugin was rescanned through the native shell command; no backend restart, file transfer, radio operation or marketplace update was performed.

## Historical native QR sending — 0.3.0-alpha.1

The locked workspace suite passed **124 tests** with the consenting physical SDP regression ignored. New regressions cover published HKDF values, fixed-02 key reconstruction, visible/hidden metadata, malformed/duplicate TLVs, wrong keys/tampering, monotonic expiry, owner-safe cancellation, one-time claim, direct routing, UKEY2-bound P1363 proof and preservation of both consent gates.

An independent Python protobuf/cryptography receiver ran across a private veth pair in separate user/network namespaces, with a private HOME and session bus. The actual Rust daemon used its real mDNS discovery, TCP probe, QR matching channel, command actor and session engine under NoNewPrivileges. Both visible-token and hidden AES-GCM-name receivers obtained **two files**, independently hashed to the original SHA-256 values. The receiver verified the actual wire QR signature using the public key reconstructed from the URL. Remote ACCEPT alone yielded no file bytes during the observation interval; explicit local verification then allowed transfer completion. Pending QR replacement and stale-owner cancellation were also exercised while receive visibility remained off. Host interfaces, radio state, firewall and network profiles were not changed.

The actual Panel.qml and Service.qml ran against a real private QR backend. Showing QR without a selected peer, file count, close-during-start, selection-change withdrawal and panel-close withdrawal were exercised. The Spanish panel's rendered QR was decoded with an independent barcode reader; English live labels were inspected. Later capture attempts encountered the host display locked/DPMS off; it was not woken or unlocked.

These are source/runtime and independent-peer interoperability observations, not a claim of a new physical Android QR scan. No host backend was installed or restarted and no marketplace update was requested. Temporary peers and UI/backend processes were removed after observation.

## Native download-folder selection — 0.2.5

The editable download-path control was removed rather than hidden or made read-only. Runtime inspection of the real Panel.qml and native Omarchy components found no TextInput, TextEdit or TextField descendants with Settings collapsed or expanded. The replacement path label and folder action were rendered in English and Spanish.

The real Rust client opened the desktop's GTK folder chooser with directory selection and single-selection mode. Controlled portal responses on a private D-Bus session then exercised the real client, QML command completion and daemon: accepted folders, a Unicode/quoted folder name and explicit cancellation. The selected directory was persisted exactly; cancellation preserved it; no selected path appeared in the client command argument log. No transfer was created. Native dialog rendering and the controlled-response assertions are separate observations; native acceptance is not claimed from the response adapter.

Folder selection runs in the Rust client/desktop portal outside the shared Quickshell process. These checks did not replace the user's clipboard, run an unbounded clipboard payload, change the installed backend or modify radio/network configuration.

## First-install service lifecycle — 0.2.6

The maintenance helper now distinguishes `LoadState=not-found` from existing loaded/masked units. Five shell regressions exercise installation continuation for an absent unit, an absent-unit query with a nonzero status, successful existing-unit maintenance, and loaded/masked stop failures that must prevent replacement. CI runs these regressions without a host service manager.

A private HOME/session bus exercised the published ownership, pause and unit-installation helpers. With no user unit present, source-mode registration continued, created owned unit/drop-in and Nautilus registration, enabled its fixture target and started the actual Rust daemon under NoNewPrivileges. Its real v1 status had no transfers or errors. A second pause used the existing-unit path and shut the daemon down cleanly. Only manager/process control and package-mode detection used adapters; this is not a claim of a new host systemd installation or a new source compilation. The prebuilt 0.2.5 daemon was used because the correction is in the shell lifecycle, not the transfer engine.

The user's installed service, preferences, radio/network configuration and received files were not modified by this lifecycle smoke.

## Stable 0.3.0 promotion scope

Stable **0.3.0** brings the native LAN QR invitation flow and lower-left companion into `main`, retaining the bounded-discovery, private native-folder-picker and first-install lifecycle fixes described above. The alpha companion and QR observations remain historical evidence from those builds.

The integrated **0.3.0** workspace built with the locked dependency graph and passed **124 Rust tests**, with the consenting physical SDP regression ignored. All five shell lifecycle regressions, package-script syntax checks and native plugin validation passed.

A fresh independent Python protobuf/cryptography receiver exercised the actual **0.3.0** Rust daemon across a private veth pair in separate user/network namespaces, with a private HOME and session bus and **NoNewPrivileges**. Four completed transfers covered visible-token and hidden AES-GCM-name receivers with both consent orders. Each delivered two files with independently matching SHA-256 hashes and verified the UKEY2-bound P1363 QR signature against the public key reconstructed from the URL. No file payload arrived during the 450 ms observation while the remaining local or remote consent was withheld. Replacement and owner-safe withdrawal passed; selected paths were absent from the QR view and receive visibility stayed off.

These fresh checks establish independent-peer behavior, not a physical Android QR scan. No installed host backend, preferences, received files, radio settings or network configuration were modified.

## Advertisement daemon lifecycle — 0.3.1

The locked workspace built and passed **125 Rust tests**, with the consenting physical SDP regression ignored. The new lifecycle regression checks that loss of the visibility owner terminates a waiting hidden advertiser. All five service-maintenance regressions, package-script syntax checks and native plugin validation passed.

A throwaway executable included the actual production `mdns.rs` and metadata utilities, using the pinned mDNS library and the unchanged 120-second LAN lease constant. In private user/network namespaces, its hidden constructor and running hidden actor owned zero advertisement-daemon threads or UDP sockets. An independent peer resolved the authentic PTR/SRV/TXT/A endpoint before rotation, after rotation and after showing again.

The peer injected **384 unique unsolicited instances / 1,536 records** with 4,500-second TTLs. A fresh native daemon and new UDP socket inodes replaced the first generation at approximately **60.14 seconds**, despite repeated Visible updates. Hiding, visible cancellation and visibility-owner closure in visible/hidden states released the advertisement daemon and sockets. A separate real TCP connection remained open across these transitions and echoed **1,441,792 bytes** with identical independent SHA-256 hashes.

This verifies actual advertisement resources and a separate TCP stream, not a Quick Share protocol transfer or a new physical Android run. Finite resource sampling does not exclude sub-sample thread epilogues; the code waits for shutdown acknowledgment, which the pinned library sends after dropping its cache and sockets. Generation retirement bounds cache retention lifetime, not absolute memory bytes within a generation. No installed host service, network/radio configuration, preferences or received files were changed. Owned processes were reaped.

## Bluetooth discovery retention — 0.3.2

The locked workspace built and passed **131 Rust tests**, with the consenting physical SDP regression ignored. All five service-maintenance regressions, package-script syntax checks and native plugin validation passed. New regressions cover rotating-address eviction and header retirement, shared-route withdrawal, independent radio/peer freshness, protected-state/count admission, malformed signal bounds and same-address Classic protection.

A private executable called the actual production `RQS::with_options`, `run`, `discovery` and `stop` APIs with Bluetooth enabled and LAN/hotspot disabled. A private D-Bus fixture modeled BlueZ 5.87 public/static-random promotion and retained objects; a process-only HCI adapter returned an explicitly empty connection snapshot and rejected all other Bluetooth sockets. **325 recorded-evidence assertions passed** across retention, failure/timeout, cancellation, owner replacement, emerging protection and focused expiry/admission scenarios.

The run delivered **8,192 ServiceData reports** and retained **389 modeled devices** across an application restart. Actual private-bus match rules stayed at **7 baseline**, with at most **8** during a transient confirmation, and zero after shutdown; these are bus-resource observations, not an introspection counter for BlueR's internal registry. Command dispatch continued after the flood, with the first recorded command **6.8 ms** after burst emission completed. Restart over the admission threshold caused zero new Connect calls. In a stable inventory, **127 objects allowed a connection and 128 denied further attempts**, including after restart.

Without DeviceRemoved, duplicate legacy headers remained cached, an unrefreshed peer withdrew at **50.008 seconds** on the expiry cadence, and the identical header was read again at **52.090 seconds**. Paired, bonded-but-unpaired, trusted, blocked and preconnected fixtures received no Connect/Disconnect; preconnected metadata remained readable. Five owned error/timeout connections received five cleanup Disconnects, with timeout cleanup at **9.991 seconds** and no same-header retries. Cancellation released its owned bearer. When bonding emerged during a pending read, cleanup instead preserved the connection, reported the explicit protected-state failure and set availability false. Owner replacement also failed closed at the five-second health check without Connect/Disconnect against the replacement owner.

Two concurrent-injection observations had a census of **127**, followed by a Connect-time count of **128** 57/84 ms later. They complied with the snapshot admission policy, not an atomic global bound; the limitation is retained rather than discarded from the evidence. These are production-engine behavior checks against a modeled BlueZ contract, not native bluetoothd, physical HCI/ATT, Android interoperability or a new file transfer. No installed service, host Bluetooth/network state, preferences or received files were changed.

## Transparent-bar contrast and alpha promotion — 0.3.3

PR [#2](https://github.com/PuroDelphi/omarchy-quickshare/pull/2), contributed by **OverStyleFR**, changes only the normal bar glyph foreground from `bar.foreground` to Omarchy's contrast-aware `bar.barForeground`. The installed SDK exposes this property through `Ui.PluginBarApi` and already uses it in native `Ui.WidgetButton`. The PR adds no commands, dependencies, permissions, network access or file access.

The actual `BarWidget.qml`, native `Ui.BarIconButton`/`Ui.PluginBarApi` and theme singletons rendered in an isolated Quickshell configuration. Eight observed states covered opaque bars, transparent bars over light/dark surfaces, incoming requests, outgoing PIN confirmation, disconnected-service dimming, a vertical bar and the no-bar fallback. Captured glyphs used the expected contrast/urgent colors, remained unframed and retained offline opacity. This was offscreen software rendering with controlled presentation/state fixtures, not a capture of the user's desktop bar: native Wayland capture was unavailable.

Stable security fixes were merged into alpha before PR integration. Promotion also retains alpha's repository-local GitHub Sponsors/PayPal funding configuration, plain README footer and original PayPal QR, without donation badges. `zbarimg` decoded the QR to the configured PayPal hosted-button URL. No host bar/theme settings, backend service, radio/network configuration or received files were changed.

The integrated **0.3.3** locked workspace built and passed **131 Rust tests**, with one consenting physical SDP regression ignored. All five service-maintenance regressions, package/builder syntax checks and native plugin validation passed. No new physical phone transfer is claimed for this presentation/funding update.

## Pinned outgoing-file ownership — 0.3.4

The integrated workspace built and passed **139 Rust tests**, with one consenting physical SDP regression ignored, plus all five service-maintenance regressions and package/builder syntax checks. Security regressions cover final/ancestor symlink rejection, original-inode reads after rename/unlink/replacement, special-file/FIFO rejection, partial-preparation descriptor retirement, captured-size growth/truncation behavior and aggregate QR selection boundaries without replacing a valid pending offer.

The actual new CLI/daemon and a public-RQS receiver ran as UID 1000 with **NoNewPrivileges**, private HOME/D-Bus/user namespaces and two separate private network namespaces joined by veth. Genuine mDNS discovery found the receiver; a process-only connect interposer held the sender's TCP setup after file preparation/queueing. While the sender still reported Connecting, the controller replaced selected leaf paths and a parent directory with symlinks to private mode-0600 fixture files. The compared PINs matched, no file was created before consent, both real consent gates were exercised and both sessions completed.

The receiver committed **three files / 196,892 bytes**, including a multi-chunk binary and an empty file, matching the original selections byte-for-byte and by independent SHA-256 rather than either private substitute. The three selected descriptors remained owned during the held connection and retired after completion. Initial CLI symlinks were rejected; replacing a pending QR retired its old descriptor, stopping an old invitation preserved the new one, and stopping the current invitation released its descriptor. Both owned processes exited cleanly with no engine errors.

This is an actual encrypted production-engine transfer over private kernel TCP/filesystems, not a native Android transfer or a separate-UID attacker execution. The path-mutating controller was a separate process with the same test UID; different-UID filesystem substitution is the reported threat model, not newly reproduced here. NetworkManager capability/inventory reads were explicitly adapted, and the connect gate was only a private scheduling fixture. No installed host service/plugin, user files, physical radios or host network configuration were changed. In-place content mutation by existing writers is not prevented by inode pinning.

## Evidence interpretation

Successful transfer means engine completion plus the stated integrity method. A valid media container, a round trip, a capability label or 0/0 bytes alone is not independent destination integrity.

Android owns its receive/cache lifecycle. Omarchy's incoming staging rollback is local; sender-side cancellation does not establish remote-file deletion. Empty native offers can be rejected by Android. These operational behaviors are described in the [user guide](USER_GUIDE.md) and [architecture](ARCHITECTURE.md), not disguised with modified payloads.

A camera original read through USB and an Android share-provider export can differ in location metadata despite matching media data. The comparison is not presented as complete-file equality, and private coordinates are not published or reconstructed.

Locked-source checks run in [GitHub Actions](https://github.com/PuroDelphi/omarchy-quickshare/actions). Verification results are scoped observations, not an external security certification or a claim that every radio/platform combination behaves identically.
