# Upgrading from Waku to Narel

The Windows updater may offer Narel to an installed Waku copy. Setup installs
Narel separately and leaves Waku's executable, uninstall entry, and data intact.
Close Waku before launching Narel for the first time for the cleanest import.

Before release-build startup reads settings, connections, or window state,
Narel copies missing files from `~/.waku` to `~/.narel`, and from the platform
local-data directory's `Waku` folder to its `Narel` folder. On Windows these
are `%USERPROFILE%\.waku` and `%LOCALAPPDATA%\Waku`. Debug checkouts are not
imported; `WAKU_DATA_DIR` isolates daemon task data from the default import.

The daemon imports `app.db` using SQLite's consistent `VACUUM INTO` snapshot,
including committed WAL transactions, before advertising readiness. It never
copies raw database/WAL files. Projects, history blobs, drafts, account
configuration, settings, and saved connections are retained. Existing Narel
files always win; databases and JSON catalogs are **not merged** if the
corresponding Narel file already exists. Old account/workspace absolute paths
remain valid because the originals remain in place.

Imports use cross-process OS file locks and publish complete files without
replacing existing destinations. A completion marker is written only after a
directory import succeeds. On failure startup stops; fix the filesystem or
permissions problem and launch again to fill the remaining missing files.
Do not delete the `.waku-import-v1` or `.waku-database-import-v1` markers to retry a completed import:
that could restore intentionally deleted Narel files. Interrupted attempts may
leave unused `.narel-import-*.tmp` files; these are not loaded as user data.
Filesystems must support hard links for atomic no-replace publication.

Symbolic links and special files are not followed or imported. If a workspace
or account directory was a symlink, continue using its original absolute path
or configure the desired path explicitly in Narel. Runtime daemon endpoint
records are not imported. No source is moved or removed.

SSH credentials are not written to disk or logs. Narel first reads its own OS
credential-store entry; only a missing entry falls back to Waku's service.
Locked or unavailable credential stores do not trigger fallback. Saving a new
password affects Narel only. Clearing it records a Narel-only fallback opt-out,
so the old password cannot reappear and Waku's password is not deleted.

This is a one-time import, not synchronization: later Waku edits are not
automatically reflected in Narel. Keep legacy data until you have checked your
history and provider accounts. Some provider/account/worktree paths still
reference the originals; do not delete or relocate those originals casually.
