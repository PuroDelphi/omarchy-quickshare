#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/service-common.sh"
ACTION=${1:-install}
[[ $ACTION == install || $ACTION == update ]] || { printf 'Usage: %s [install|update]\n' "$0" >&2; exit 2; }
initialize
check_checkout
check_resources
check_nautilus
check_idle
MODE=source
STAGE=$(mktemp -d "$STATE_DIR/.build.XXXXXXXX")
trap 'rm -rf -- "$STAGE"' EXIT
if package_installed; then
  MODE=package
  require_commands pacman
  if [[ $ACTION == update ]] || ! backend_supported "$SYSTEM_BINARY"; then
    require_commands makepkg cargo rustc protoc pkg-config sudo
    confirm \
      'Build the current checkout and authorize sudo pacman -U for the Quick Share package? Shared dependencies will not be removed. [y/N]' \
      '¿Compilar el checkout actual y autorizar sudo pacman -U para el paquete Quick Share? No se retirarán dependencias compartidas. [s/N]' || exit 2
    QUICKSHARE_PACKAGE_OUTPUT="$STAGE/packages" bash "$ROOT/scripts/build-package.sh" --noconfirm
    PACKAGES=("$STAGE/packages/"omarchy-quickshare-[0-9]*.pkg.tar.*)
    [[ ${#PACKAGES[@]} == 1 && -f ${PACKAGES[0]} ]] || fail 'Expected exactly one Quick Share package artifact.' 'Se esperaba exactamente un paquete Quick Share compilado.'
    check_resources
    pause_service
    sudo pacman -U -- "${PACKAGES[0]}"
    backend_supported "$SYSTEM_BINARY" &&
      [[ $("$SYSTEM_BINARY" --version) == "omarchy-quickshare $(jq -r .version "$ROOT/manifest.json")" ]] || fail \
      'Installed backend is unsupported or does not match the checkout.' 'El servicio instalado no es compatible o no coincide con el checkout.'
  else
    pause_service
  fi
  # A previous recorded source backend must not linger as a PATH shadow of the package.
  if owned_file "$BINARY" "$STATE_DIR/binary.sha256"; then
    rm -- "$BINARY"
    rm -f -- "$STATE_DIR/binary.sha256"
  fi
  if [[ -f $BACKEND_DATA/.service-owned ]] && cmp -s "$BACKEND_DATA/.service-owned" <(printf 'omarchy-quickshare-service\n'); then
    rm -rf -- "$BACKEND_DATA"
  fi
else
  require_commands cargo rustc protoc pkg-config tar gzip
  [[ -x /usr/bin/cargo && -x /usr/bin/rustc ]] || fail \
    'Install the Arch rust and cargo packages before building the service.' \
    'Instala los paquetes rust y cargo de Arch antes de compilar el servicio.'
  if [[ $ACTION == update && ! -f $STATE_DIR/binary.sha256 ]]; then
    fail 'No owned source service is installed. Choose Install service first.' 'No hay un servicio de código fuente propio instalado. Elige primero Instalar servicio.'
  fi
  say 'Building the locked Rust backend from this native plugin checkout…' 'Compilando el servicio Rust bloqueado de este plugin Git nativo…'
  # Keep a reusable compiler cache outside the watched Git plugin checkout.
  BUILD_CACHE="$STATE_DIR/build-cache"
  if [[ -e $BUILD_CACHE || -L $BUILD_CACHE ]]; then
    [[ -d $BUILD_CACHE && ! -L $BUILD_CACHE ]] &&
      cmp -s "$BUILD_CACHE/.compiler-owned" <(printf 'omarchy-quickshare-build\n') ||
      fail 'Compiler cache is not owned by this service.' 'La caché de compilación no pertenece a este servicio.'
  else
    install -d -m700 "$BUILD_CACHE"
    printf 'omarchy-quickshare-build\n' > "$BUILD_CACHE/.compiler-owned"
  fi
  BUILT_BINARY=$(RUSTC=/usr/bin/rustc /usr/bin/cargo build --manifest-path "$ROOT/Cargo.toml" --target-dir "$BUILD_CACHE" --release --locked \
    --package omarchy-quickshare --message-format=json-render-diagnostics |
    jq -er 'select(.reason == "compiler-artifact" and .target.name == "omarchy-quickshare" and (.target.kind | index("bin")) != null) | .executable | select(. != null)')
  [[ -x $BUILT_BINARY ]] || fail 'Cargo did not produce the backend executable.' 'Cargo no produjo el ejecutable del servicio.'
  backend_supported "$BUILT_BINARY" || fail \
    'The built backend is not a supported stable version.' 'El servicio compilado no es una versión estable compatible.'
  # Preserve matching GPL corresponding source alongside all notices, not a mutable Git URL alone.
  tar --exclude=target --exclude=.git --exclude=__pycache__ --exclude='*.pyc' -czf "$STAGE/source.tar.gz" -C "$ROOT" \
    Cargo.toml Cargo.lock src vendor scripts packaging manifest.json Service.qml BarWidget.qml Panel.qml TransferEffects.qml TransferEffectsArt.qml nautilus LICENSE
  check_resources
  pause_service
  install -Dm755 "$BUILT_BINARY" "$BINARY"
  record_file "$BINARY" "$STATE_DIR/binary.sha256"
  install -d -m700 "$BACKEND_DATA"
  printf 'omarchy-quickshare-service\n' > "$BACKEND_DATA/.service-owned"
  install -Dm644 "$STAGE/source.tar.gz" "$BACKEND_DATA/source/source.tar.gz"
  install -Dm644 "$ROOT/LICENSE" "$BACKEND_DATA/licenses/LICENSE"
  for NOTICE in LICENSE LICENSE-APACHE LICENSE-CHROMIUM NOTICE; do
    DEST=$NOTICE
    [[ $NOTICE != LICENSE ]] || DEST=LICENSE-GPL
    install -Dm644 "$ROOT/vendor/rquickshare/$NOTICE" "$BACKEND_DATA/licenses/$DEST"
  done
  install -Dm644 "$ROOT/vendor/THIRD_PARTY_NOTICES.txt" "$BACKEND_DATA/licenses/THIRD_PARTY_NOTICES.txt"
fi
install_unit "$MODE"
say 'Service installed and running. The native Git plugin, preferences and received files were left intact. Visibility starts off.' \
  'Servicio instalado y activo. El plugin Git nativo, las preferencias y los archivos recibidos permanecen intactos. La visibilidad empieza desactivada.'
say 'Nautilus: select one or several local files → Send via Quick Share. Reopen Nautilus if the menu has not loaded yet.' \
  'Nautilus: selecciona uno o varios archivos locales → Enviar por Quick Share. Reabre Nautilus si el menú aún no aparece.'
