# Narel on Windows

Narel is a community fork of egoist/waku (GPL-3.0). These steps install this
fork's Windows package.

## Install

Download `Narel-<version>-x86_64-Setup.exe` from the
[latest fork release](https://github.com/gaoxiang89/waku-droid/releases/latest)
and run it. It installs per-user into `%LOCALAPPDATA%\Programs\Narel`, so it never
asks for administrator rights — which is also what lets Narel update itself
later without a UAC prompt. By default the installer also checks **Add Narel
CLI (`narel`) to user PATH**. That adds `<install-dir>\bin` to your user `PATH` (and remembers
your choice across silent updates).

When an older Waku copy runs Narel Setup through its updater, Setup uses an
existing separate Narel installation or the default Narel folder. Waku keeps
its own installation and can still be uninstalled separately.
If even the default Narel folder contains Waku, Setup selects a separate
`Narel-2` (or next available non-Waku sibling) instead.

### Portable

`narel-<version>-<target>.zip` is the same build without an installer. Unpack it
anywhere and run `narel.exe` (or `<dir>\bin\narel.exe` from a terminal).

**Keep the installation folder intact.** Narel launches its daemon and
Computer Use helpers from this directory and loads the bundled SDK DLL and
`resources/` files. Moving `narel.exe` out on its own leaves those unavailable.
A shortcut is fine.

A portable copy still updates itself: the updater passes the running
directory to the installer, so it replaces that copy in place rather than
creating a second install.

Narel expects:

- **Windows 10 version 1809 or newer**, or Windows 11.
- **A Direct3D 11 driver at feature level 11_0 or newer.** GPUI renders
  through DirectX and falls back to the Microsoft Basic Render Driver, so it
  can run in a VM — see Troubleshooting if the window comes up black.
- **x86_64**, or Windows on ARM with x64 emulation. This fork publishes only
  the x86_64 desktop package; there is no native aarch64 installer or feed.

Nothing else: Narel links the C runtime statically, so there is no Visual C++
redistributable to install first.

SmartScreen may warn about an unrecognized publisher on first launch when the
release was not code-signed. Choose **More info → Run anyway**.

## Command line

The installer registers `<install-dir>\bin` on your user `PATH` by default
(**Add Narel CLI (`narel`) to user PATH**). Open a new shell after installing so
the updated `PATH` takes effect.

The GUI app stays at `<install-dir>\narel.exe`. The console command lives at
`<install-dir>\bin\narel.exe`. Putting only `bin\` on `PATH` gives PowerShell,
Command Prompt, and Git Bash a console `narel` command without colliding with
the GUI executable or exposing helper binaries.

```powershell
narel                                    # launch or focus Narel
narel .                                  # open the current directory
narel C:\path\to\project                 # open a folder (or a file's parent folder)
narel --host ssh:build /srv/app          # open a folder on an SSH host
narel --host wsl:Ubuntu-24.04 ~/src/app  # open a folder in a WSL distribution
```

If Narel is already running, `narel` focuses the existing window and opens the
workspace there; otherwise it launches Narel and opens the workspace.

With the portable `.zip`, invoke `<dir>\bin\narel.exe` directly or add
`<dir>\bin` to your user `PATH`. To open workspaces from inside a WSL or SSH
shell, see [Remote hosts](remote.md#opening-workspaces-from-the-command-line).

## Updating

Narel updates itself from this fork's GitHub releases. It checks once per
launch, and an available update appears in the sidebar footer; clicking it
downloads the installer, verifies its signature, and runs it. Narel closes,
is replaced in place, and reopens. Turn the check off in **Settings → General
→ Automatic updates** — **Check for Updates…** in the app menu still works
either way.

The published Windows x86_64 feed (also used under x64 emulation on ARM):

- `https://github.com/gaoxiang89/waku-droid/releases/latest/download/appcast-windows-x86_64.xml`

Every installer carries an EdDSA signature, and Narel refuses one that does not
verify against the public key built into it — so a compromised mirror or a
tampered download cannot install anything. The preference itself lives in
`%LOCALAPPDATA%\Narel\updater.json`.

## Where Narel keeps its data

| What | Path |
| --- | --- |
| Tasks, sessions, transcripts | `%LOCALAPPDATA%\Narel\app.db` |
| Attachments and blobs | `%LOCALAPPDATA%\Narel\blobs` |
| Settings | `%USERPROFILE%\.narel\app.json` |

Unpacking a new release over the old directory leaves all of it untouched.

On the first release-build launch, Narel imports missing Waku configuration
from `%USERPROFILE%\.waku` and data from `%LOCALAPPDATA%\Waku`. It never
overwrites existing Narel files or removes Waku's originals. The daemon takes
a consistent SQLite snapshot, including committed WAL history; a failed import
stops startup and can be retried. SSH passwords remain available through a
read-only fallback to Waku's credential-store entries. Clearing one in Narel
disables that fallback without deleting Waku's password.
See [upgrade behavior](migration.md) before running both products in parallel.

## Agent CLIs

Narel detects the provider CLIs on `PATH` and, because a fresh `PATH` may
predate an install, also looks in the usual per-user prefixes:
`%APPDATA%\npm`, `%USERPROFILE%\.bun\bin`, `%USERPROFILE%\.cargo\bin`,
`%USERPROFILE%\scoop\shims`, and `%LOCALAPPDATA%\Microsoft\WindowsApps`.

Bare names resolve through `PATHEXT`, so the `claude.cmd` shim npm installs is
found the same way `claude` would be in a shell. Nothing is spawned with a
console window attached.

If a CLI is installed but not detected, set its path explicitly in
**Settings → Providers**.

## Terminal

The built-in terminal opens PowerShell 7 (`pwsh.exe`) when it is installed,
then Windows PowerShell, then whatever `COMSPEC` names. Ctrl+Shift+C and
Ctrl+Shift+V copy and paste so Ctrl+C stays available to the shell.

## Browser

The right panel's Browser tab runs on WebView2, which is in-box on Windows 11
and evergreen-installed on Windows 10. Navigation, devtools, downloads, and
pop-up handling behave as they do on macOS.

Narel hosts it in *visual* mode rather than as a child window: the page renders
into a DirectComposition visual that GPUI hands out between its own content
and its overlay plane, so menus, tooltips and dialogs composite above a live
page instead of hiding it. That is also why the browser needs a working
composition path — see the black-window note under Troubleshooting.

Differences worth knowing:

- **No load progress in the toolbar.** WebView2 reports no equivalent of
  WebKit's `estimatedProgress`, so the bar stays empty while a page loads.
- **Devtools open but do not toggle.** WebView2 offers no way to ask whether
  its devtools window is open, or to close it, so the shortcut only opens and
  refocuses it.
- **Pen, touch and dragging files into the page are not wired up.** Visual
  hosting delivers no input of its own; Narel forwards mouse, wheel, cursor and
  focus, and leaves `SendPointerInput` and the external drop target for later.
  Keyboard and IME are unaffected — those still reach the page directly once
  it holds focus.

## Computer Use

Computer Use is prohibited. No build starts the bundled Cua Driver SDK, so
Narel does not inject pointer or keyboard input into the Windows desktop.
See [Computer Use](computer-use.md).

## What is not available yet

- **Terminals over the daemon's browser client.** The desktop terminal works;
  a remote browser client connected to a Windows daemon cannot open one.

## Troubleshooting

**The window opens black, or the app exits at startup.** Narel needs a working
Direct3D 11 device. Update the GPU driver; in a VM, enable 3D acceleration.

**A provider is listed as not installed.** Open a new PowerShell window and run
the CLI by name. If the shell cannot find it either, the install did not put a
shim on `PATH`. If the shell finds it but Narel does not, set the binary path in
**Settings → Providers** and file an issue with the install method.

**Git-backed features do nothing.** Narel shells out to `git`. Install Git for
Windows and make sure `git --version` works in a new terminal.

**The update never arrives.** Narel reaches the feed with the `curl.exe` in
System32; a proxy or filter that blocks `github.com` blocks updates too.
**Check for Updates…** reports the reason, where the once-per-launch check
stays quiet. Downloading the installer by hand and running it is always
equivalent.
