#!/usr/bin/env bash
# Hermetic installer regression tests. No request is sent to GitHub.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
installer="${AGZ_RUST_MCP_TEST_INSTALLER:-$root/install.sh}"
fixture="$(mktemp -d)"
trap 'rm -rf -- "$fixture"' EXIT
mkdir -p "$fixture/shims" "$fixture/assets" "$fixture/source" "$fixture/home"
# The installed executable is always renamed to the current product identity.
installed="agz-rust-mcp"
expected_version=""
export TEST_ASSETS="$fixture/assets" TEST_REAL_TAR="$(command -v tar)"
cat > "$fixture/shims/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
output=""
url=""
while (($#)); do
  case "$1" in
    --output) output="$2"; shift 2 ;;
    --proto|--retry) shift 2 ;;
    --fail|--location|--tlsv1.2|--silent|--show-error) shift ;;
    https://github.com/ugur-murat-alt/agz-rust-mcp/releases/download/*) url="$1"; shift ;;
    *) echo "unexpected curl argument: $1" >&2; exit 1 ;;
  esac
done
[[ -n "$output" && -n "$url" ]]
cp -- "$TEST_ASSETS/${url##*/}" "$output"
if [[ "${TEST_INTERRUPT:-0}" = 1 ]]; then
  kill -TERM "$PPID"
fi
EOF
cat > "$fixture/shims/tar" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${TEST_BAD_LIST:-0}" = 1 && "${1:-}" = -tzf ]]; then
  printf 'agz-rust-mcp\n'
  exit 2
fi
exec "$TEST_REAL_TAR" "$@"
EOF
chmod +x "$fixture/shims/curl" "$fixture/shims/tar"
export PATH="$fixture/shims:$PATH" HOME="$fixture/home"
# 0.1.1 is a published legacy release: its tag, archive, and archived binary
# keep the former `agz-rust-coder` identity.
export AGZ_RUST_MCP_VERSION=0.1.1
asset=""
make_asset() {
  local prefix="$1" version="$2"
  asset="${prefix}-linux-x86_64.tar.gz"
  printf '#!/usr/bin/env bash\nprintf "%s %s\\n"\n' "$prefix" "$version" > "$fixture/source/$prefix"
  chmod +x "$fixture/source/$prefix"
  "$TEST_REAL_TAR" -czf "$fixture/assets/$asset" -C "$fixture/source" "$prefix"
  (cd "$fixture/assets" && sha256sum "$asset" > "$asset.sha256")
}
make_legacy_asset() {
  make_asset agz-rust-coder "$1"
}
passed=0
run_case() {
  local name="$1" expected="$2" temporary="$3" destination="$4" status=0
  mkdir -p -- "$temporary"
  TMPDIR="$temporary" AGZ_RUST_MCP_INSTALL_DIR="$destination" \
    bash "$installer" > "$fixture/stdout" 2> "$fixture/stderr" || status=$?
  if [[ "$status" != "$expected" ]]; then
    printf 'FAIL %s: expected exit %s, got %s\n' "$name" "$expected" "$status" >&2
    cat "$fixture/stdout" "$fixture/stderr" >&2
    exit 1
  fi
  if [[ "$expected" = 0 ]]; then
    [[ "$("$destination/$installed" --version)" = "$expected_version" ]]
  fi
  [[ -z "$(find "$temporary" -mindepth 1 -maxdepth 1 -print -quit)" ]]
  if [[ -d "$destination" ]]; then
    [[ -z "$(find "$destination" -name ".${installed}.tmp.*" -print -quit)" ]]
  fi
  passed=$((passed + 1))
  printf 'PASS %s\n' "$name"
}
make_legacy_asset 0.1.1
expected_version='agz-rust-coder 0.1.1'
run_case legacy-asset-ordinary-paths 0 "$fixture/tmp" "$fixture/bin"
run_case legacy-asset-temporary-path-with-spaces 0 "$fixture/tmp space" "$fixture/bin2"
run_case legacy-asset-install-path-with-spaces 0 "$fixture/tmp2" "$fixture/bin space"
run_case legacy-asset-both-paths-with-spaces 0 "$fixture/tmp space2" "$fixture/bin space2"
# Failed validations must not replace an existing installation.
original="$(sha256sum "$fixture/bin/$installed")"
printf '%064d  %s\n' 0 "$asset" > "$fixture/assets/$asset.sha256"
run_case checksum-mismatch 1 "$fixture/tmp" "$fixture/bin"
[[ "$(sha256sum "$fixture/bin/$installed")" = "$original" ]]
make_legacy_asset 9.9.9
run_case wrong-binary-version 1 "$fixture/tmp" "$fixture/bin"
[[ "$(sha256sum "$fixture/bin/$installed")" = "$original" ]]
make_legacy_asset 0.1.1
export TEST_BAD_LIST=1
run_case failed-archive-listing 1 "$fixture/tmp" "$fixture/bin"
unset TEST_BAD_LIST
[[ "$(sha256sum "$fixture/bin/$installed")" = "$original" ]]
mkdir "$fixture/link-bin"
ln -s "$fixture/bin/$installed" "$fixture/link-bin/$installed"
run_case symlink-destination 1 "$fixture/tmp" "$fixture/link-bin"
[[ -L "$fixture/link-bin/$installed" ]]
printf 'unexpected\n' > "$fixture/source/extra"
"$TEST_REAL_TAR" -czf "$fixture/assets/$asset" -C "$fixture/source" agz-rust-coder extra
(cd "$fixture/assets" && sha256sum "$asset" > "$asset.sha256")
run_case extra-archive-entry 1 "$fixture/tmp" "$fixture/bin"
make_legacy_asset 0.1.1
export TEST_INTERRUPT=1
run_case interrupted-download 143 "$fixture/tmp" "$fixture/bin"
[[ "$(sha256sum "$fixture/bin/$installed")" = "$original" ]]
unset TEST_INTERRUPT
# The published 0.2.0 release uses the same legacy identity and is the
# installer default.
export AGZ_RUST_MCP_VERSION=0.2.0
make_legacy_asset 0.2.0
expected_version='agz-rust-coder 0.2.0'
run_case published-0.2.0-legacy-asset 0 "$fixture/tmp" "$fixture/bin4"
# Releases from 0.3.0 onward use the new identity for the tag, archive, and
# archived binary; the installed executable keeps the same public name.
export AGZ_RUST_MCP_VERSION=0.3.0
make_asset agz-rust-mcp 0.3.0
expected_version='agz-rust-mcp 0.3.0'
run_case current-asset-ordinary-paths 0 "$fixture/tmp" "$fixture/bin3"
printf '%s installer scenarios passed\n' "$passed"
