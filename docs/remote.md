# Remote hosts: WSL and SSH

Waku can run its daemon inside a WSL distribution or on an SSH host, so
agents (Claude Code, Pi, Codex, and every other provider), file access, Git,
checkpoints, and terminals all run where the code lives. The desktop only
renders; nothing is synced.

## How it works

```
desktop                                   WSL distribution / SSH host
DaemonClient ─TCP─▶ loopback relay
                    └─ stdio ⇄ wsl.exe --exec … / ssh -T host …
                                            └─▶ waku-daemon bridge
                                                  ├─ starts or reuses the host's daemon
                                                  ├─ prints BridgeReady{token, …}
                                                  └─ relays bytes ⇄ 127.0.0.1:<daemon port>
```

- **Same daemon everywhere.** The remote side is the ordinary `waku-daemon`
  speaking the ordinary WebSocket protocol. Nothing about providers or the
  wire contract is remote-specific.
- **Transport is the user's own `wsl.exe` or `ssh`.** `~/.ssh/config`,
  ssh-agent, `ProxyJump`, and known hosts apply as in a terminal. No port is
  opened on the remote host's network; the daemon binds its loopback only.
- **The remote daemon outlives the connection.** `waku-daemon bridge` starts
  the daemon in its own session (`setsid`) and records its address and token
  in `~/.cache/waku/server/daemon.json` (mode `0600`). A dropped network
  only kills the bridge; the desktop starts a new one and resumes every
  runtime from its replay cursor, so a running agent keeps going.
- **Idle exit.** A bridge-managed daemon exits after 30 minutes with no client
  and no live provider runtime or terminal.
- **One daemon per host and build.** Bridges serialize on `daemon.lock`. A
  daemon from another build (different fingerprint) is stopped before the
  current one starts, so two daemons never share a task database.

## Deployment

On connect the desktop:

1. probes the host with `uname -s; uname -m` (Linux or macOS, x86_64 or
   aarch64);
2. compares `~/.cache/waku/server/waku-daemon --fingerprint` with the
   daemon it carries for that target, and reuses the install when they match;
3. otherwise copies (WSL, via `wslpath`) or streams over ssh's stdin (SSH)
   the binary into a temporary file, verifies its fingerprint, and moves it
   into place atomically;
4. launches `waku-daemon bridge`. A reused install that reports another
   protocol version is reinstalled once.

Development builds use `~/.cache/waku-dev/server` so they never replace a
released app's daemon.

The fingerprint is a 64-bit FNV-1a hash of the binary's bytes. It decides
reuse only; integrity in transit is the transport's job.

### Where the remote daemon binary comes from

For a target such as Linux x86_64 the desktop looks, in order, for:

1. `WAKU_REMOTE_DAEMON_LINUX_X86_64` (likewise `_LINUX_AARCH64`,
   `_DARWIN_AARCH64`, `_DARWIN_X86_64`);
2. `waku-daemon-linux-x86_64` in the package's `remote/` directory: next to
   `waku.exe` on Windows, `Contents/Resources/remote/` on macOS, and
   `share/waku/remote/` in the Linux archive;
3. in development, `target/remote-dev/`, then
   `target/<triple>/{debug,release}/waku-daemon` for the musl and gnu triples;
4. the app's own `waku-daemon` when the host has the same platform.

Release CI builds statically linked `*-unknown-linux-musl` daemons for
x86_64 and aarch64 (the `remote-daemon` job) and every desktop package
carries both, so the daemon never depends on the host's glibc. The bundle
scripts stage whatever `WAKU_REMOTE_DAEMONS_DIR` holds, and warn about an
architecture it does not hold rather than failing — a local package can
carry just the one your WSL distribution needs. Building one by hand: see
Development below.

## Using it

**Settings → Connections** lists WSL distributions (Windows only) and saved
SSH hosts. *Connect* installs the daemon on first use and adds that host to
the current window. Saved connections reconnect in the background at launch;
this computer's daemon remains available even if another host cannot connect.
*Test* probes, installs, and handshakes without adding the host. An SSH host
is either a `Host` alias from `~/.ssh/config` or `user@host:port`, optionally
with a private key (offered exclusively). Saving a connection does not contact it.

With several hosts connected, the sidebar shows a row for each host. In
Project view, projects and tasks appear below their host; Updated view keeps
a combined recent-task list. Each host row can collapse its tasks, add a
project, switch the sidebar view, or disconnect/remove a remote connection.
The local-only sidebar keeps its existing layout.

- The host row reports *Reconnecting* while its transport is down. Its last
  loaded projects remain visible and other hosts keep working.
- The host's **+** opens its own folder browser. Remote folders are listed
  by that daemon; `~` in the path field means the remote home directory.
- Task submissions, transcripts, attachments, checkpoints, Git, files, and
  right-panel terminals use the task's host. Models and CLI availability are
  cached separately for each host; account meters also stay host-specific.
- Disconnecting detaches this desktop without deleting daemon data or
  stopping running agents. Removing a saved connection also disables its
  next-launch reconnect. Disconnecting alone lasts for this app session.
- App preferences and the window frame/selection use the local `app.json`
  and `state.json`. Each daemon receives only its own project/session
  snapshot. A host that has never loaded successfully cannot be saved.
- Providers, Skills, Usage history, and Daemon settings currently manage
  this computer. Remote provider CLIs must be configured on their host.

`WAKU_CONNECTION` (a saved id, `wsl:<distro>`, or `ssh:<Host alias>`) adds a
host at launch alongside the local daemon and the saved connections. The
legacy `active` choice remains accepted as an additional launch connection.
There are no separate connection windows or new `state.<id>.json` files.

```jsonc
// connections.json
{
  "connections": [
    { "id": "wsl:Ubuntu-24.04", "name": "WSL · Ubuntu-24.04",
      "kind": { "type": "wsl", "distro": "Ubuntu-24.04" } },
    { "id": "ssh:3f2a…", "name": "SSH · build",
      "kind": { "type": "ssh", "target": { "type": "configHost", "host": "build" } } }
  ],
  "active": "wsl:Ubuntu-24.04"
}
```

SSH targets are either `configHost` (an alias) or `direct` (`hostname`,
`port`, `user`, `identityFile`, `proxyJump`). `authMethod` is `agent`
(default), `key` (requires `identityFile`, offered exclusively), or
`password`. Passwords live in the system credential store under the
connection id; ssh reads them back by launching Waku itself as
`SSH_ASKPASS`, so they never appear on a command line or in a file.

Every provider CLI the remote tasks use — `claude`, `pi`, `codex`, and so on
— must be installed and signed in **on the remote host**. The daemon resolves
them from the remote login shell's `PATH`.

## Development

On Windows, `bun ./scripts/dev.ts` also builds a Linux daemon inside WSL
after every daemon build and copies it to `target/remote-dev/`, where debug
builds find it without any environment variable. The WSL distribution needs
a Rust toolchain (`~/.cargo/env` is sourced). It uses the default
distribution; set `WAKU_DEV_WSL_DISTRO` to pick another, or
`WAKU_DEV_SKIP_WSL_DAEMON=1` to skip it. The build runs beside the watcher,
so the first WSL connection after an edit may install the previous daemon
until the log reports `WSL daemon ready`.

Without the watcher, build it by hand:

```sh
# in WSL
CARGO_TARGET_DIR=$HOME/.cache/waku-target cargo build -p waku-daemon
cp ~/.cache/waku-target/debug/waku-daemon /mnt/c/path/to/waku/target/remote-dev/waku-daemon-linux-x86_64
```

```sh
# on Windows
cargo run -p waku-client --example remote_connect -- wsl:Ubuntu-24.04 --drop
```

On Windows the dev profile builds `gpui_windows` without debug assertions
(see `Cargo.toml`): with them GPUI requests the D3D11 debug layer, which fails
on some machines and keeps the debug app from starting.

`remote_connect` installs or reuses the daemon, lists the remote home
directory, and with `--drop` kills the remote bridge to confirm the
supervisor reconnects to the same daemon.

Remote state lives in `~/.cache/waku[-dev]/server/`: `waku-daemon`,
`daemon.json`, `daemon.lock`, and `daemon.log` (the daemon's stderr).

## Current limitations

- Features that act on the desktop filesystem stay disabled for remote
  tasks: "Open in" apps and revealing remote attachments. Settings pages
  currently manage the local host.
- Terminal output produced while a connection is down is not replayed; the
  shell itself keeps running on the host.
- The Connections page has no password field yet; password authentication
  needs the credential entry created by other means. Prefer ssh-agent or a
  key.
- Hosts must be Linux or macOS on x86_64 or aarch64.
- `StrictHostKeyChecking=accept-new` trusts a new SSH host on first use and
  refuses a changed key; fix `known_hosts` in a terminal.
- On Linux desktops, SSH passwords use the kernel keyring and do not survive
  a reboot; prefer ssh-agent or key authentication there.
