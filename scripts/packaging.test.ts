import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dir, "..");
const read = (path: string) => readFileSync(join(root, path), "utf8");

test("macOS release checks match the bundler's executable and skill paths", () => {
  const release = read("scripts/release.ts");
  const bundle = read("scripts/bundle.sh");
  expect(release).toContain('const releaseExecutable = join(releaseDirectory, "narel");');
  expect(release).not.toContain('"waku-computer-use"');
  expect(release.match(/"narel-computer-use"/g)?.length).toBe(2);
  expect(bundle).toContain('Resources/skills/narel-computer-use');
  expect(bundle).toContain('$cargo_target_dir/$profile/narel');
});

test("Windows installer redirects an occupied default without overwriting Waku", () => {
  const installer = read("resources/windows/narel.iss");
  expect(installer).toContain("while IsWakuInstallDir(TargetDir) do");
  expect(installer).toContain("ExpandConstant('{autopf}\\Narel-') + IntToStr(Suffix)");
  expect(installer).toContain("if IsWakuInstallDir(WizardDirValue) then");
});

test("Linux installer rejects unpublished downloads and accepts a local bundle", () => {
  const shell = process.platform === "win32"
    ? "C:/Program Files/Git/bin/bash.exe"
    : "bash";
  const script = String.raw`
set -eu
root="$(mktemp -d /tmp/narel-package-test-XXXXXX)"
trap 'rm -rf -- "$root"' EXIT
mkdir -p "$root/home" "$root/mockbin" "$root/package/narel-test/bin" \
  "$root/package/narel-test/share/narel" "$root/package/narel-test/share/applications"
cat > "$root/mockbin/uname" <<'EOF'
#!/bin/sh
case "$1" in -s) echo Linux ;; -m) echo x86_64 ;; esac
EOF
cat > "$root/mockbin/curl" <<'EOF'
#!/bin/sh
echo "unexpected download" >&2
exit 99
EOF
chmod +x "$root/mockbin/"*
export HOME="$root/home" PATH="$root/mockbin:$PATH"
unset WAKU_BUNDLE_PATH WAKU_RELEASES_URL WAKU_VERSION
if sh "$1/website/public/install.sh" > "$root/unsupported.log" 2>&1; then
  echo "unsupported download unexpectedly succeeded"; exit 1
fi
grep -q 'does not publish Narel Linux desktop downloads' "$root/unsupported.log"
test ! -e "$HOME/.local/narel.app"
for binary in narel narel-daemon narel-updater; do
  printf '#!/bin/sh\nexit 0\n' > "$root/package/narel-test/bin/$binary"
  chmod +x "$root/package/narel-test/bin/$binary"
done
printf 'narel-self-update-v1\n' > "$root/package/narel-test/share/narel/self-update-v1"
printf '[Desktop Entry]\nExec=narel\nIcon=sh.narel\n' > "$root/package/narel-test/share/applications/sh.narel.desktop"
tar -czf "$root/local.tar.gz" -C "$root/package" narel-test
WAKU_BUNDLE_PATH="$root/local.tar.gz" sh "$1/website/public/install.sh"
test -x "$HOME/.local/narel.app/bin/narel"
# Git Bash can emulate ln as a copy when Windows symlink privileges are absent.
# The actual symlink contract is asserted on Linux, not that emulation.
if [ "$NAREL_TEST_GIT_BASH" != 1 ]; then
  test "$(readlink "$HOME/.local/bin/narel")" = "$HOME/.local/narel.app/bin/narel"
fi
grep -qF "Exec=$HOME/.local/narel.app/bin/narel" "$HOME/.local/share/applications/sh.narel.desktop"
sh "$1/website/public/install.sh" --uninstall
test ! -e "$HOME/.local/narel.app"
`;
  const result = Bun.spawnSync([shell, "-c", script, "packaging-test", root.replaceAll("\\", "/")], {
    cwd: root,
    env: { ...process.env, NAREL_TEST_GIT_BASH: process.platform === "win32" ? "1" : "" },
    stdout: "pipe",
    stderr: "pipe",
    timeout: 30_000,
  });
  if (result.exitCode !== 0) {
    throw new Error(Buffer.concat([result.stdout, result.stderr]).toString());
  }
  expect(result.exitCode).toBe(0);
});
