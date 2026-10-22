#!/usr/bin/env bash
# Shared by the terminal maintenance actions; never changes the native plugin checkout.
set -euo pipefail
ROOT=$(realpath "$(dirname "${BASH_SOURCE[0]}")/..")
CONFIG_HOME=${XDG_CONFIG_HOME:-$HOME/.config}
DATA_HOME=${XDG_DATA_HOME:-$HOME/.local/share}
STATE_HOME=${XDG_STATE_HOME:-$HOME/.local/state}
PLUGIN_DIR="$HOME/.config/omarchy/plugins/macondo.quickshare"
STATE_DIR="$STATE_HOME/omarchy-quickshare/service"
UNIT_DIR="$CONFIG_HOME/systemd/user"
UNIT="$UNIT_DIR/omarchy-quickshare.service"
DROPIN="$UNIT_DIR/omarchy-quickshare.service.d/binary.conf"
BINARY="$HOME/.local/bin/omarchy-quickshare"
SYSTEM_BINARY=/usr/bin/omarchy-quickshare
BACKEND_DATA="$DATA_HOME/omarchy-quickshare/backend"
NAUTILUS_TARGET="$PLUGIN_DIR/nautilus/omarchy_quickshare.py"
NAUTILUS_EXTENSION="$DATA_HOME/nautilus-python/extensions/omarchy_quickshare.py"
LANGUAGE=${QUICKSHARE_LANGUAGE:-en}
MINIMUM_BACKEND_VERSION=0.3.7
backend_version_supported() {
  local version=${1:-} index LC_ALL=C
  local -a actual minimum
  [[ $version =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || return 1
  IFS=. read -r -a actual <<< "$version"
  IFS=. read -r -a minimum <<< "$MINIMUM_BACKEND_VERSION"
  for index in 0 1 2; do
    if (( ${#actual[index]} > ${#minimum[index]} )); then return 0; fi
    if (( ${#actual[index]} < ${#minimum[index]} )); then return 1; fi
    if [[ ${actual[index]} > "${minimum[index]}" ]]; then return 0; fi
    if [[ ${actual[index]} != "${minimum[index]}" ]]; then return 1; fi
  done
  return 0
}
backend_supported() {
  local binary=$1 identity
  [[ -x $binary ]] || return 1
  identity=$("$binary" --version) || return 1
  [[ $identity == 'omarchy-quickshare '* ]] || return 1
  backend_version_supported "${identity#omarchy-quickshare }"
}
backend_snapshot_supported() {
  local snapshot=$1 version
  version=$(jq -er 'select(.version == 1) | .backend_version | select(type == "string")' <<<"$snapshot") || return 1
  backend_version_supported "$version"
}
say() { if [[ $LANGUAGE == es ]]; then printf '%s\n' "$2"; else printf '%s\n' "$1"; fi; }
fail() { say "$1" "$2" >&2; exit 1; }
require_commands() {
  local name
  for name in "$@"; do
    command -v "$name" >/dev/null || fail "Missing required command: $name" "Falta el comando necesario: $name"
  done
}
initialize() {
  (( EUID != 0 )) || fail 'Run as the desktop user, not root.' 'Ejecuta como el usuario del escritorio, no como root.'
  require_commands realpath systemctl jq sha256sum cmp flock install
  [[ $CONFIG_HOME == /* && $DATA_HOME == /* && $STATE_HOME == /* ]] || fail 'XDG paths must be absolute.' 'Las rutas XDG deben ser absolutas.'
  [[ ! -L $STATE_DIR && ! -L $BACKEND_DATA ]] || fail 'Refusing a symlinked backend/state directory.' 'No se acepta una carpeta de servicio/estado con enlace simbólico.'
  install -d -m700 "$STATE_DIR"
  exec 9>"$STATE_DIR/maintenance.lock"
  flock -n 9 || fail 'Another service maintenance action is running.' 'Ya hay otra operación de mantenimiento del servicio en curso.'
}
check_checkout() {
  [[ -d $PLUGIN_DIR/.git && $(realpath "$PLUGIN_DIR") == "$ROOT" ]] || fail \
    'Use the native Git plugin checkout: omarchy plugin add https://github.com/PuroDelphi/omarchy-quickshare --enable. Legacy copied plugins must be moved out of the plugin registry first; preserve shell.json and settings.toml.' \
    'Usa el plugin Git nativo: omarchy plugin add https://github.com/PuroDelphi/omarchy-quickshare --enable. Mueve primero los plugins copiados antiguos fuera del registro; conserva shell.json y settings.toml.'
  jq -e '.id == "macondo.quickshare"' "$ROOT/manifest.json" >/dev/null || fail 'Wrong plugin manifest.' 'El manifiesto del plugin no es válido.'
}
package_installed() {
  [[ -x $SYSTEM_BINARY ]] || return 1
  command -v pacman >/dev/null && pacman -Qq omarchy-quickshare >/dev/null 2>&1 || fail \
    'A system binary exists but is not the omarchy-quickshare package. Resolve its ownership before installing a user backend.' \
    'Existe un binario del sistema que no pertenece al paquete omarchy-quickshare. Resuelve su propiedad antes de instalar un servicio de usuario.'
}
current_binary() {
  if package_installed; then printf '%s\n' "$SYSTEM_BINARY"; else printf '%s\n' "$BINARY"; fi
}
check_idle() {
  local status binary load
  load=$(systemctl --user show omarchy-quickshare.service --property=ActiveState --value)
  case $load in
    active|activating|reloading|deactivating)
      binary=$(current_binary)
      [[ -x $binary ]] || fail 'The running service has no known client; maintenance is blocked.' 'El servicio activo no tiene un cliente conocido; mantenimiento bloqueado.'
      status=$("$binary" status) || fail 'Cannot verify the running service; maintenance is blocked.' 'No se puede comprobar el servicio activo; mantenimiento bloqueado.'
      jq -e '.version == 1 and (.transfers | type == "array") and all(.transfers[]; .state == "completed" or .state == "rejected" or .state == "cancelled" or .state == "failed")' <<<"$status" >/dev/null || fail \
        'Finish or cancel all transfers before maintaining the service.' 'Termina o cancela todas las transferencias antes de mantener el servicio.'
      ;;
    inactive|failed|'') ;;
    *) fail "Unknown service state: $load; maintenance is blocked." "Estado desconocido del servicio: $load; mantenimiento bloqueado." ;;
  esac
}
owned_file() {
  local file=$1 record=$2 expected
  [[ -f $record && -f $file && ! -L $file ]] || return 1
  expected=$(sha256sum "$file")
  [[ ${expected%% *} == "$(<"$record")" ]]
}
record_file() {
  local checksum
  checksum=$(sha256sum "$1")
  printf '%s\n' "${checksum%% *}" > "$2"
  chmod 600 "$2"
}
check_resources() {
  local file
  if [[ -e $UNIT || -L $UNIT ]]; then
    if [[ -L $UNIT && $(readlink "$UNIT") == /dev/null && -f $STATE_DIR/disabled ]]; then
      : # Only the recorded maintenance mask may be replaced.
    elif [[ -L $UNIT ]] || { ! owned_file "$UNIT" "$STATE_DIR/unit.sha256" && ! cmp -s "$UNIT" "$ROOT/packaging/omarchy-quickshare.service"; }; then
      fail "User unit is not owned or was modified: $UNIT" "La unidad de usuario no es propia o fue modificada: $UNIT"
    fi
  fi
  [[ ! -L $UNIT_DIR/omarchy-quickshare.service.d ]] || fail 'Refusing a symlinked service override directory.' 'No se acepta una carpeta de configuración adicional del servicio con enlace simbólico.'
  if [[ -d $UNIT_DIR/omarchy-quickshare.service.d ]]; then
    for file in "$UNIT_DIR/omarchy-quickshare.service.d/"*; do
      [[ -e $file || -L $file ]] || continue
      [[ $file == "$DROPIN" && ! -L $file ]] || fail "Unowned service override: $file" "Configuración adicional del servicio ajena: $file"
      owned_file "$file" "$STATE_DIR/dropin.sha256" || cmp -s "$file" "$ROOT/packaging/local-binary.conf" || cmp -s "$file" "$ROOT/packaging/system-binary.conf" || fail \
        "Modified service override: $file" "Configuración adicional del servicio modificada: $file"
    done
  fi
  if [[ -e $BINARY || -L $BINARY ]]; then
    owned_file "$BINARY" "$STATE_DIR/binary.sha256" || fail \
      "Unrecorded/modified user binary: $BINARY. Move it to a backup after checking its ownership; it will not be overwritten or deleted." \
      "Binario de usuario sin registro/modificado: $BINARY. Muévelo a una copia tras comprobar su propiedad; no se sobrescribirá ni eliminará."
  fi
  if [[ -e $BACKEND_DATA ]] && { [[ -L $BACKEND_DATA/.service-owned ]] || ! cmp -s "$BACKEND_DATA/.service-owned" <(printf 'omarchy-quickshare-service\n'); }; then
    fail "Backend notices/source directory is unowned: $BACKEND_DATA" "La carpeta de licencias/fuente del servicio es ajena: $BACKEND_DATA"
  fi
  if [[ -e $NAUTILUS_EXTENSION || -L $NAUTILUS_EXTENSION ]]; then
    [[ -L $NAUTILUS_EXTENSION && $(readlink "$NAUTILUS_EXTENSION") == "$NAUTILUS_TARGET" ]] || fail \
      "Nautilus registration is not owned: $NAUTILUS_EXTENSION" "El registro Nautilus no es propio: $NAUTILUS_EXTENSION"
  fi
}
preserve_language() {
  local value temporary
  [[ -f $HOME/.config/omarchy/shell.json ]] || return 0
  value=$(jq -r '[((.bar.layout.left // []) + (.bar.layout.center // []) + (.bar.layout.right // []) + (.plugins // []))[] | select(.id == "macondo.quickshare")][0].language // empty' "$HOME/.config/omarchy/shell.json")
  [[ $value == en || $value == es ]] || return 0
  install -d -m700 "$CONFIG_HOME/omarchy-quickshare"
  temporary=$(mktemp "$CONFIG_HOME/omarchy-quickshare/.language.XXXXXXXX")
  printf '%s\n' "$value" > "$temporary"
  chmod 600 "$temporary"
  mv -f -- "$temporary" "$CONFIG_HOME/omarchy-quickshare/language"
}
check_nautilus() {
  require_commands python nautilus
  python -c 'import gi; gi.require_version("Nautilus", "4.1"); gi.require_version("Gtk", "4.0"); from gi.repository import Nautilus, Gtk' || fail \
    'Install nautilus, nautilus-python, python-gobject and gtk4 first.' 'Instala primero nautilus, nautilus-python, python-gobject y gtk4.'
  [[ -f /usr/lib/nautilus/extensions-4/libnautilus-python.so ]] || fail 'Install nautilus-python first.' 'Instala primero nautilus-python.'
}
pause_service() {
  check_idle
  printf 'disabled\n' > "$STATE_DIR/disabled"
  # First installation writes the unit later in install_unit().
  if [[ $(systemctl --user show omarchy-quickshare.service --property=LoadState --value) != not-found ]]; then
    systemctl --user stop omarchy-quickshare.service
  fi
}
register_nautilus() {
  mkdir -p "$(dirname "$NAUTILUS_EXTENSION")"
  if [[ ! -e $NAUTILUS_EXTENSION && ! -L $NAUTILUS_EXTENSION ]]; then
    ln -s -- "$NAUTILUS_TARGET" "$NAUTILUS_EXTENSION"
  fi
}
install_unit() {
  local mode=$1 binary snapshot
  if [[ $mode == package ]]; then binary=$SYSTEM_BINARY; else binary=$BINARY; fi
  printf 'disabled\n' > "$STATE_DIR/disabled"
  backend_supported "$binary" || fail \
    "Backend $MINIMUM_BACKEND_VERSION or newer (stable) is required; repair the service before starting it." \
    "Se requiere el servicio estable $MINIMUM_BACKEND_VERSION o posterior; repáralo antes de iniciarlo."
  mkdir -p "$UNIT_DIR"
  if [[ -L $UNIT && $(readlink "$UNIT") == /dev/null && -f $STATE_DIR/disabled ]]; then rm -- "$UNIT"; fi
  install -m644 "$ROOT/packaging/omarchy-quickshare.service" "$UNIT"
  if [[ $mode == package ]]; then
    install -Dm644 "$ROOT/packaging/system-binary.conf" "$DROPIN"
  else
    install -Dm644 "$ROOT/packaging/local-binary.conf" "$DROPIN"
  fi
  record_file "$UNIT" "$STATE_DIR/unit.sha256"
  record_file "$DROPIN" "$STATE_DIR/dropin.sha256"
  printf '%s\n' "$mode" > "$STATE_DIR/mode"
  systemctl --user daemon-reload
  systemctl --user enable omarchy-quickshare.service
  if ! systemctl --user start omarchy-quickshare.service; then
    reject_startup
  fi
  if ! snapshot=$("$binary" status) || ! backend_snapshot_supported "$snapshot"; then
    reject_startup
  fi
  register_nautilus
  rm -f -- "$STATE_DIR/disabled"
}
reject_startup() {
  # This startup has not been attested. Never leave it running or auto-enabled.
  systemctl --user stop omarchy-quickshare.service || say \
    'Could not stop the unverified service; stop it manually before retrying.' \
    'No se pudo detener el servicio sin verificar; detenlo manualmente antes de reintentar.' >&2
  systemctl --user disable omarchy-quickshare.service || say \
    'Could not disable the unverified service; disable it manually before retrying.' \
    'No se pudo deshabilitar el servicio sin verificar; deshabilítalo manualmente antes de reintentar.' >&2
  fail 'Service startup failed or the running backend is unsupported. Review journalctl --user -u omarchy-quickshare.service.' \
    'El servicio no inició o la versión activa no es compatible. Revisa journalctl --user -u omarchy-quickshare.service.'
}
confirm() {
  local answer
  say "$1" "$2"
  read -r answer || return 1
  [[ $answer == y || $answer == Y || $answer == s || $answer == S ]]
}
