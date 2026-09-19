#!/usr/bin/env bash
# poros installer — https://github.com/gildrb/poros
# Usage: curl -fsSL <raw url of this file> | bash
set -euo pipefail
IFS=$'\n\t'

REPO="gildrb/poros"
BIN_DIR="${HOME}/.local/bin"

log() { printf 'poros: %s\n' "$1"; }
fail() { printf 'poros: %s\n' "$1" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"

case "$(uname -s)" in
  Linux) os="linux" ;;
  Darwin) os="darwin" ;;
  *) fail "unsupported operating system: $(uname -s); use 'nix profile install github:${REPO}' or 'cargo install --git https://github.com/${REPO}'" ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) arch="x86_64" ;;
  aarch64 | arm64) arch="aarch64" ;;
  *) fail "unsupported architecture: $(uname -m); use 'nix profile install github:${REPO}' or 'cargo install --git https://github.com/${REPO}'" ;;
esac

target="${arch}-${os}"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

api_url="https://api.github.com/repos/${REPO}/releases/latest"
tag="$(
  curl --proto '=https' --tlsv1.2 -fsSL "$api_url" |
    grep -o '"tag_name": *"[^"]*"' |
    head -n 1 |
    cut -d'"' -f4
)"
case "$tag" in
  v[0-9]*) ;;
  *) fail "could not resolve the latest release tag" ;;
esac

base="https://github.com/${REPO}/releases/download/${tag}"
tarball="poros-${target}.tar.gz"
log "downloading ${tag} for ${target}"
curl --proto '=https' --tlsv1.2 -fsSL "${base}/${tarball}" -o "${tmpdir}/${tarball}"
curl --proto '=https' --tlsv1.2 -fsSL "${base}/${tarball}.sha256" -o "${tmpdir}/${tarball}.sha256"

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$tmpdir" && sha256sum -c "${tarball}.sha256" >/dev/null 2>&1) ||
    fail "checksum verification failed for ${tarball}"
elif command -v shasum >/dev/null 2>&1; then
  expected="$(cut -d' ' -f1 "${tmpdir}/${tarball}.sha256")"
  actual="$(shasum -a 256 "${tmpdir}/${tarball}" | cut -d' ' -f1)"
  [[ "$actual" == "$expected" ]] || fail "checksum verification failed for ${tarball}"
else
  fail "need sha256sum or shasum to verify the download"
fi

tar -xzf "${tmpdir}/${tarball}" -C "$tmpdir"
[[ -f "${tmpdir}/poros" ]] || fail "release archive did not contain a poros binary"

mkdir -p "$BIN_DIR"
install -m 0755 "${tmpdir}/poros" "${BIN_DIR}/poros"
ln -sf poros "${BIN_DIR}/p"

case ":$PATH:" in
  *":${BIN_DIR}:"*) ;;
  *) log "note: add ${BIN_DIR} to your PATH" ;;
esac

log "installed poros ${tag} to ${BIN_DIR}/poros (p is an alias)"
log "run: poros <dev command>   e.g. poros vp dev"
log "Linux only: one-time 'sudo tailscale set --operator=\$USER' lets poros manage Serve without root."
log "open the printed Poros URL from any device on your tailnet"
