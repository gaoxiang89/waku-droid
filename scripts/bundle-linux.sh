#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

target_dir="${CARGO_TARGET_DIR:-target}"
version="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"waku","version":"\([^"]*\)".*/\1/p')"
target_triple="$(rustc -vV | sed -n 's/^host: //p')"
package="narel-${version}-${target_triple}"
archive="$target_dir/release/$package.tar.gz"
staging="$(mktemp -d)"
trap 'rm -rf -- "$staging"' EXIT

cargo build --locked --release \
  --package waku --bin narel --bin narel-updater --bin narel_js_repl \
  --package waku-daemon --bin narel-daemon \
  --package waku-computer-use --bin narel_computer_use

package_dir="$staging/$package"
bun scripts/cua-driver.ts bundle "$package_dir/bin" "$package_dir/share/narel" release
install -Dm755 "$target_dir/release/narel" "$package_dir/bin/narel"
install -Dm755 "$target_dir/release/narel-updater" "$package_dir/bin/narel-updater"
install -Dm755 "$target_dir/release/narel-daemon" "$package_dir/bin/narel-daemon"
# Statically linked daemons for SSH hosts of either architecture; this
# machine's own glibc-linked daemon would not run on an older distribution.
# A local build usually stages only this machine's architecture, so a missing
# one is a warning: CI always stages both.
if [[ -n "${WAKU_REMOTE_DAEMONS_DIR:-}" ]]; then
  for candidate in "$WAKU_REMOTE_DAEMONS_DIR"/narel-daemon-*; do
    [[ -f "$candidate" ]] || continue
    remote_daemon="$(basename "$candidate")"
    install -Dm755 "$candidate" "$package_dir/share/narel/remote/$remote_daemon"
  done
  for remote_daemon in narel-daemon-linux-x86_64 narel-daemon-linux-aarch64; do
    if [[ ! -f "$package_dir/share/narel/remote/$remote_daemon" ]]; then
      echo "warning: WAKU_REMOTE_DAEMONS_DIR has no $remote_daemon; SSH hosts of that architecture cannot be reached." >&2
    fi
  done
else
  echo "warning: WAKU_REMOTE_DAEMONS_DIR is not set; SSH hosts must match this machine's platform." >&2
fi
install -Dm644 resources/linux/sh.narel.desktop \
  "$package_dir/share/applications/sh.narel.desktop"
install -Dm644 resources/linux/self-update-v1 \
  "$package_dir/share/narel/self-update-v1"
install -Dm644 website/public/app-icon.png \
  "$package_dir/share/icons/hicolor/256x256/apps/sh.narel.png"
install -Dm644 LICENSE "$package_dir/share/licenses/narel/LICENSE"

mkdir -p "$(dirname "$archive")"
tar -C "$staging" -czf "$archive" "$package"
printf 'Created %s\n' "$archive"
