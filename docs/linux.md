# Narel on Linux

## Install

```sh
curl -fsSL https://waku.sh/install.sh | sh
```

The script needs no root. It unpacks the release tarball into
`~/.local/narel.app` and installs the desktop entry into
`~/.local/share/applications`, so **Narel appears in your applications menu** —
you can also launch it from a terminal via the `narel` command. Run the script again to
upgrade manually; the installed app also keeps itself current.

Narel expects:

- **glibc 2.35 or newer** — Ubuntu 22.04, Debian 12, Fedora 36, and anything
  more recent. Releases are built on Ubuntu 22.04, so older distributions must
  build from source.
- **A working Vulkan or OpenGL driver.** Narel renders through wgpu, which tries
  Vulkan first and falls back to GL. Software rasterizers (lavapipe, llvmpipe)
  are accepted, so it can run in a VM, but see the note below.
- **x86_64 or aarch64.** Other architectures build from source.
- `xdg-desktop-portal` for native file dialogs.
- `curl` or `wget` for installation and update downloads.

Set `WAKU_VERSION` to install a specific version rather than the latest.

## Installing manually

The script is a convenience, not a requirement. Download
`narel-<version>-<target>.tar.gz` from
[releases.waku.sh](https://releases.waku.sh) or the
[GitHub release](https://github.com/egoist/waku/releases), then unpack it
wherever you like:

```sh
mkdir -p ~/.local/narel.app
tar -xzf narel-<version>-<target>.tar.gz --strip-components=1 -C ~/.local/narel.app
ln -sf ~/.local/narel.app/bin/narel ~/.local/bin/narel   # optional
```

The archive uses an install-prefix layout (`bin/`, `share/`) beneath one
versioned directory, so `--strip-components=1` into a prefix such as
`/usr/local` works too.

**Keep `bin/` and `share/narel/` intact.** Narel launches its daemon, updater,
and Computer Use helpers from `bin/`; the SDK library and supporting resources
ship in the same installation. A symlink is fine — Narel resolves it back to
the real path.

Installing the desktop entry is the part that matters — it is how the app is
launched normally, and it is what associates the running window with its icon
and name (Narel reports the Wayland `app_id` / X11 `WM_CLASS` `sh.narel`, which
matches the entry's filename). Install the packaged file and point it at the
install (the packaged copy uses bare `Exec=narel` and `Icon=sh.narel` names so it
can be relocated):

```sh
install -D ~/.local/narel.app/share/applications/sh.narel.desktop \
  -t ~/.local/share/applications
sed -i "s|^Exec=narel$|Exec=$HOME/.local/narel.app/bin/narel|" \
  ~/.local/share/applications/sh.narel.desktop
sed -i "s|^Icon=sh.narel$|Icon=$HOME/.local/narel.app/share/icons/hicolor/256x256/apps/sh.narel.png|" \
  ~/.local/share/applications/sh.narel.desktop
```

## Command line

`install.sh` links `~/.local/narel.app/bin/narel` to `~/.local/bin/narel` (for a
manual install, copy or link it into a directory on your `PATH`). On Linux,
`bin/narel` handles CLI arguments directly:

```sh
narel                            # launch or focus Narel
narel .                          # open the current directory
narel /path/to/project           # open a folder (or a file's parent folder)
narel --host ssh:build /srv/app  # open a folder on an SSH host
```

If Narel is already running, `narel` focuses the existing window and opens the
workspace there; otherwise it launches Narel and opens the workspace. To open
workspaces from inside a connected SSH host, see
[Remote hosts](remote.md#opening-workspaces-from-the-command-line).

## Updating

Tarball installs under the user's home directory update themselves. Narel
checks once per launch by default; an available release appears in the sidebar
footer. Clicking it validates the staged installation, quits through Narel's
normal draft/state saves, swaps the complete prefix, and relaunches. If the new
build exits before opening its window, the helper restores and relaunches the
previous version.

Every archive is verified with the same Ed25519 release key used by the macOS
and Windows updaters. The architecture-specific feeds are:

- `https://releases.waku.sh/appcast-linux-x86_64.xml`
- `https://releases.waku.sh/appcast-linux-aarch64.xml`

Use **Check for Updates** for an explicit check, or disable launch checks in
**Settings → General → Automatic updates**. System-wide installs such as
`/usr/local`, builds without the managed-install marker, root sessions, and
package-manager-owned builds do not modify themselves; upgrade those through
their original installation method. Re-running `install.sh` remains a safe
manual fallback for the default `~/.local/narel.app` install.

## Uninstalling

```sh
curl -fsSL https://waku.sh/install.sh | sh -s -- --uninstall
```

This removes `~/.local/narel.app`, the symlink, and the desktop entry. Projects
and settings stay in `~/.narel`; delete that directory to remove them too.

## Building from source

See [CONTRIBUTING.md](../CONTRIBUTING.md) for build prerequisites, then
install Bun for the SDK artifact assembler, then produce the same archive
this page installs with:

```sh
./scripts/bundle-linux.sh
```

To exercise the install script against that local build:

```sh
WAKU_BUNDLE_PATH=target/release/narel-<version>-<target>.tar.gz \
  sh website/public/install.sh
```

## Computer Use

Debug builds expose Computer Use through the bundled Cua Driver SDK. X11 and
AT-SPI use the current desktop session; native Wayland support is experimental
and depends on compositor integrations. See [Computer Use](computer-use.md)
for capability checks, packaged helper files, and limitations.

## Running in a virtual machine

VMs usually have no GPU passthrough, so Mesa falls back to a software
rasterizer. That works in principle — wgpu accepts a CPU adapter — but both
lavapipe (Vulkan) and llvmpipe (GL) JIT-compile shaders through LLVM, and that
path is fragile: on Fedora 44 aarch64 (mesa 26.0.3 + LLVM 22.1) it segfaults
inside `gallivm_jit_function` while compiling a fragment shader. The crash is
in the driver, not in Waku, and no application-side setting avoids it.

If the app dies on its first frame in a VM, check `coredumpctl info` for a
backtrace through `libvulkan_lvp.so` or `libgallium`. The reliable fix is to
give the guest a real GL driver — on UTM that means the QEMU backend with
virtio-gpu-gl (virgl) rather than Apple Virtualization, which offers Linux
guests no 3D at all. `VK_DRIVER_FILES=/nonexistent.json` hides the software
Vulkan driver so wgpu takes the GL path instead.
