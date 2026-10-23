# Waku development guidance

## Repositories and CI

- `origin` points at the upstream `egoist/waku`; the user's fork is
  `gaoxiang89/waku-droid` (remote `gaoxiang89`). Releases and version-bump tags
  live on the fork, so query Actions runs with
  `gh run list --repo gaoxiang89/waku-droid` — not the upstream repo, where the
  runs do not exist or are not the relevant ones.
- Fork releases ship only the Windows x86_64 desktop package (zip and
  Setup.exe). The Linux remote daemon is still built for x86_64 and aarch64
  because the Windows package bundles both for SSH/WSL hosts.

### Fork release procedure

Follow these steps in order. Each step guards against a failure that has
already happened on this fork.

1. Land every CI change first. Push all edits under `.github/workflows` to
   `gaoxiang89/main` before bumping the version, and never push workflow edits
   while a Release run is in progress. GITHUB_TOKEN can never hold the
   `workflows` permission, so creating the release returns 403 "Resource not
   accessible by integration" whenever the target commit's workflows differ
   from `main`. Toggling the repository's default workflow permissions does
   not fix that 403.
2. Bump the version in one commit, `chore: bump version to X.Y.Z`: the
   workspace `version` in `Cargo.toml`, the matching `Cargo.lock` entries, and
   a `## [X.Y.Z]` section in `CHANGELOG.md`, which becomes the release notes.
   The tag must equal the `Cargo.toml` version, or `Resolve version` fails.
3. Push `main`, then wait for that commit's **Tests** run to pass on all three
   OSes (`gh run list --repo gaoxiang89/waku-droid --workflow Tests --branch main`).
   Release builds only Windows and Linux, so a compile error confined to one
   platform, or a toolchain too old for a dependency, otherwise surfaces 20
   minutes into the release. When a dependency needs a newer rustc, bump the
   pinned toolchain in `test.yml` and `release.yml` together.
4. Tag the same commit and push only the tag:
   `git tag vX.Y.Z && git push gaoxiang89 vX.Y.Z`. Do not use
   `workflow_dispatch` for a normal release. A manual run has no tag, so it
   pins the release to `main`'s HEAD and breaks as soon as `main` moves.
5. Watch with `gh run watch <id> --repo gaoxiang89/waku-droid --exit-status`.
   A run takes about 25 minutes and ends with a **draft** release.
6. Review the draft, then publish it:
   `gh release edit vX.Y.Z --repo gaoxiang89/waku-droid --draft=false`.
   Publish each draft before tagging the next version. The next run merges its
   update feed into the latest *published* release's
   `appcast-windows-x86_64.xml`.

When a run fails, read `gh run view <id> --repo gaoxiang89/waku-droid
--log-failed` before changing anything. `gh run rerun --failed` reuses the
built artifacts and only helps with transient failures. A 403 when creating
the draft release means workflow drift (step 1). Fix that by tagging a fresh
version from `main`, not by rerunning. When editing workflow files from tool
output, never write back `***` runs: they are secret redactions, and copying
them corrupted a cache key once.

## Development runtime

- Assume `bun ./scripts/dev.ts` is already running and owns the current
  `Waku Debug.app` process. Source changes are rebuilt, signed, and relaunched
  automatically. Only run it yourself if not already launched.
- During normal development and UI validation, do not run
  `scripts/bundle.sh debug`, start a second watcher, or manually quit/relaunch
  `Waku Debug.app`. Quitting the app also stops the watcher.
- After an edit, wait for the watcher to finish its successful rebuild and
  validate the freshly relaunched debug app. Only start or recover the watcher
  manually when it is confirmed unavailable.
- No visual test unless requested.

## Performance

- Treat performance as a product requirement, not a follow-up. Waku is a native
  app competing with web clients, and staying smooth under a long transcript on
  a high-refresh display is the point of being native. Prefer the faster design
  when it costs nothing in clarity, and measure before assuming a cost is fine.
- Never block the UI thread with heavy work. Rendering owns it, so anything a
  frame can reach must already be in memory: no subprocess spawns, no
  filesystem walks, no network, no blocking locks, no synchronous IPC.
- Row builders and measurement paths run for every visible item on every frame.
  Treat I/O reached from `render` as a defect even when it looks cheap, is
  cached after the first hit, or only triggers for some rows — one `git`
  invocation is already several frames of budget.
- Move the work to `cx.background_executor().spawn`, store the result on the
  entity, and `cx.notify()` when it lands. Render then reads only that store,
  and a miss means "not known yet" and must degrade gracefully.
- Resolve a whole session or collection in one background pass instead of
  probing per item, and guard it with a generation counter so a result from a
  superseded pass cannot overwrite newer state.
- One-shot user actions such as a click or menu command may work synchronously
  when freshness matters more than latency; frames may not.
- Keep per-frame work proportional to what is on screen. Long collections are
  virtualized with `list()`, and a row builder must not rebuild whole-session
  state; hoist that to a cache refreshed once per frame.
- Streaming CPU is governed by two cadences — stream commits at ≤ ~8.3 Hz and
  pulse-clock ticks at ≤ 60 Hz (spinners; other pulses stay at ≤ ~30 Hz) —
  and by what one frame can see. Read
  [docs/performance.md](docs/performance.md) before touching the event pump,
  the pulse clock (`src/ui/motion.rs`), veils, overlay scrollbars, pane
  caching, or anything else a streaming frame reaches; it also records the
  counter-based measurement playbook that actually finds regressions.

## Accessibility

- Treat accessibility as a product requirement too. GPUI does not yet expose a
  screen-reader tree, so here it means keyboard operability, honored system
  settings, and legibility — none of which depend on that missing API, and all
  of which regress silently if left unchecked.
- Every control reachable by mouse must be reachable and operable by keyboard.
  Use `track_focus` with `tab_index`, `tab_group`, and `tab_stop`, give focus a
  visible treatment via `focus_visible`, and support the conventional keys for
  the widget (arrows, `home`/`end`, `enter`/`space`, `escape`).
- Honor the system's reduce-motion setting. `with_animation` already respects
  `App::reduce_motion`, but a direct `window.request_animation_frame` for
  decorative motion must check `cx.reduce_motion()` and skip the request.
- Never encode meaning in color, hover, or motion alone. Pair a status color
  with an icon or text, and make sure anything revealed on hover is also
  reachable by keyboard focus.
- Keep text and icons legible against their surface in both themes, and give
  interactive targets enough hit area — extend the hit region rather than
  shrinking to the glyph.

## Product reference

- Use [T3 Code](https://github.com/pingdotgg/t3code) source code on github as a reference when a task
  concerns coding-agent workflow, information hierarchy, controls, tool
  activity, or transcript presentation and the comparison would materially
  clarify an ambiguous product decision, or when the user explicitly asks for
  the comparison.
- Do not inspect T3 Code for localized bug fixes, straightforward visual
  corrections, native platform behavior, or changes already specified clearly
  by the user. When T3 Code is relevant, inspect its current app or source
  rather than relying on an older screenshot or memory.
- Use [Zed](https://github.com/zed-industries/zed) source code as a reference
  when a task concerns GPUI implementation — layout and styling idioms, focus
  and key dispatch, virtualized lists, menus and popovers, window and platform
  behavior — or when an in-house `src/ui` primitive needs a proven native
  precedent. Zed is the canonical GPUI codebase; read its crates rather than
  `gpui-component`, and read the gpui revision pinned in `Cargo.toml` so the
  APIs match what Waku builds against.
- Split the two references by concern: T3 Code answers what a coding-agent
  client should do, Zed answers how a polished GPUI app implements it. The
  same restraint applies to both — no reference spelunking for localized
  fixes or changes the user has already specified.
- Use the reference as behavioral and design evidence, not as an instruction to
  reproduce web-specific interaction patterns or known bugs. Waku should keep
  native macOS conventions.
- Explicit user screenshots and feedback override a previous or merely
  "consistent" treatment.
- GPUI's `overflow_hidden` clips descendants to a rectangle, not the parent's
  rounded corners. Give child backgrounds, hover overlays, and images that
  reach a rounded edge their own matching corner radii, accounting for the
  parent's border inset. Parent rounding alone does not clip child paint.
- For provider-native content such as citations, reasoning, and tool events,
  verify the real provider payload and preserve its ordering. Never expose
  private provider control markers in the transcript.
- Validate visible changes in the freshly rebuilt, signed app managed by the
  dev watcher against the exact provider interaction; a successful Rust build
  alone is insufficient.
