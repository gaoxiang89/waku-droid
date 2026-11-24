# Narel

> **English below — 简体中文说明见下。**
>
> Narel is a community fork of [egoist/waku](https://github.com/egoist/waku) by
> [EGOIST](https://github.com/egoist), licensed under the same
> [GNU GPL v3.0](LICENSE). All credit for the original Waku belongs to the
> original author — thank you for building and open-sourcing it.
>
> **Most users should use the original app instead**: download it at
> [waku.sh](https://waku.sh) or follow
> [egoist/waku](https://github.com/egoist/waku) for the canonical, signed
> macOS, Linux, and Windows builds.
>
> This fork packages Windows builds with WSL and SSH host support, and is
> shared in case it is useful. The window title, menus, and command are
> **Narel**. `narel --version` reports `community fork of egoist/waku, GPL-3.0`.
> The complete source lives in this repository.

**关于本 fork**

> Narel 是 [EGOIST](https://github.com/egoist) 的
> [egoist/waku](https://github.com/egoist/waku) 的社区 fork，遵循与原版相同的
> [GNU GPL v3.0](LICENSE)。原版 Waku 及其全部成果归原作者所有，感谢原作者的开发与开源。
>
> **一般用户请使用原版**：前往 [waku.sh](https://waku.sh) 下载，或关注
> [egoist/waku](https://github.com/egoist/waku) 获取官方签名的 macOS、Linux
> 和 Windows 版本。
>
> 本 fork 打包带 WSL 与 SSH 主机支持的 Windows 版本。窗口标题、菜单和命令
> 使用 **Narel**。`narel --version` 会标明基于 egoist/waku、GPL-3.0。
> 完整源代码位于本仓库。

Narel is a fast, native desktop app for working with local coding agents. It is
built in Rust with [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui)
and keeps projects, sessions, transcripts on your machine.

## Install

On macOS, [download the signed `.dmg`](https://waku.sh) from the original
project. It updates itself.

On Linux:

```sh
curl -fsSL https://waku.sh/install.sh | sh
```

The script installs into `~/.local` without root. See
[docs/linux.md](docs/linux.md) for requirements, manual installation, and
uninstalling. This script comes from the original project.

On Windows, run `Narel-<version>-<arch>-Setup.exe` from the
[latest fork release](https://github.com/gaoxiang89/waku-droid/releases/latest).
It installs per-user and updates itself. A portable `.zip` is published
alongside it. See
[docs/windows.md](docs/windows.md) for requirements and what is not available
there yet.

To run agents where your code lives, open a WSL distribution or SSH host from
Settings → Connections; see [docs/remote.md](docs/remote.md).

### Command line

Open a local or remote folder in Narel directly from a terminal.

```sh
narel .                                  # open the current directory
narel /path/to/project                   # open a folder (or a file's parent folder)
narel --host ssh:build /srv/app          # open a folder on an SSH host
narel --host wsl:Ubuntu-24.04 ~/src/app  # open a folder in a WSL distribution
```

- **Windows:** the installer adds `<install-dir>\bin` (`narel.exe`) to your
  user `PATH` by default; portable zips ship it at `<dir>\bin\`
  ([docs/windows.md](docs/windows.md#command-line)).
- **Linux:** `install.sh` links `bin/narel` into `~/.local/bin`
  ([docs/linux.md](docs/linux.md#command-line)).
- **macOS:** symlink the app binary onto your `PATH`:
  `ln -sf /Applications/Narel.app/Contents/MacOS/Narel /usr/local/bin/narel`.
- **WSL & SSH hosts:** `narel .` works automatically inside Narel's built-in
  remote terminal, and a release bridge links `narel` into
  `~/.local/bin` for other shells
  ([docs/remote.md](docs/remote.md#opening-workspaces-from-the-command-line)).

## Supported agents

Narel works with:

- [Amp](https://ampcode.com/)
- Antigravity
- Claude Code
- Codex CLI
- Cursor CLI
- [Fx](https://fx.sh/)
- Grok Build
- Kimi Code
- OpenCode
- Pi

Install and authenticate at least one supported agent CLI before starting Narel.
Narel detects available CLIs automatically and uses each provider's native
structured protocol and session continuity.

## Highlights

- Keep projects and independent agent sessions in one native app.
- Switch models, reasoning effort, and access modes from a shared interface.
- Queue or steer follow-up messages while an agent is working.
- Rewind Git-backed tasks with conversation-aware checkpoints.
- Store app state locally, with no Narel account or remote service required.

## Architecture

The native desktop is an RPC client of the standalone `narel-daemon` process.
Provider sessions run in [`waku-core`](crates/waku-core), behind the
authenticated, versioned WebSocket contract in
[`waku-protocol`](crates/waku-protocol). Narel Desktop depends on
[`waku-client`](crates/waku-client), not on the daemon implementation. The
daemon owns task SQLite data, uploaded attachments, provider-native session
forks, and all workspace filesystem and Git operations; paths returned by it
always refer to the daemon host. The desktop retains only presentation state
and a disposable preview cache.

The browser client lives at [`apps/web`](apps/web) and uses the generated
browser transport in [`packages/waku-client`](packages/waku-client). Its
checked-in types are generated directly from the Rust protocol, while its
WebSocket client implements the same handshake, request IDs, subscriptions,
sequence deduplication, and replay cursors as the Rust client. Run
`bun run protocol:generate` after changing a wire type and
`bun run protocol:check` to verify that generated files are current.

Projectless task workspaces live on the daemon host under
`~/.narel/projects/<date>/<slug>`. The daemon moves workspaces created by the
older `~/.narel/<date>/<slug>` layout on first load.

Configuration ownership is separate too: the Release desktop writes
`~/.narel/app.json`, while Debug stays isolated at `temp/app.json`. Daemon
provider and Computer Use settings live in `~/.narel/settings.json`. The
desktop's Settings → Daemon page can explicitly
expose the child daemon on a fixed port, configure exact browser origins, and
copy its stable authentication token. It remains loopback-only by default.

When connected to a daemon managed outside the desktop process, Narel never
interprets daemon paths on the client machine. The local folder picker and PTY
are therefore unavailable until the protocol gains daemon-host picker and
terminal-stream endpoints; files, diffs, Git, skills, usage, task state, and
attachments already use daemon RPC.

Release apps bundle and sign `narel-daemon`. Development keeps the daemon at
`target/debug/narel-debug-daemon`, allowing provider-only edits to rebuild and
replace the daemon without relaunching Narel Debug (`Narel Debug.app`).

## Development

Development is supported on macOS, Linux, and Windows and requires
[Rust 1.96 or newer](https://www.rust-lang.org/tools/install) and
[Bun](https://bun.sh/). Linux supports both Wayland and X11, and Windows needs
the MSVC toolchain; install the native build prerequisites listed in
[CONTRIBUTING.md](CONTRIBUTING.md) first.

```sh
bun install
bun run dev
```

The embedded browser and experimental computer-use integration currently
remain macOS-only. Agent sessions, projects, transcripts, skills, usage,
diffs, file editing, and the terminal run natively on Linux and Windows.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow and checks.
Release maintainers should also read [RELEASING.md](RELEASING.md).

## Sponsorship

You can support the project development via [GitHub Sponsors](https://github.com/sponsors/egoist).
Sponsoring supports the original author, whose work this fork builds on.

## License

Narel is licensed under the [GNU General Public License v3.0 only](LICENSE).

This fork is a derivative work of [egoist/waku](https://github.com/egoist/waku)
by EGOIST, redistributed under the same GPL v3.0 license with full source code
available in this repository. Original copyright belongs to the original
author.
