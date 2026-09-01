#!/usr/bin/env bash

set -euo pipefail

readonly REPOSITORY="ugur-murat-alt/agz-rust-coder"
readonly BINARY_NAME="agz-rust-coder"
readonly VERSION="${AGZ_RUST_CODER_VERSION:-0.1.0}"

die() {
  printf 'agz-rust-coder installer: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    die "SHA-256 verification requires sha256sum or shasum"
  fi
}

case "$VERSION" in
  *[!0-9A-Za-z.-]* | .* | *..* | *.)
    die "invalid version: $VERSION"
    ;;
esac

case "$(uname -s)" in
  Linux) readonly PLATFORM="linux" ;;
  *) die "unsupported operating system: $(uname -s); only Linux is available in this release" ;;
esac

case "$(uname -m)" in
  x86_64 | amd64) readonly ARCHITECTURE="x86_64" ;;
  *) die "unsupported architecture: $(uname -m); only x86_64 is available in this release" ;;
esac

[[ -n "${HOME:-}" ]] || die "HOME is not set"
readonly INSTALL_DIR="${AGZ_RUST_CODER_INSTALL_DIR:-$HOME/.local/bin}"
[[ "$INSTALL_DIR" = /* ]] || die "install directory must be absolute: $INSTALL_DIR"

require_command curl
require_command install
require_command mktemp
require_command tar
require_command awk

readonly ASSET="${BINARY_NAME}-${PLATFORM}-${ARCHITECTURE}.tar.gz"
readonly TAG="${BINARY_NAME}-v${VERSION}"
readonly BASE_URL="https://github.com/${REPOSITORY}/releases/download/${TAG}"

temp_dir="$(mktemp -d "${TMPDIR:-/tmp}/${BINARY_NAME}.install.XXXXXX")"
staged_binary=""
cleanup() {
  if [[ -n "$staged_binary" && -e "$staged_binary" ]]; then
    rm -f -- "$staged_binary"
  fi
  rm -rf -- "$temp_dir"
}
trap cleanup EXIT INT TERM

printf 'Downloading %s %s for %s-%s...\n' "$BINARY_NAME" "$VERSION" "$PLATFORM" "$ARCHITECTURE"
curl --fail --location --proto '=https' --tlsv1.2 --retry 3 --silent --show-error \
  --output "$temp_dir/$ASSET" "$BASE_URL/$ASSET" \
  || die "could not download release asset: $BASE_URL/$ASSET"
curl --fail --location --proto '=https' --tlsv1.2 --retry 3 --silent --show-error \
  --output "$temp_dir/$ASSET.sha256" "$BASE_URL/$ASSET.sha256" \
  || die "could not download checksum: $BASE_URL/$ASSET.sha256"

read -r expected_checksum _ < "$temp_dir/$ASSET.sha256"
[[ "$expected_checksum" =~ ^[0-9A-Fa-f]{64}$ ]] || die "release checksum file is malformed"
actual_checksum="$(sha256 "$temp_dir/$ASSET")"
[[ "${actual_checksum,,}" = "${expected_checksum,,}" ]] \
  || die "checksum mismatch for $ASSET"

mapfile -t archive_entries < <(tar -tzf "$temp_dir/$ASSET")
[[ "${#archive_entries[@]}" -eq 1 ]] || die "release archive must contain exactly one file"
[[ "${archive_entries[0]#./}" = "$BINARY_NAME" ]] \
  || die "release archive contains an unexpected path: ${archive_entries[0]}"

mkdir "$temp_dir/extracted"
tar -xzf "$temp_dir/$ASSET" -C "$temp_dir/extracted"
extracted_binary="$temp_dir/extracted/$BINARY_NAME"
[[ -f "$extracted_binary" && ! -L "$extracted_binary" ]] \
  || die "release archive did not contain a regular binary"
chmod 0755 "$extracted_binary"
[[ "$($extracted_binary --version)" = "$BINARY_NAME $VERSION" ]] \
  || die "downloaded binary reported an unexpected version"

mkdir -p -- "$INSTALL_DIR"
[[ -d "$INSTALL_DIR" && ! -L "$INSTALL_DIR" ]] \
  || die "install directory is not a regular directory: $INSTALL_DIR"
destination="$INSTALL_DIR/$BINARY_NAME"
[[ ! -L "$destination" ]] || die "refusing to replace a symlink: $destination"
[[ ! -e "$destination" || -f "$destination" ]] \
  || die "install destination is not a regular file: $destination"

staged_binary="$(mktemp "$INSTALL_DIR/.${BINARY_NAME}.tmp.XXXXXX")"
install -m 0755 -- "$extracted_binary" "$staged_binary"
[[ "$($staged_binary --version)" = "$BINARY_NAME $VERSION" ]] \
  || die "staged binary failed its version check"
mv -f -- "$staged_binary" "$destination"
staged_binary=""

printf 'Installed %s %s to %s\n' "$BINARY_NAME" "$VERSION" "$destination"
case ":${PATH:-}:" in
  *":$INSTALL_DIR:"*) ;;
  *) printf 'Add %s to PATH to run %s directly.\n' "$INSTALL_DIR" "$BINARY_NAME" ;;
esac
