#!/usr/bin/env bash
#
# install.sh — install sidepanel for the current user and start it with the
# graphical session via a systemd user service.
#
# The unit is bound to graphical-session.target, so it starts and stops with
# the Plasma session. It is deliberately NOT lingered: the app needs
# WAYLAND_DISPLAY, which only exists inside a graphical session.

set -euo pipefail

PROG=sidepanel
PREFIX="${HOME}/.local"
CONFIG_HOME="${XDG_CONFIG_HOME:-${HOME}/.config}"
CONFIG_DIR="${CONFIG_HOME}/${PROG}"
UNIT_DIR="${CONFIG_HOME}/systemd/user"
AUTOSTART_DIR="${CONFIG_HOME}/autostart"
UNIT_NAME="${PROG}.service"
START=1
UNINSTALL=0

info() { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
Install sidepanel for the current user and start it with the graphical session.

Usage:
  install.sh [--prefix DIR] [--no-start] [--uninstall]

Options:
  --prefix DIR   install prefix (default: ~/.local; binary goes to DIR/bin)
  --no-start     install and enable, but do not start it now
  --uninstall    stop, disable and remove the binary and unit (config is kept)
  -h, --help     show this help
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix)
      [ $# -ge 2 ] || die "--prefix needs a directory"
      PREFIX="$2"
      shift 2
      ;;
    --no-start) START=0; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
done

BINDIR="${PREFIX}/bin"
BINARY="${BINDIR}/${PROG}"

command -v systemctl >/dev/null 2>&1 || die "systemctl not found; this installer needs systemd"

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH='' cd -- "${SCRIPT_DIR}/.." && pwd)

# --- uninstall -------------------------------------------------------------
if [ "$UNINSTALL" = 1 ]; then
  info "stopping and disabling ${UNIT_NAME}"
  systemctl --user disable --now "$UNIT_NAME" 2>/dev/null || true
  rm -f "${UNIT_DIR}/${UNIT_NAME}"
  systemctl --user daemon-reload 2>/dev/null || true
  rm -f "$BINARY"
  info "removed ${BINARY} and ${UNIT_DIR}/${UNIT_NAME}"
  info "kept your config at ${CONFIG_DIR}/config.toml"
  exit 0
fi

# --- build -----------------------------------------------------------------
# Always rebuild: a stale binary in target/release would otherwise be installed
# verbatim, and a stale D-Bus client can fail against the running service (for
# example a mismatched KGlobalAccel method name).
command -v cargo >/dev/null 2>&1 || die "cargo not found; it is required to build ${PROG}"
info "building release binary"
( cd "$REPO_ROOT" && cargo build --release )

BUILT="${REPO_ROOT}/target/release/${PROG}"
[ -x "$BUILT" ] || die "build did not produce ${BUILT}"

# --- install binary --------------------------------------------------------
install -d "$BINDIR"
install -m 0755 "$BUILT" "$BINARY"
info "installed ${BINARY}"

# --- install config (never clobber) ---------------------------------------
install -d "$CONFIG_DIR"
if [ -e "${CONFIG_DIR}/config.toml" ]; then
  info "keeping existing config at ${CONFIG_DIR}/config.toml"
else
  install -m 0644 "${REPO_ROOT}/contrib/config.toml" "${CONFIG_DIR}/config.toml"
  info "installed default config to ${CONFIG_DIR}/config.toml"
fi

# --- install unit ----------------------------------------------------------
install -d "$UNIT_DIR"
if [ "$BINDIR" = "${HOME}/.local/bin" ]; then
  install -m 0644 "${REPO_ROOT}/contrib/${UNIT_NAME}" "${UNIT_DIR}/${UNIT_NAME}"
else
  # Rewrite ExecStart so a custom --prefix still points at the right binary.
  sed "s|^ExecStart=.*|ExecStart=${BINARY}|" \
    "${REPO_ROOT}/contrib/${UNIT_NAME}" > "${UNIT_DIR}/${UNIT_NAME}"
  chmod 0644 "${UNIT_DIR}/${UNIT_NAME}"
fi
info "installed ${UNIT_DIR}/${UNIT_NAME}"

# --- avoid a conflicting autostart entry ----------------------------------
# Two daemons would both register the same KGlobalAccel action and toggle two
# panels, so make sure the XDG autostart path is not also active.
if [ -e "${AUTOSTART_DIR}/${PROG}.desktop" ]; then
  warn "removing conflicting autostart entry ${AUTOSTART_DIR}/${PROG}.desktop"
  rm -f "${AUTOSTART_DIR}/${PROG}.desktop"
fi

# --- enable and start ------------------------------------------------------
systemctl --user daemon-reload
systemctl --user enable "$UNIT_NAME"
info "enabled ${UNIT_NAME} for graphical-session.target"

if [ "$START" = 1 ]; then
  if systemctl --user show-environment 2>/dev/null | grep -q '^WAYLAND_DISPLAY='; then
    systemctl --user restart "$UNIT_NAME"
    info "started ${UNIT_NAME}"
  else
    warn "no WAYLAND_DISPLAY in the systemd user environment; it will start at next login"
  fi
fi

info "done. Check status with: systemctl --user status ${UNIT_NAME}"
