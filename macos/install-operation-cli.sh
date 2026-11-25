#!/bin/bash
set -euo pipefail
umask 077

if [[ "$(uname -s)" != Darwin || $# -gt 1 ]]; then
  printf 'Usage (macOS): bash macos/install-operation-cli.sh [absolute-install-directory]\n' >&2
  exit 2
fi

repo="$(cd "$(dirname "$0")/.." && pwd -P)"
build_dir="$repo/target/operation-build"
destination="${1:-$HOME/Library/Application Support/AssignmentManagerCLI/bin}"
if [[ "$destination" != /* ]]; then
  printf 'The install directory must be an absolute path.\n' >&2
  exit 2
fi

cargo build --manifest-path "$repo/Cargo.toml" --locked --offline --release \
  --target-dir "$build_dir" --bin assignment-manager
mkdir -p "$destination"
private_directory() {
  [[ -d "$1" && ! -L "$1" && -O "$1" && "$(stat -f '%Lp' "$1")" == 700 ]]
}
if ! private_directory "$destination"; then
  printf 'The install directory must be owned by this user and have mode 0700.\n' >&2
  exit 1
fi

staging="$(mktemp -d "$destination/.staging.XXXXXXXX")"
cleanup() {
  rm -f "$staging/assignment-manager"
  rmdir "$staging"
}
trap cleanup EXIT
candidate="$staging/assignment-manager"
install -m 0700 "$build_dir/release/assignment-manager" "$candidate"
if [[ -n "${ASSIGNMENT_MANAGER_SIGN_IDENTITY:-}" ]]; then
  codesign --force --sign "$ASSIGNMENT_MANAGER_SIGN_IDENTITY" \
    --identifier io.assignment-manager.cli "$candidate"
fi
codesign --verify "$candidate"
chmod 0500 "$candidate"
digest="$(shasum -a 256 < "$candidate")"
release="$destination/${digest%% *}"
if ! mkdir "$release" 2>/dev/null && ! private_directory "$release"; then
  printf 'The version directory is unavailable.\n' >&2
  exit 1
fi

binary="$release/assignment-manager"
if [[ ! -e "$binary" && ! -L "$binary" ]]; then
  ln -h "$candidate" "$binary" 2>/dev/null || true
fi
if [[ ! -f "$binary" || -L "$binary" || ! -O "$binary" ||
  "$(stat -f '%Lp' "$binary")" != 500 ]] || ! cmp -s "$candidate" "$binary"; then
  printf 'The installed version could not be verified.\n' >&2
  exit 1
fi
codesign --verify "$binary"
printf '%s\n' "$binary"
