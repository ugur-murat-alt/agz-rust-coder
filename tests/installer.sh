#!/usr/bin/env bash
# Hermetic installer regression tests. No request is sent to GitHub.
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
installer="${AGZ_RUST_CODER_TEST_INSTALLER:-$root/install.sh}"
fixture="$(mktemp -d)"
trap 'rm -rf -- "$fixture"' EXIT
mkdir -p "$fixture/shims" "$fixture/assets" "$fixture/source" "$fixture/home"
asset="agz-rust-coder-linux-x86_64.tar.gz"
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
    https://github.com/ugur-murat-alt/agz-rust-coder/releases/download/*) url="$1"; shift ;;
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
  printf 'agz-rust-coder\n'
  exit 2
fi
exec "$TEST_REAL_TAR" "$@"
EOF
chmod +x "$fixture/shims/curl" "$fixture/shims/tar"
export PATH="$fixture/shims:$PATH" HOME="$fixture/home"
export AGZ_RUST_CODER_VERSION=0.1.1
make_asset() {
  printf '#!/usr/bin/env bash\nprintf "agz-rust-coder %s\\n"\n' "$1" > "$fixture/source/agz-rust-coder"
  chmod +x "$fixture/source/agz-rust-coder"
  "$TEST_REAL_TAR" -czf "$fixture/assets/$asset" -C "$fixture/source" agz-rust-coder
  (cd "$fixture/assets" && sha256sum "$asset" > "$asset.sha256")
}
passed=0
run_case() {
  local name="$1" expected="$2" temporary="$3" destination="$4" status=0
  mkdir -p -- "$temporary"
  TMPDIR="$temporary" AGZ_RUST_CODER_INSTALL_DIR="$destination" \
    bash "$installer" > "$fixture/stdout" 2> "$fixture/stderr" || status=$?
  if [[ "$status" != "$expected" ]]; then
    printf 'FAIL %s: expected exit %s, got %s\n' "$name" "$expected" "$status" >&2
    cat "$fixture/stdout" "$fixture/stderr" >&2
    exit 1
  fi
  if [[ "$expected" = 0 ]]; then
    [[ "$("$destination/agz-rust-coder" --version)" = 'agz-rust-coder 0.1.1' ]]
  fi
  [[ -z "$(find "$temporary" -mindepth 1 -maxdepth 1 -print -quit)" ]]
  if [[ -d "$destination" ]]; then
    [[ -z "$(find "$destination" -name '.agz-rust-coder.tmp.*' -print -quit)" ]]
  fi
  passed=$((passed + 1))
  printf 'PASS %s\n' "$name"
}
make_asset 0.1.1
run_case ordinary-paths 0 "$fixture/tmp" "$fixture/bin"
run_case temporary-path-with-spaces 0 "$fixture/tmp space" "$fixture/bin2"
run_case install-path-with-spaces 0 "$fixture/tmp2" "$fixture/bin space"
run_case both-paths-with-spaces 0 "$fixture/tmp space2" "$fixture/bin space2"
# Failed validations must not replace an existing installation.
original="$(sha256sum "$fixture/bin/agz-rust-coder")"
printf '%064d  %s\n' 0 "$asset" > "$fixture/assets/$asset.sha256"
run_case checksum-mismatch 1 "$fixture/tmp" "$fixture/bin"
[[ "$(sha256sum "$fixture/bin/agz-rust-coder")" = "$original" ]]
make_asset 9.9.9
run_case wrong-binary-version 1 "$fixture/tmp" "$fixture/bin"
[[ "$(sha256sum "$fixture/bin/agz-rust-coder")" = "$original" ]]
make_asset 0.1.1
export TEST_BAD_LIST=1
run_case failed-archive-listing 1 "$fixture/tmp" "$fixture/bin"
unset TEST_BAD_LIST
[[ "$(sha256sum "$fixture/bin/agz-rust-coder")" = "$original" ]]
mkdir "$fixture/link-bin"
ln -s "$fixture/bin/agz-rust-coder" "$fixture/link-bin/agz-rust-coder"
run_case symlink-destination 1 "$fixture/tmp" "$fixture/link-bin"
[[ -L "$fixture/link-bin/agz-rust-coder" ]]
printf 'unexpected\n' > "$fixture/source/extra"
"$TEST_REAL_TAR" -czf "$fixture/assets/$asset" -C "$fixture/source" agz-rust-coder extra
(cd "$fixture/assets" && sha256sum "$asset" > "$asset.sha256")
run_case extra-archive-entry 1 "$fixture/tmp" "$fixture/bin"
make_asset 0.1.1
export TEST_INTERRUPT=1
run_case interrupted-download 143 "$fixture/tmp" "$fixture/bin"
[[ "$(sha256sum "$fixture/bin/agz-rust-coder")" = "$original" ]]
printf '%s installer scenarios passed\n' "$passed"
