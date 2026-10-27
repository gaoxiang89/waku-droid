# Persistent terminals

Terminal processes belong to the daemon on their host (including local
Windows ConPTY sessions). Switching tasks or workspaces, hiding a panel,
closing a terminal tab, and quitting the desktop detach views. They do not
terminate the shell. Use **End terminal session** in the terminal menu to
stop it. **Restore workspace terminals** in the command palette reattaches
detached live sessions belonging to the selected host and workspace.

The desktop saves terminal IDs, host/workspace ownership, split weights,
active panes and panel visibility in `terminal-layouts.json` beside the
desktop state database. Normal exit flushes pending layout changes. Restoring
a saved ID only attaches: a missing process is shown as exited, never silently
replaced with a new shell. A crash may lose the latest second of layout changes;
the restore command can still discover the daemon's live terminals.

The local daemon writes a resident listener record beside that database and
outlives its launching desktop. Credentials remain owner-local. Reconnection
never kills an unreachable daemon. Daemon upgrades are deferred while live
terminals exist; an incompatible protocol requires explicitly ending old
sessions first. Idle daemons may exit after 30 minutes without connections or
live work.

Each terminal maintains an independent byte offset and a 2 MiB replay window.
Attach subscribes before requesting recovery, and the renderer discards bytes
already included in the snapshot. A short disconnect replays exact missing
bytes. Fresh attachments and evicted cursors receive a reconstructed screen and up to 2,000 lines
of readable scrollback; scrollback formatting is not retained in this fallback.
The fallback supports common VT screen modes and kitty keyboard state, but is
not a full serialization of every Alacritty extension. Input is never retried
after a network failure. Device queries are answered by the daemon, so an
unattached program does not wait for a desktop renderer to reply.

This preserves processes across network failures and desktop restarts, not
daemon crashes or host reboots. Agent-specific conversation resume after a
host reboot is a separate capability. The agent's own model API connection
can still fail independently of the terminal transport.
