# Contributing

Thank you for improving [Omarchy Quick Share](https://github.com/PuroDelphi/omarchy-quickshare). Please follow the [Code of Conduct](CODE_OF_CONDUCT.md) and [security reporting guidance](SECURITY.md).

## Start with the actual scope

Read [README.md](README.md), the [architecture guide](docs/ARCHITECTURE.md) and [recorded verification](docs/VERIFICATION.md) before proposing protocol or transport changes. Preserve native Android Quick Share compatibility, explicit consent and file integrity; OBEX or a companion Android app is not an equivalent implementation.

Open an issue for a substantive behavior/design change and describe the distinct condition addressed. Small focused fixes can go directly to a pull request. Do not post vulnerability details publicly; use SECURITY.md instead. English is the primary documentation language; the UI retains optional Spanish support.

## Development setup

Use an Arch/Omarchy desktop for UI and lifecycle work. Dependencies and the native plugin/service lifecycle are in [Installation](docs/INSTALLATION.md). The canonical plugin is the repository-root manifest/QML plus `nautilus/`; the Rust daemon is separate. Do not modify the installed Omarchy package's desktop files to develop this plugin.

```bash
cargo build --workspace --locked
cargo test --workspace --locked
bash scripts/tests/service-maintenance.sh
omarchy plugin validate .
```

Build and deterministic tests run in CI; local plugin validation requires the installed Omarchy CLI. Physical radio tests are consent-required and ignored by default; do not enable them automatically in CI or scan unfamiliar phones. Never run a second daemon alongside the installed service. Finish/cancel active transfers before installation or service replacement.

For a release build, use `cargo build --release --locked --package omarchy-quickshare`; for a local Arch artifact, use `./scripts/build-package.sh`. Install the Git plugin with the native Omarchy command and maintain its backend through the panel's service controls, as described in the installation guide.

## Implementation expectations

- Keep protocol/crypto/network operations in Rust, not QML; stream files with bounded resources.
- Preserve authentication, explicit code comparison/acceptance, destination validation, non-overwrite and local partial cleanup.
- Reflect confirmed engine state/medium. Capability flags, Linux loopback peers and fixtures are not native Android acceptance evidence.
- Keep the plugin theme-integrated; update English and Spanish strings together. Preserve unrelated inline shell settings when changing preferences.
- Own only project resources; reject foreign destination/link collisions. Preserve downloads/configuration/preferences during lifecycle operations.
- Add focused regression coverage for behavior changes, migrate affected callers and remove obsolete paths rather than add compatibility shims without need.
- Update affected user, architecture and verification documentation. Describe observed behavior accurately; do not recast a failed transfer as success.
- For a backend security release, update `MINIMUM_BACKEND_VERSION` in `scripts/service-common.sh` along with release versions and exercise Install/repair with an older system package, not only Update. Verify both the executable on disk and `backend_version` from the running daemon; snapshot API compatibility is not a security-version check. Keep the stable-version boundary regressions and current minimum documentation aligned.
- Preserve pinned dependency provenance and notices. If dependencies change, update `Cargo.lock` and regenerate notices with `./scripts/update-notices.sh`.

## Evidence in pull requests

Describe the problem, actual change, affected interfaces and checks **actually run**, including failures/not-run checks. For physical results, record direction, radio/network conditions, confirmed transport, size, state, relevant versions and integrity method. A stored Android copy independently read by USB/MTP or hashed locally is stronger evidence than a round trip. Reuse existing evidence where the changed behavior is unaffected.

Never automate consent, publish verification codes/keys, manipulate radios without agreement or use manual Android deletion to claim protocol cleanup. Redact private filenames/paths, network identifiers, Bluetooth/USB device IDs and screenshots before upload. Keep location metadata/coordinates out of reports. Report observer failures separately from daemon failures.

## Licensing contributions

Original root `src/`, QML/plugin integration, `nautilus/`, scripts and documentation use the [MIT license](LICENSE), copyright 2026 Jhonny Suarez - PuroDelphi. The vendored rquickshare core, including changes made to it here, remains GPL-3.0-only; separately licensed protobuf schemas retain their Apache-2.0/BSD-3-Clause headers. The combined daemon binary carries GPL distribution and corresponding-source obligations. MIT licensing of original components does not relicense the GPL core. Preserve the [component inventory](README.md#license-and-provenance), third-party headers and attribution; do not introduce unlicensed code or claim MIT-only compatibility for the complete daemon. State the provenance/license of imported code in the pull request. Contribute only material you are authorized to license under the applicable license for the changed component.
