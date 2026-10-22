#!/usr/bin/env bash
set -euo pipefail
ROOT=$(realpath "$(dirname "${BASH_SOURCE[0]}")/../..")
WORK=$(mktemp -d)
trap 'rm -rf -- "$WORK"' EXIT

check_transition() {
  local name=$1 load_state=$2 query_status=$3 stop_status=$4 expected=$5 result
  mkdir "$WORK/$name"
  # Model only systemd's load/stop boundary; run the published pause function
  # under errexit, as the installer does, and observe whether replacement proceeds.
  if CASE_DIR="$WORK/$name" COMMON="$ROOT/scripts/service-common.sh" \
    LOAD_STATE="$load_state" QUERY_STATUS="$query_status" STOP_STATUS="$stop_status" \
    bash <<'CASE'
set -euo pipefail
source "$COMMON"
STATE_DIR=$CASE_DIR
systemctl() {
  case "$*" in
    *--property=ActiveState*) printf 'inactive\n' ;;
    *--property=LoadState*) printf '%s\n' "$LOAD_STATE"; return "$QUERY_STATUS" ;;
    '--user stop omarchy-quickshare.service')
      [[ $LOAD_STATE != not-found ]] || return 5
      return "$STOP_STATUS"
      ;;
    *) printf 'Unexpected manager operation\n' >&2; return 64 ;;
  esac
}
pause_service
printf 'replacement may proceed\n' > "$CASE_DIR/replacement"
CASE
  then result=0; else result=$?; fi
  if (( result != expected )); then
    printf '%s: expected exit %s, got %s\n' "$name" "$expected" "$result" >&2
    exit 1
  fi
  if (( expected == 0 )); then
    [[ -f "$WORK/$name/replacement" ]] || { printf '%s: installation was blocked\n' "$name" >&2; exit 1; }
  else
    [[ ! -e "$WORK/$name/replacement" ]] || { printf '%s: replacement continued after a stop failure\n' "$name" >&2; exit 1; }
  fi
  printf '%s: passed\n' "$name"
}

check_transition first-install not-found 0 0 0
check_transition absent-unit-query-status not-found 4 0 0
check_transition existing-unit loaded 0 0 0
check_transition existing-unit-stop-failure loaded 0 29 29
check_transition masked-unit-stop-failure masked 0 31 31

assert() {
  local description=$1
  shift
  "$@" || { printf '%s: %s\n' "${CASE_NAME:-versions}" "$description" >&2; exit 1; }
}
event_count() {
  local event=$1 line count=0
  while IFS= read -r line; do
    if [[ $line == "$event" ]]; then count=$((count + 1)); fi
  done < "$CASE_DIR/events"
  printf '%s\n' "$count"
}

# The installer and terminal receipt wrapper are unmodified copies. Only the
# host boundaries (systemd, package building/installing, and Nautilus dependency
# probing) are fixtures. Everything, including ownership records and symlinks,
# lives under a temporary HOME with spaces; no sudo or host service is touched.
installer_case() {
  local name=$1 candidate=$2 runtime=$3 answer=$4 build_status=$5 package_status=$6 expected=$7
  local expected_build=$8 expected_start=$9 active=${10:-inactive} installed=${11:-0.3.7} action=${12:-install}
  local result state checkout command
  CASE_NAME=$name
  CASE_DIR="$WORK/$name"
  mkdir -p "$CASE_DIR/bin" "$CASE_DIR/home with spaces"
  checkout="$CASE_DIR/home with spaces/.config/omarchy/plugins/macondo.quickshare"
  mkdir -p "$checkout/scripts" "$checkout/packaging" "$checkout/.git"
  cp "$ROOT/scripts/"{install.sh,setup-service.sh,service-common.sh} "$checkout/scripts/"
  cp "$ROOT/packaging/"{omarchy-quickshare.service,system-binary.conf,local-binary.conf} "$checkout/packaging/"
  printf '{"id":"macondo.quickshare","version":"0.3.7"}\n' > "$checkout/manifest.json"
  cat >> "$checkout/scripts/service-common.sh" <<'FIXTURE'
# Test boundaries: isolated package path and unavailable desktop libraries.
SYSTEM_BINARY="$CASE_DIR/system-backend"
check_nautilus() { :; }
FIXTURE
  cat > "$CASE_DIR/system-backend" <<'FIXTURE'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  --version) printf 'omarchy-quickshare %s\n' "$(<"$CASE_DIR/candidate")" ;;
  status)
    if [[ -f $CASE_DIR/started ]]; then cat "$CASE_DIR/runtime"; else cat "$CASE_DIR/idle"; fi
    ;;
  *) exit 64 ;;
esac
FIXTURE
  cat > "$CASE_DIR/bin/systemctl" <<'FIXTURE'
#!/usr/bin/env bash
set -euo pipefail
case "$*" in
  *--property=ActiveState*) cat "$CASE_DIR/active" ;;
  *--property=LoadState*) printf 'loaded\n' ;;
  '--user stop omarchy-quickshare.service')
    printf 'stop\n' >> "$CASE_DIR/events"
    printf 'inactive\n' > "$CASE_DIR/active"
    rm -f "$CASE_DIR/started"
    ;;
  '--user start omarchy-quickshare.service')
    printf 'start\n' >> "$CASE_DIR/events"
    printf 'active\n' > "$CASE_DIR/active"
    touch "$CASE_DIR/started"
    ;;
  '--user enable omarchy-quickshare.service') printf 'enable\n' >> "$CASE_DIR/events" ;;
  '--user disable omarchy-quickshare.service') printf 'disable\n' >> "$CASE_DIR/events" ;;
  '--user daemon-reload') printf 'reload\n' >> "$CASE_DIR/events" ;;
  *) printf 'Unexpected systemctl operation: %s\n' "$*" >&2; exit 64 ;;
esac
FIXTURE
  cat > "$CASE_DIR/bin/pacman" <<'FIXTURE'
#!/usr/bin/env bash
[[ $* == '-Qq omarchy-quickshare' ]] || exit 64
FIXTURE
  cat > "$CASE_DIR/bin/sudo" <<'FIXTURE'
#!/usr/bin/env bash
set -euo pipefail
[[ $# == 4 && $1 == pacman && $2 == -U && $3 == -- && -f $4 ]] || exit 64
printf 'package-install\n' >> "$CASE_DIR/events"
(( PACKAGE_STATUS == 0 )) || exit "$PACKAGE_STATUS"
printf '%s\n' "$INSTALLED_VERSION" > "$CASE_DIR/candidate"
FIXTURE
  cat > "$checkout/scripts/build-package.sh" <<'FIXTURE'
#!/usr/bin/env bash
set -euo pipefail
[[ $* == --noconfirm ]] || exit 64
printf 'build\n' >> "$CASE_DIR/events"
(( BUILD_STATUS == 0 )) || exit "$BUILD_STATUS"
mkdir -p "$QUICKSHARE_PACKAGE_OUTPUT"
printf 'isolated package artifact\n' > "$QUICKSHARE_PACKAGE_OUTPUT/omarchy-quickshare-0.3.7-1-x86_64.pkg.tar.zst"
FIXTURE
  for command in makepkg cargo rustc protoc pkg-config; do
    printf '#!/usr/bin/env bash\nexit 64\n' > "$CASE_DIR/bin/$command"
  done
  chmod +x "$CASE_DIR/system-backend" "$CASE_DIR/bin/"*
  printf '%s\n' "$candidate" > "$CASE_DIR/candidate"
  printf '%s\n' "$runtime" > "$CASE_DIR/runtime"
  if [[ $active == transferring ]]; then
    printf 'active\n' > "$CASE_DIR/active"
    printf '{"version":1,"backend_version":"0.3.5","transfers":[{"state":"sending"}]}\n' > "$CASE_DIR/idle"
  else
    printf '%s\n' "$active" > "$CASE_DIR/active"
    printf '{"version":1,"backend_version":"0.3.5","transfers":[]}\n' > "$CASE_DIR/idle"
  fi
  : > "$CASE_DIR/events"
  if printf '%s\n\n' "$answer" | env CASE_DIR="$CASE_DIR" HOME="$CASE_DIR/home with spaces" \
    XDG_CONFIG_HOME="$CASE_DIR/home with spaces/.config" XDG_DATA_HOME="$CASE_DIR/data" XDG_STATE_HOME="$CASE_DIR/state" \
    PATH="$CASE_DIR/bin:$PATH" BUILD_STATUS="$build_status" PACKAGE_STATUS="$package_status" INSTALLED_VERSION="$installed" \
    bash "$checkout/scripts/setup-service.sh" "$action" en > "$CASE_DIR/output" 2>&1; then result=0; else result=$?; fi
  state="$CASE_DIR/state/omarchy-quickshare/service"
  assert "expected setup exit $expected, got $result (see $CASE_DIR/output)" test "$result" = "$expected"
  assert 'unexpected package build count' test "$(event_count build)" = "$expected_build"
  assert 'unexpected startup count' test "$(event_count start)" = "$expected_start"
  if (( expected == 0 )); then
    assert 'missing installed receipt' jq -e '.action == "'"$action"'" and .outcome == "installed"' "$state/setup-result.json"
    assert 'successful setup left maintenance disabled' test ! -e "$state/disabled"
    assert 'successful setup omitted Nautilus registration' test -L "$CASE_DIR/data/nautilus-python/extensions/omarchy_quickshare.py"
    assert 'successful setup did not enable service' test "$(event_count enable)" = 1
  else
    assert 'failed setup claimed installed receipt' jq -e '.outcome == "failed"' "$state/setup-result.json"
    assert 'failed setup registered Nautilus' test ! -L "$CASE_DIR/data/nautilus-python/extensions/omarchy_quickshare.py"
    if (( expected_start > 0 )); then
      assert 'unverified service was not stopped' test ! -e "$CASE_DIR/started"
      assert 'unverified service was not disabled' test "$(event_count disable)" = 1
      assert 'unverified runtime lost maintenance marker' test -f "$state/disabled"
    else
      assert 'failed setup enabled an unchecked service' test "$(event_count enable)" = 0
    fi
  fi
  if (( expected_build > 0 && build_status == 0 )) && [[ $answer == y ]]; then
    assert 'package authorization was not exercised' test "$(event_count package-install)" = 1
  else
    assert 'unexpected package authorization' test "$(event_count package-install)" = 0
  fi
  if (( expected != 0 && expected_build > 0 && build_status == 0 )) && [[ $answer == y ]]; then
    assert 'failed package replacement lost maintenance marker' test -f "$state/disabled"
    assert 'failed package replacement left the service running' test "$(<"$CASE_DIR/active")" = inactive
  fi
  if [[ $answer == n || $build_status != 0 || $active == transferring ]]; then
    assert 'cancelled/blocked setup stopped the existing service' test "$(event_count stop)" = 0
    assert 'cancelled/blocked setup changed maintenance marker' test ! -e "$state/disabled"
  fi
  printf '%s: passed\n' "$name"
}

CURRENT='{"version":1,"backend_version":"0.3.7","transfers":[]}'
installer_case install-old-upgrades 0.3.5 "$CURRENT" y 0 0 0 1 1
installer_case repair-old-active-idle 0.3.5 "$CURRENT" y 0 0 0 1 1 active
installer_case install-old-declined 0.3.5 "$CURRENT" n 0 0 1 0 0 active
installer_case install-old-build-failure 0.3.5 "$CURRENT" y 23 0 1 1 0 active
installer_case install-old-package-failure 0.3.5 "$CURRENT" y 0 24 1 1 0 active
installer_case install-old-package-unchanged 0.3.5 "$CURRENT" y 0 0 1 1 0 inactive 0.3.5
installer_case install-prerelease-upgrades 0.3.7-alpha "$CURRENT" y 0 0 0 1 1
installer_case install-current-runtime-old 0.3.7 '{"version":1,"backend_version":"0.3.5"}' y 0 0 1 0 1
installer_case install-current-runtime-missing 0.3.7 '{"version":1}' y 0 0 1 0 1
installer_case install-current-runtime-prerelease 0.3.7 '{"version":1,"backend_version":"0.3.7-alpha"}' y 0 0 1 0 1
installer_case install-current-runtime-malformed 0.3.7 '{"version":1,"backend_version":7}' y 0 0 1 0 1
installer_case install-current-runtime-invalid-json 0.3.7 'not a snapshot' y 0 0 1 0 1
installer_case install-current-runtime-wrong-api 0.3.7 '{"version":2,"backend_version":"0.3.7"}' y 0 0 1 0 1
installer_case install-current-runtime-current 0.3.7 "$CURRENT" y 0 0 0 0 1
installer_case update-old-active-transfer 0.3.5 "$CURRENT" y 0 0 1 0 0 transferring 0.3.7 update

# Direct unit installation also gates the selected executable in both modes,
# even if a caller bypasses the installer's package/source selection.
for mode in package source; do
  for version in 0.3.5 0.3.7-alpha malformed; do
    CASE_NAME="candidate-$mode-$version"
    CASE_DIR="$WORK/$CASE_NAME"
    mkdir -p "$CASE_DIR/state"
    if CASE_DIR="$CASE_DIR" COMMON="$ROOT/scripts/service-common.sh" VERSION="$version" MODE="$mode" bash <<'CASE'
set -euo pipefail
source "$COMMON"
STATE_DIR="$CASE_DIR/state"
UNIT_DIR="$CASE_DIR/units"
UNIT="$UNIT_DIR/omarchy-quickshare.service"
DROPIN="$UNIT_DIR/omarchy-quickshare.service.d/binary.conf"
NAUTILUS_EXTENSION="$CASE_DIR/nautilus/extension.py"
NAUTILUS_TARGET="$CASE_DIR/plugin/extension.py"
SYSTEM_BINARY="$CASE_DIR/system-backend"
BINARY="$CASE_DIR/user-backend"
for candidate in "$SYSTEM_BINARY" "$BINARY"; do
  if [[ $MODE == package && $candidate == "$SYSTEM_BINARY" || $MODE == source && $candidate == "$BINARY" ]]; then
    selected=$VERSION
  else
    selected=0.3.7
  fi
  printf '#!/usr/bin/env bash\nprintf "omarchy-quickshare %%s\\n" %q\n' "$selected" > "$candidate"
  chmod +x "$candidate"
done
systemctl() { printf '%s\n' "$*" >> "$CASE_DIR/manager"; }
install_unit "$MODE"
CASE
    then result=0; else result=$?; fi
    assert 'unsupported candidate was accepted' test "$result" != 0
    assert 'unsupported candidate reached service manager' test ! -e "$CASE_DIR/manager"
    assert 'unsupported candidate lost maintenance marker' test -f "$CASE_DIR/state/disabled"
    printf '%s: passed\n' "$CASE_NAME"
  done
done

CASE_NAME=version-boundaries
source "$ROOT/scripts/service-common.sh"
for version in 0.3.7 0.3.8 0.3.10 0.4.0 0.10.0 1.0.0 10.0.0; do
  assert "stable version $version was rejected" backend_version_supported "$version"
done
for version in '' 0.1.2 0.3.5 0.3.6 0.3.7-alpha 0.3.7+build 00.3.7 0.03.7 0.3.07 v0.3.7 '0.3.7 extra' $'0.3.7\n0.3.8'; do
  if backend_version_supported "$version"; then printf 'unsupported version accepted: %q\n' "$version" >&2; exit 1; fi
done
printf 'version-boundaries: passed\n'
