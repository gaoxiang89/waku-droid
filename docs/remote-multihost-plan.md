# Plan: several hosts in one window, grouped in the sidebar

Status: all four implementation phases are complete; automated validation is
recorded below. Visual/provider interaction testing remains manual (no visual
test was requested). This replaces "one window per
connection" (see `docs/remote.md`) with one window that holds a daemon for
each connected host and groups projects by host in the sidebar, like Pilo
(`~/git/pilo/src/components/sidebar/{sidebar-tree,env-row,types}.tsx`).

## Pilo reference

- `SidebarEnv { id, name }` is one row per connection (Local / WSL / SSH).
  `SidebarProject.envId` places each project under its host.
- The env row has a collapse chevron, `+` (add a project on that host, opening
  the host's folder picker) and a `…` menu (view: projects / recent sessions,
  remove connection).
- In "recent" view the sidebar lists sessions from every host in one flat
  list.

## Where Waku assumes a single daemon today

- `Waku { daemon: DaemonSupervisor, store: StateStore, state: PersistedState }`
  (`src/app.rs` ~1044). `Waku::new` builds the store from that one daemon.
- `StateStore::load` (`crates/waku-client/src/persistence.rs` ~997) gets
  `projects` and `sessions` from `Command::LoadTaskState`. `save` (~1068)
  sends the **whole** `projects` list and `live_session_ids` back with
  `SaveTaskState`, so the daemon treats anything missing as deleted.
- Event subscription: `runtime.rs:917` `self.daemon.subscribe_clients()`;
  RPC/`WorkspaceClient` from `self.daemon.client()` in runtime.rs (~11
  sites), sessions.rs, composer.rs, commit_dialog, branches, autocomplete,
  transcript_view, right_panel (terminal), image_preview, background_work.
- `ComposerDraftStore::remote(daemon)`, `migrate_legacy_projectless_projects`,
  and `apply_daemon_settings` / `update_settings` run against the same
  daemon.
- `projectless::add_workspace_root` already supports several roots.
- Settings pages (Providers, Skills, Usage, Daemon) read the window's daemon.

## Design

1. **Host registry on `Waku`**: `hosts: Vec<Host>` with
   `Host { key: HostKey (Local | Connection(id)), name, daemon, store,
   disconnected }`. Local is always `hosts[0]`. Remote hosts connect in the
   background (`DaemonSupervisor::connect_remote`) and are added when ready.
   Saved connections reconnect at launch.
2. **Ownership maps**: `project_host: HashMap<Uuid, HostKey>` and
   `session_host: HashMap<Uuid, HostKey>`, filled as each host's task state
   loads. Projectless sessions are keyed by session. Helpers:
   `daemon_for_session(id)`, `daemon_for_project(id)`, `selected_daemon()`.
   Replace every `self.daemon` with the matching helper (about 50 sites).
3. **Merged state**: `PersistedState` stays a single merged list in memory.
   `load` per host appends that host's projects and sessions. `save` splits
   per host: each store receives only its own projects, live ids and dirty
   sessions. App settings and app state (`app.json`, `state.json`) are written
   by the local store only. Keep the `task_state_loaded` guard **per host**
   so a host that failed to load is never overwritten.
4. **Events**: one `subscribe_clients` pump per host, all feeding the same
   handler. Session ids are UUIDs, so they cannot collide across hosts.
5. **Sidebar**: add host group rows (icon, name, Reconnecting state,
   `+` add project → `RemoteFolderPicker` bound to that host, menu: remove /
   disconnect). With only Local connected the sidebar looks as it does now.
6. **Per-host features**: the right-panel terminal, file browser, git and the
   folder picker use the selected project's host. Model catalog and provider
   status: use the selected project's host, starting with the local catalog
   as a fallback. Settings pages stay local in the first iteration.
7. **Retire connection windows**: Connections page "Open window" becomes
   "Connect" (adds the host to the sidebar). Remove `open_connection_window`,
   `secondary_window`, and `state.<id>.json`; keep `WAKU_CONNECTION` only if
   it is still useful.

## Phases

1. Host registry + ownership maps + per-host load/save (no UI change; Local
   only). Tests: split-save keeps each host's projects; failed host never
   saved.
2. Route every daemon call through the helpers; per-host event pumps.
3. Sidebar host groups + per-host add project; Connections page → Connect.
4. Per-host model catalog/provider status; remove connection windows; update
   `docs/remote.md`.

## Implementation and verification

- Host registry, project/session ownership, filtered replacement snapshots,
  independent load guards, and host-scoped composer drafts are implemented.
- Task state subscriptions carry host identity and a connection generation;
  stale results are discarded and subscriptions are detached on disconnect.
- Sidebar host rows, host-bound project pickers, Connect, launch reconnect,
  disconnect/remove, and per-host model/provider/account caches are implemented.
- Connection windows and per-connection UI state paths have been retired.
- Regression coverage includes partitioned saves, failed-load refusal, empty
  host catalogs, identical paths on different hosts, inherited session
  ownership, and catalog updates preserving other hosts' transcripts.
- `cargo test -p waku-client -p waku --lib --locked -- --quiet`: 399 app
  tests and 41 client tests pass; one benchmark is ignored. The client tests
  also cover a superseded load being unable to reopen the write gate.
- `git diff --check` and rustfmt checks pass.
- The development watcher initially failed while packaging its native Cua
  SDK: `msvc_spectre_libs` requires the Visual Studio Spectre-mitigated C++
  libraries, which were missing on this machine. Installing the BuildTools
  2022 component `Microsoft.VisualStudio.Component.VC.14.44.17.14.x86.x64.Spectre`
  (the unversioned `Microsoft.VisualStudio.Component.VC.Spectre.x86.x64` ID is
  a no-op on 17.14) resolved it. A follow-up watcher run compiled Waku and the
  Cua SDK cleanly and launched `target\debug\waku.exe` with its dev daemon.
- See `docs/remote.md` for the current behavior and local settings limitations.
  Real remote provider interactions and visual review were not exercised.

## Earlier fixes retained

- Windows dev builds compile `gpui_windows` without debug assertions
  (`Cargo.toml`).
- The connection-window deduplication fix was superseded by the removal of
  connection windows in phase 4.
- Remote-window copy: `onboarding.remote_description`, and the composer
  "Local" chip shows the host name with a server icon.
