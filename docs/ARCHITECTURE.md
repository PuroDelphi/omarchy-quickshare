# Architecture and command reference

[Back to Quick Share](../README.md) · [User guide](USER_GUIDE.md) · [Installation](INSTALLATION.md)

## Components

| Component | Responsibility |
| --- | --- |
| Rust/Tokio service | Discovery, connection negotiation, authentication, encryption, file streaming and transfer state |
| Omarchy QML plugin | Bar visibility, nearby devices, file selection, consent, progress, cancellation and service maintenance |
| Session D-Bus and CLI | Control commands and versioned snapshots between the independent daemon and UI |
| Nautilus extension | Preload one or multiple local files into the panel without automatically sending |
| BlueZ | BLE/GATT discovery metadata and Classic RFCOMM data sockets |
| NetworkManager | Local network information and consent-controlled temporary network profiles |

The daemon continues independently of the panel. A shell reload reconnects the UI to the daemon snapshot instead of creating a second transfer engine. File content and cryptographic keys do not pass through QML or D-Bus.

## Session interface

- Service: `io.omarchy.QuickShare`
- Object: `/io/omarchy/QuickShare`
- Interface: `io.omarchy.QuickShare1`
- Stable plugin ID: `macondo.quickshare`

Snapshots carry an instance ID, monotonic revision, visibility/deadline, backend availability, nearby devices, transfer IDs/states, bytes, transport and errors. The UI ignores stale revisions and recovers after service ownership changes.

A transfer's medium and terminal result come from engine state, not a UI capability label. Send/accept/reject/cancel operate on the actual peer or transfer IDs.

Selected file paths travel through private stdin pipes and the user's session D-Bus, not subprocess arguments. `PrepareShare` holds a selection in daemon memory and returns an opaque token. The shell summon carries only that token; the panel retrieves the paths with `TakeShare`, which consumes the selection once. Pending selections expire after five minutes and are limited to 16. Opening a selection does not start a transfer. This boundary protects against other local users reading process command lines; it does not isolate the application from root or other processes in the same user session.

Since backend **0.3.4**, Send/StartQrSend securely open regular source files once, before connection setup or QR publication, using Linux `openat2(RESOLVE_NO_SYMLINKS)` with read-only, close-on-exec, no-follow and nonblocking flags. The complete selected path must be free of symlinks, including ancestor folders and procfs magic links. The CLI preserves the selected pathname rather than canonicalizing links away; the Nautilus handoff only preloads paths and does not grant a byte source. An unavailable/denied secure-open syscall fails closed, without a weaker fallback.

The move-only `OutboundFile` owns that descriptor and its captured validated basename/size. Normal queues, preferred/fallback connection attempts and pending QR offers move ownership; QR claim consumes the original offer without reopening/re-preparing it. Session consumes those descriptors and derives protocol metadata from the captured names/sizes. No selected pathname is retained in the outbound payload for later lookup, no descriptor is cloned for history, and file handles are not serializable. Partial preparation errors, terminal queue/connection failures, cancellation/shutdown, QR replacement/withdrawal/expiry and session destruction release their owned handles. UI history keeps display names only.

Service selections remain limited to **1–1,000 files**, with the shared **1 TiB aggregate limit** checked before enqueue/QR publication. Descriptor availability can cause preparation to fail; partial ownership is dropped on that error. An open descriptor pins the inode, not immutable contents against existing writers. Pumping reads no more than the captured size; growth cannot append unapproved bytes and premature EOF is an error. The fix does not copy, hash or freeze entire source files.

Stable **0.3.0** includes `StartQrSend(files)` and ownership-safe `StopQrSend(invitationId)`. The CLI equivalents are `qr-send` (JSON paths on stdin) and `qr-stop ID` (opaque ID only). Snapshots add `qr_send_supported` and an optional `qr_send` view containing ID, public URL, SVG data URI, expiry, count and total size. The private signing key and, since 0.3.4, pending file descriptors are excluded from serialization.

## Native QR sending

The wire format follows [NearDrop's documented protocol and implementation](https://github.com/grishka/NearDrop/blob/add655c6c5a40b9ba03c0329b598f2817185f5a9/PROTOCOL.md#qr-codes). A new P-256 signing key is normalized to an even-Y point and encoded as `00 00 02 || X32`, base64url without padding, in `https://quickshare.google/qrcode#key=…`. HKDF-SHA256 over all 35 bytes derives the 16-byte advertising token (`advertisingContext`) and name key (`encryptionKey`).

Android's scanner advertises a receiver endpoint; transport roles are not reversed. The existing bounded mDNS pipeline validates the service and TCP endpoint. A visible receiver's type-1 TLV matches the token, or its encrypted name is authenticated using AES-128-GCM (12-byte nonce, 16-byte tag, advertising token as AAD). Hidden matching names use a separate bounded private channel, not the public nearby-device list. Replaced and expired invitation IDs cannot claim a newer selection.

One invitation waits for up to 120 monotonic seconds and is claimed once. The sender connects directly to the matched LAN socket without an unrelated route preference/fallback. Its encrypted `PairedKeyEncryptionFrame.qr_code_handshake_data` (field 4) carries ECDSA-P256/SHA256 over the raw 32-byte UKEY2 authentication token, serialized as P1363 `R32 || S32`. It does not sign the displayed PIN, reset session keys or replace either acceptance gate: local verification and remote ACCEPT still precede file bytes. The QR lifetime limits pending discovery, not an established transfer.

This path uses LAN discovery, not Bluetooth QR TLVs or cloud download services. It neither turns on receive visibility nor creates firewall/network profiles. Treat a displayed QR as an invitation to request the selected files; only show it to intended recipients and compare the verification code.

## Local transports and authentication

Direct LAN discovery uses mDNS and TCP, default port **47593**. Bluetooth discovery uses BLE/GATT metadata; file content travels through Classic RFCOMM rather than OBEX or GATT characteristics.

The protocol uses UKEY2/P-256 key exchange, authenticated encryption/MAC and message-sequence checks. Names and sizes are introduced for the receiver's consent screen. Device names and Bluetooth addresses are discovery metadata, not authenticated account identity. Verify the displayed code and handle consent explicitly.

Since backend **0.2.3**, accepting an incoming Bluetooth session attempts an existing-LAN upgrade before offering a hotspot, when the peer advertises Wi-Fi LAN support. A read-only NetworkManager snapshot is intersected with live kernel addresses and operational link state. Upgrade hosting admits activated physical Wi-Fi/Ethernet interfaces with usable IPv4 subnets, not software bridges, VLANs, tunnels, link-local or IPv6-only addresses. Candidates reuse the configured LAN listener's actual TCP port, normally **47593**. Accepted upgrade sockets are restricted to the advertised address, active interface and subnet; no profile, firewall rule or Wi-Fi setting is changed.

The shared listener distinguishes new connection requests from upgrade introductions with the same cancellation-safe frame reader used by sessions. New requests still require receive visibility; an existing transfer's registered upgrade can arrive after visibility expires. Four-byte public endpoint IDs route only to a unique owned admission slot. Duplicate registrations are refused, queues are bounded, and selecting one socket closes admission without replacing it or deleting a newer registration.

The LAN offer travels over the authenticated old channel. Public `CLIENT_INTRODUCTION` routes the candidate; it is not key proof. Native `LAST_WRITE` / `SAFE_TO_CLOSE` and old-channel terminal shutdown precede **encrypted** replacement-channel traffic. Existing keys and sequence counters continue without reset. Already-authorized sharing acceptance can be encrypted on the replacement, but file sending and the reported LAN medium wait for authenticated replacement-channel input. The ordering follows [Google Nearby's BWU implementation](https://github.com/google/nearby/blob/d128b5fca30b3fa8d4642f8c75dd9e314e090e74/connections/implementation/bwu_manager.cc).

Setup, connection or introduction failure before either old-channel `LAST_WRITE` retains healthy Bluetooth; it does not replace an active Wi-Fi connection with a hotspot. After drain begins, failure is terminal rather than a pretend Bluetooth rollback. A shared SSID or working Internet connection does not prove peer reachability through a firewall or AP client isolation.

Since backend **0.2.4**, the application discovery cache, aggregate peer/routing index and public snapshot each retain at most **128 discovered peers**. This count does not bound the mDNS dependency's internal DNS-record cache. A LAN service is identified by transport and fullname, not its mutable address/port. Replacing its route withdraws the previous ID before probing or publishing the replacement; an old owner's withdrawal cannot remove a newer owner of the same address. Count pressure evicts the oldest observation.

LAN observations have a **120-second monotonic lease**. Source and aggregate timers expire them even while receive visibility is off; snapshot reads and Send also prune expired devices. Lease-only refreshes do not churn the serialized model. Bluetooth uses the discovery-owned lease described below because unchanged Bluetooth observations are not emitted as aggregate refresh events. Dropped discovery events invalidate the affected cache/model instead of retaining an unknowable partial list.

The pinned mDNS dependency silently renews unchanged DNS records without sending a new resolution event. The owned discovery browser therefore starts a fresh daemon/browse every **60 seconds**, retaining only the bounded application cache and renewing leases only from fresh validated responses and reachable TCP endpoints. Old-generation events are drained and discarded; advertisement, Bluetooth, active file sockets, PIN consent and network configuration are untouched.

Since backend **0.3.1**, the separate mDNS advertisement daemon is created only while receive visibility is on; constructing the advertiser creates no daemon, and an invisible advertiser owns no multicast listener or retained DNS cache. A visible daemon generation is retired every **60 seconds** (half the LAN peer lease), even if repeated Visible updates arrive. Retirement unregisters the service and shuts down the daemon, awaiting shutdown acknowledgment before creating a replacement. Hiding, cancellation, visibility-channel closure and errors also retire the generation; shutdown is still attempted if setup or unregister fails, and failures remain explicit. No discarded-event monitor subscription is retained.

This bounds the advertisement daemon's cache retention lifetime, not its absolute memory use or the number of DNS records a flood can deliver within one generation. Advertisement and discovery-browser generations are separate lifecycles. Their rotation does not replace the shared TCP listener, interrupt active data sockets, change endpoint IDs or LAN priority, or alter QR discovery and consent.

Since backend **0.3.2**, Bluetooth discovery shares one **128-address / 45-second monotonic lease** across address observations, legacy-header results and peers. Expiry, oldest-record eviction, removal and shutdown retire the associated metadata and withdraw routes together. Only radio observations renew freshness; Connected, Paired, Bonded and Trusted changes do not. A bounded **128-message**, unique-BlueZ-owner signal stream replaces persistent per-address BlueR event streams. GATT enumeration uses directly owned, short-lived streams rather than `device.services()`, whose service-resolution helper also creates a retained BlueR subscription. Signal connections are separate from the command connection so their backpressure cannot prevent Connect, property-read or Disconnect replies.

BlueZ 5.87 makes public/static-random devices non-temporary during `Device1.Connect` and successful GATT discovery; Disconnect does not undo that promotion. Before each new automatic connection, the backend therefore reads the live adapter-wide Device1 inventory and requires **fewer than 128 objects**. This counts objects retained by BlueZ across application restarts, not a resettable process-local quota. Paired or bonded, trusted and blocked devices are not automatically connected; existing connections are never claimed for cleanup. Before closing an owned GATT connection, protection is checked again and a same-address live Classic bearer prevents aggregate Disconnect. Control calls use the captured unique BlueZ owner; owner and adapter identity changes fail closed.

This is conservative admission for the application's automatic promotions, not an atomic reservation against other clients or a bound on all BlueZ RAM, radio discoveries or disk caches. Temporary objects added concurrently can exceed the threshold between census and Connect. Public/non-private cached names may persist independently of this connection path. The plugin does not call RemoveDevice, change pairing/trust/block state or delete BlueZ storage to reclaim a budget; those operations could affect devices and connections it does not own.

Live LE connections hold the radio-coordination gate even when a peer skips reading our GATT metadata. An ordinary-user, unbound HCI socket uses the read-only Linux `HCIGETCONNLIST` ioctl to distinguish LE bearers, including public-address links, from unrelated Classic audio connections. No extra capabilities or privileged Bluetooth monitor are required. Device connection signals wake the coordinator; a passive policy tick also detects LE disappearance when BlueZ's aggregate `Connected` flag remains true for Classic.

Hiding or cancelling a transfer is not disconnect confirmation. Incomplete or unreadable connection snapshots fail closed. Radio errors remain explicit; the service does not reset adapters, disconnect foreign clients or silently retry a transfer. Coordination checks prevent changes while a known LE bearer is live, but are not a guarantee against firmware faults or races with other Bluetooth clients.

Temporary hotspot changes require explicit consent before interrupting an existing Wi-Fi connection. Hosting/joining those routes uses NetworkManager-owned profiles and protocol-protected credentials; existing-LAN upgrades use only read-only interface inspection and owned sockets. No Wi-Fi Direct/P2P, WPA3, 6 GHz, Google account/contact identity or lossless resume is advertised.

## File publication and cancellation

Incoming storage is anchored to the destination directory descriptor. Remote names are validated, size/offset sequences are checked, and staging files use restrictive permissions. Completed files are published without overwriting existing destinations. A collision produces a numbered name.

Cancelled or failed incoming sessions roll back local staging. The sender cannot delete Android's files: Android's Quick Share implementation owns its destination and cache lifecycle. A local cancelled/failed state is not a promise of remote storage cleanup. Zero-byte offers can be rejected by Android; content is not transformed to disguise that result.

The sender's cancellation path retains encrypted-frame state, sends the protocol cancellation messages and retires resources before publishing its terminal result. Cancellation and disconnection are not completion, and a transfer is not silently resumed or retried.

## CLI

```bash
omarchy-quickshare doctor
omarchy-quickshare status
omarchy-quickshare watch
omarchy-quickshare visible 120
omarchy-quickshare visible 0
omarchy-quickshare network-consent on
omarchy-quickshare network-consent off
omarchy-quickshare pick-files | omarchy-quickshare share
```

`share` prepares the panel only. `share` and `send DEVICE_ID` read a JSON array of file paths from stdin; `download-dir` reads a JSON string. File paths are not accepted as positional arguments. Keep paths out of command strings and external programs' arguments when constructing input; the portal pipeline above avoids both. Other commands:

| Command | Effect |
| --- | --- |
| `send DEVICE_ID` | Request an outgoing transfer using a JSON file-path array on stdin |
| `qr-send` | Create a short-lived native QR invitation for JSON file paths on stdin |
| `qr-stop INVITATION_ID` | Withdraw only that pending QR invitation, not an established transfer |
| `accept TRANSFER_ID` | Approve the identified request |
| `reject TRANSFER_ID` | Reject the identified request |
| `cancel TRANSFER_ID` | Request cancellation |
| `download-dir` | Select an existing absolute destination from a JSON string on stdin |
| `pick-files` | Open the desktop portal chooser; cancellation returns `[]` |
| `take-share TOKEN` | Retrieve and consume a prepared selection; output a JSON array |
| `watch` | Initial snapshot followed by NDJSON changes |

Use `--help` for current arguments. `doctor` inspects local tools, radio availability and permissions; it does not certify a remote Android transfer. `serve` has transport-isolation flags for controlled development. Never run another daemon alongside the installed service with the same D-Bus name.

## Locations and maintenance

Native Omarchy manages the Git checkout under its plugin directory. The service and Nautilus registration are maintained separately by the panel's setup terminal. Configuration and received data are not code-installation resources. See [Installation](INSTALLATION.md) for source/package modes and ownership rules.

Scope firewall permissions to trusted interfaces and actual ports. Do not expose the service through port forwarding, globally disable the firewall or grant unrestricted system authorization.

## Licenses and corresponding source

| Component | License |
| --- | --- |
| Original `src/`, QML, Nautilus integration, scripts and documentation | [MIT](../LICENSE) |
| Vendored rquickshare core, including modifications | [GPL-3.0-only](../vendor/rquickshare/LICENSE) |
| Google protobuf schemas | [Apache-2.0](../vendor/rquickshare/LICENSE-APACHE), retained file headers |
| Chromium protobuf schemas | [BSD-3-Clause](../vendor/rquickshare/LICENSE-CHROMIUM), retained file headers |
| Locked dependencies | Respective licenses in [THIRD_PARTY_NOTICES.txt](../vendor/THIRD_PARTY_NOTICES.txt) |

The core derives from [rquickshare](https://github.com/Martichou/rquickshare), revision `378d8ae969941bee4bf60ad34ac9cf8bb7005eb7`. The **combined daemon is not MIT-only**. Distributing it carries GPL source and notice obligations; MIT licensing of original components does not relicense the core.

Provide the corresponding source for the distributed build, including the vendored modifications, locked dependencies/build scripts and applicable notices. Source installation retains source material and license notices; Arch package generation produces a source snapshot alongside the package. Regenerate dependency notices with `./scripts/update-notices.sh` when dependencies change.

Quick Share and other names belong to their respective owners. This project is independent of Google, Android and Omarchy.
