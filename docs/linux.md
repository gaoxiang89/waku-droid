# Narel on Linux

## Install

This fork currently publishes **Windows x86_64 desktop packages only**. Linux
remote daemons are bundled for SSH/WSL, but are not Linux desktop archives.
Upstream `waku.sh` / `releases.waku.sh` serve Waku, not Narel; do not use them
to install this fork.

Build a Linux desktop bundle locally (see [Building from source](#building-from-source)),
then install it from the checkout:

```sh
WAKU_BUNDLE_PATH=target/release/narel-<version>-<target>.tar.gz \
  sh website/public/install.sh
```

The script needs no root. It unpacks the local tarball into
`~/.local/narel.app` and installs the desktop entry into
`~/.local/share/applications`, so **Narel appears in your applications menu** —
you can also launch it from a terminal via the `narel` command. Run the script again to
upgrade manually after producing a new local bundle.

Narel expects:

- **A compatible glibc** for the machine on which you build the bundle.
  See the source-build prerequisites for supported distributions.
- **A working Vulkan or OpenGL driver.** Narel renders through wgpu, which tries
  Vulkan first and falls back to GL. Software rasterizers (lavapipe, llvmpipe)
  are accepted, so it can run in a VM, but see the note below.
- **x86_64 or aarch64.** Other architectures build from source.
- `xdg-desktop-portal` for native file dialogs.
Local bundle installation does not require `curl` or `wget`. Without
`WAKU_BUNDLE_PATH`, the script exits with source-build instructions; advanced
users may explicitly set `WAKU_RELEASES_URL` to their own Narel release server.

## Installing manually

The script is a convenience, not a requirement. After building
`target/release/narel-<version>-<target>.tar.gz` locally, unpack it wherever
you like:

```sh
mkdir -p ~/.local/narel.app
tar -xzf target/release/narel-<version>-<target>.tar.gz --strip-components=1 -C ~/.local/narel.app
mkdir -p ~/.local/bin
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
manual install, symlink it into a directory on your `PATH`; do not copy the
executable away from its sibling helpers/resources). On Linux,
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

There is no published Narel Linux desktop update feed for this fork. Do not
use upstream Waku appcasts as Narel feeds. Build a fresh bundle and rerun the
local install command to upgrade; keep the installed app closed while replacing
it. You can disable automatic checks in **Settings → General → Automatic
updates**. System-wide or package-manager-owned builds should be upgraded
through their original installation method.

Release builds import missing legacy Waku files once without deleting the
originals; see [upgrade behavior](migration.md) for retry and coexistence rules.

## Uninstalling

```sh
sh website/public/install.sh --uninstall
```

This removes `~/.local/narel.app`, the symlink, and the desktop entry. Projects
and settings remain in `~/.narel` and the platform data directory (normally
`~/.local/share/Narel`). Legacy Waku data is also left untouched.

## Building from source

See [CONTRIBUTING.md](../CONTRIBUTING.md) for build prerequisites, then
install Bun for the SDK artifact assembler, then produce a local archive:

```sh
./scripts/bundle-linux.sh
```

To exercise the install script against that local build:

```sh
WAKU_BUNDLE_PATH=target/release/narel-<version>-<target>.tar.gz \
  sh website/public/install.sh
```

## Computer Use

Computer Use is prohibited. No build starts the bundled Cua Driver SDK, so
Narel does not inject pointer or keyboard input into the desktop session.
See [Computer Use](computer-use.md).

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
