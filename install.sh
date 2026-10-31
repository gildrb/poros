#!/bin/sh
# Porthmeus installer: the newest GitHub release, checksum-verified, no root.
#
# Usage:
#   curl -fsSL https://porthmeus.com/install | sh
#   curl -fsSL https://porthmeus.com/install | sh -s -- 0.4.0
#   sh install.sh [version]
#
# https://porthmeus.com/install redirects to the copy of this script that each
# release attaches (releases/latest/download/install.sh).
#
# Installs ${PORTHMEUS_INSTALL_BIN_DIR:-~/.local/bin}/porthmeus and the short
# command p beside it. Re-running the script updates in place. It never uses
# sudo. Every download is checked against the release's SHA256SUMS.
set -eu

REPO="gildrb/porthmeus"
RELEASES="https://github.com/${REPO}/releases"

log()  { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<EOF
Usage: install.sh [version]

Install Porthmeus from the ${REPO} GitHub releases.
No root. macOS and Linux, x64 and arm64.

  version   pin one release (0.4.0, v0.4.0 or porthmeus-v0.4.0; 0.4.0 or
            newer); default: the newest release

Environment:
  PORTHMEUS_INSTALL_BIN_DIR  install directory (default ~/.local/bin)
EOF
}

for arg in "$@"; do
  case "$arg" in
    -h|--help) usage; exit 0 ;;
    -*) usage >&2; die "unknown option: $arg" ;;
  esac
done
[ $# -le 1 ] || { usage >&2; die "at most one version argument"; }

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

fetch() {
  # fetch <url> <output-file>
  curl -fsSL --proto '=https' --tlsv1.2 --retry 3 --retry-delay 2 -o "$2" "$1"
}

# --- platform ----------------------------------------------------------
os="$(uname -s)"
machine="$(uname -m)"
case "${os}:${machine}" in
  Linux:x86_64|Linux:amd64)    platform=linux-x64 ;;
  Linux:aarch64|Linux:arm64)   platform=linux-arm64 ;;
  Darwin:arm64|Darwin:aarch64) platform=darwin-arm64 ;;
  Darwin:x86_64)               platform=darwin-x64 ;;
  Linux:*|Darwin:*) die "unsupported architecture '${machine}' on ${os}" ;;
  *) die "unsupported OS '${os}': Porthmeus ships for Linux and macOS" ;;
esac

# --- version -----------------------------------------------------------
if [ $# -eq 1 ]; then
  case "$1" in
    porthmeus-v*) version=${1#porthmeus-v} ;;
    v*)           version=${1#v} ;;
    *)            version=$1 ;;
  esac
  printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
    || die "not a release version: '$1' (expected X.Y.Z)"
else
  # github.com/<repo>/releases/latest redirects to .../tag/<tag>.
  effective="$(curl -fsSL --proto '=https' --tlsv1.2 --retry 3 \
    -o /dev/null -w '%{url_effective}' "${RELEASES}/latest")" \
    || die "cannot resolve the newest release; pass a version (see --help)"
  tag="${effective##*/}"
  case "$tag" in
    porthmeus-v*) version=${tag#porthmeus-v} ;;
    *) die "newest release tag looks wrong: '${tag}'" ;;
  esac
fi
tag="porthmeus-v${version}"
tarball="porthmeus-${version}-${platform}.tar.gz"
base="${RELEASES}/download/${tag}"

# --- download ----------------------------------------------------------
work="$(mktemp -d "${TMPDIR:-/tmp}/porthmeus-install.XXXXXX")"
trap 'rm -rf "$work"' EXIT INT TERM

log "porthmeus ${version} for ${platform}"
log "downloading ${base}/${tarball}"
fetch "${base}/${tarball}" "${work}/${tarball}" \
  || die "download failed: ${tarball}"
fetch "${base}/SHA256SUMS" "${work}/SHA256SUMS" \
  || die "download failed: SHA256SUMS"

# --- verify ------------------------------------------------------------
expected="$(awk -v name="${tarball}" '$2 == name { print $1 }' "${work}/SHA256SUMS")"
[ -n "$expected" ] || die "${tarball} is not listed in SHA256SUMS"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${work}/${tarball}" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "${work}/${tarball}" | awk '{print $1}')"
else
  die "need sha256sum or shasum to verify the download"
fi
[ "$actual" = "$expected" ] || die "checksum mismatch for ${tarball}
  expected ${expected}
  actual   ${actual}"

mkdir "${work}/stage"
tar -xzf "${work}/${tarball}" -C "${work}/stage" || die "extract failed"
[ -f "${work}/stage/porthmeus" ] || die "the archive has no porthmeus binary"

# Livecheck before anything is replaced.
reported="$("${work}/stage/porthmeus" --version)" \
  || die "the downloaded binary does not run on this machine"
[ "$reported" = "$version" ] || die "binary reports '${reported}', expected '${version}'"

# --- install -----------------------------------------------------------
bin_dir="${PORTHMEUS_INSTALL_BIN_DIR:-${HOME}/.local/bin}"
binary="${bin_dir}/porthmeus"
mkdir -p "$bin_dir"
# Same directory, then rename: a running porthmeus keeps its old file.
cp "${work}/stage/porthmeus" "${bin_dir}/.porthmeus-installer.$$"
chmod 755 "${bin_dir}/.porthmeus-installer.$$"
mv -f "${bin_dir}/.porthmeus-installer.$$" "$binary"

# p: the short command. Replace only a p this script (or the earlier poros
# installer) made; never another program's.
short="${bin_dir}/p"
if [ -L "$short" ]; then
  case "$(readlink "$short")" in
    porthmeus) ;;
    poros)
      ln -sf porthmeus "$short"
      rm -f "${bin_dir}/poros"
      log "replaced the earlier poros install"
      ;;
    *) warn "${short} is another program; run porthmeus by its full name" ;;
  esac
elif [ -e "$short" ]; then
  warn "${short} is another program; run porthmeus by its full name"
else
  ln -s porthmeus "$short"
fi

# --- guidance ----------------------------------------------------------
case ":${PATH}:" in
  *":${bin_dir}:"*) ;;
  *)
    warn "${bin_dir} is not on your PATH"
    log ""
    log "  add it, then reopen your shell:"
    log "    export PATH=\"${bin_dir}:\$PATH\"   # put this line in your shell profile"
    ;;
esac
if other="$(command -v porthmeus 2>/dev/null)" && [ "$other" != "$binary" ]; then
  warn "another porthmeus is first on PATH: ${other} (this install: ${binary})"
fi
command -v tailscale >/dev/null 2>&1 \
  || [ -S /run/tailscale/tailscaled.sock ] || [ -S /var/run/tailscale/tailscaled.sock ] \
  || warn "Tailscale not found: install it and sign in (https://tailscale.com/download)"

log ""
log "installed porthmeus ${version}: ${binary} (short: p)"
log "next: run 'p vp dev' (or 'porthmeus <dev command>') in a project"
if [ "$os" = "Linux" ]; then
  log "Linux, once: 'sudo tailscale set --operator=\$USER' lets porthmeus use Tailscale Serve without root"
fi
log "update: re-run this command; uninstall: remove ${binary} and ${short}"
