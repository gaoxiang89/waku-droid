# Changelog

All notable changes to Waku. This file is the **source of truth for the release
notes shown in the in-app updater**: [`scripts/release.ts`](scripts/release.ts)
extracts the section whose heading matches the version being released
(`MARKETING_VERSION`) and publishes it next to the update, so Sparkle shows it in
the update prompt.

Format follows [Keep a Changelog](https://keepachangelog.com). Add a new
`## [<version>]` section at the top for each release, matching the version you
set in the Xcode project.

Write release notes for the final product users receive, not the development
history. When a feature is still unreleased, fold its fixes and refinements into
the original feature bullet instead of adding separate entries for them.

## [unreleased]

## [0.1.30]

- Turn coding agents on or off separately for this computer and for each connected WSL or SSH host. Every agent has a switch, including ones not detected yet, and later scans skip agents you turn off

## [0.1.29]

- Browse the project's commit history in a Git graph tab in the right panel, with branch lanes, branch and tag labels, search, and a detail view of each commit's message and files
- Resize the right panel and bottom panel as far as the window allows instead of stopping at a fixed size

## [0.1.28]

- Scroll full-screen terminal apps such as Claude Code with the mouse wheel; Shift-scroll still moves the terminal's own history
- Offer the reasoning effort picker for Grok 4.7 and Grok 4.7 Fast in Grok Build, including Extra High

## [0.1.27]

- Split the bottom panel terminal into panes with keyboard shortcuts, so one session can watch two shells or logs side by side
- Draw mermaid fences as rendered diagrams in the transcript markdown, with a refined block rhythm for headings, lists, and code

## [0.1.26]

- Paste images and copied files straight into the terminal: a clipboard image is saved to a file and its path is typed into the shell (BMP and TIFF screenshots are re-encoded as PNG first), and copied files paste as shell-quoted paths — remote terminals upload images to the daemon host and say what they had to skip
- Show context compaction in the transcript as its own row with an archive glyph and state ("Compacting context", "Compacted context", failures) for Claude and OpenCode, instead of hiding it among tool calls, and cap collapsed activity summaries at the two most important kinds with the rest folded into a count
- Add a Check for Updates command to the palette, an install button and check action in Settings → General, a toast when an update is ready, and native silent updates on Windows through the installer handoff
- Read ACP tool output as text instead of escaped JSON envelopes, and size markdown table columns so code paths never wrap mid-path

## [0.1.25]

- Fix Chinese and other IME text input in the terminal, including composition previews, candidate window positioning, confirmation, and cancellation while preserving terminal control keys

## [0.1.24]

- Stream file downloads from the right panel in chunks instead of one whole-file message: no size cap remains, downloads show a live card with progress and a cancel button in the notification area, and canceled or failed transfers remove their partial file — all unchanged over WSL and SSH bridges
- Scope bottom panel terminal tabs to each session: switching tasks parks their terminals with the processes alive and restores them on return, instead of one window-global list

## [0.1.23]

- Lead activity groups with reasoning, stream thoughts inline, and fold intermediate tool noise during active turns
- Render plan steps as structured checklists with progress indicators in transcript activity cards
- Collapse long user messages into compact previews with a "Show more" / "Show less" disclosure
- Support multi-select option checklists in structured user input requests
- Preserve tool arguments and file changes across ACP tool call updates
- Show Droid autonomy level tags in the runtime mode selector

## [0.1.22]

- Serve in-app updates from this fork's own GitHub releases on every platform: the macOS, Windows, and Linux feeds and signed downloads now resolve against `gaoxiang89/waku-droid` release assets, with the Linux updater pinning enclosure URLs to that repository's downloads
- This release also carries everything listed under 0.1.21 below, which this fork never published

## [0.1.21]

- Manage files from the right panel's file list on any host: right-click a file to download it to your machine, rename files and folders inline, or delete them after a confirmation — including over WSL and SSH
- Connect to WSL distributions and SSH hosts from Settings → Connections: tasks, files, Git, checkpoints, and terminals run on that host while Waku only renders, and every package carries the static Linux daemon it installs there
- Add Factory Droid as a provider, with each model's own reasoning levels and Waku's modes mapped to Droid's autonomy levels; dk users get their managed key automatically through `droid-dk`, and a running turn keeps its thinking and tool groups expanded until the turn settles, so reasoning interleaved with answer text stays visible
- Show why model discovery failed, such as a rejected `FACTORY_API_KEY`, instead of an empty model list
- Organize the sidebar as host → workspace → session, with denser rows and status dots
- Add a terminal panel below the composer, toggled with ⌘J / Ctrl+J: run several terminals as tabs, open new ones with ⇧⌘J / Ctrl+Shift+J, switch tabs with Ctrl+PageUp/PageDown, and switch tasks without losing a running terminal
- Configure each connected host's provider binaries from its own tab on the Providers page
- Show the app version and every connected daemon's version under Settings → General → About
- Fix Grok Build, Cursor, Kimi, and Fx failing to start on Windows, and end the agent process when its session closes

## [0.1.20]

- Support the OpenCode 2.0 release, which now installs as `opencode`: OpenCode runs on its shared background service, OpenCode 2 beta and OpenCode 1 tasks continue as OpenCode tasks, and OpenCode's free models work in Waku tasks
- Fix rewinding or branching an OpenCode task after steering it mid-turn, which forked OpenCode's conversation at the wrong turn
- Fix OpenCode tasks going silent when the OpenCode service restarts; a turn it interrupted now continues in place once the service is back
- Fix Cursor model discovery and options so the pickers list every model the Cursor CLI advertises, with its reasoning effort, fast mode, and context window
- Show the release version in Claude model names, such as Opus 4.6 (1M context), in the model picker and composer
- Make the project picker searchable and scrollable, keeping long project lists reachable with the new-project and no-project actions pinned below
- Fix Pi sessions failing to start with "Could not initialize Pi" when Pi is slow to load extensions
- Keep long user messages compact: they scroll inside their bubble with edge fades instead of taking over the transcript

## [0.1.19]

- Render inline and block LaTeX math in Markdown, with a Copy Expression action and a setting to show the source
- Fix OpenCode 2 context usage to reflect the latest request instead of cumulative session totals, and cap the meter at 100%
- Clear model search when selecting a provider or Favorites tab
- Make loading spinners smoother with animations at up to 60 FPS

## [0.1.18]

- Fix Codex session forking
- Add OpenCode 2 support, add reasoning effort option for both OpenCode and OpenCode 2
- Fix memory usage for long-running sessions

## [0.1.17]

- Fix the OpenCode Resume list showing only sessions started outside a git checkout; it now lists sessions from every project
- Hold Claude's turn open while it waits on background work

## [0.1.16]

- Import and continue conversations started in agent CLIs with `/resume` or the command palette across every provider, in both Waku and Waku Web
- Linux: add signed in-app updates with clean relaunch and automatic rollback
- Copy Waku task IDs and agent CLI thread IDs from task info or the command palette
- Keep each response's actions and changed-file summary after its final tool activity
- Keep the selected task visible when navigating the sidebar
- Let nested transcript and command-output scrollers hand wheel gestures to the page only at their boundaries
- Keep multiline background-work titles on one line in summaries and panel headers

## [0.1.15]

- Codex thread goals: type /goal to set a persistent objective the task keeps pursuing — before or after the first message — with its autonomous pursuit streaming into the transcript, a status chip showing live budget or elapsed time, and a dialog to edit, pause, resume, or clear the goal (also in Waku Web)
- Discover provider-native slash commands and skills from installed agent CLIs, including multiline YAML descriptions
- Add reasoning effort selection for Grok
- Reconnect remote daemon sessions automatically after connection interruptions
- Fix Command/Ctrl+Enter steering after a provider response starts streaming
- Fix transcript file links on Windows
- Fix OpenCode dropping the first streamed event and hanging during cancellation on Windows

## [0.1.14]

- Group sidebar tasks by project or update date, order them newest or oldest first, and collapse sections
- Find in page: Search the full transcript by keywords using cmd-f or ctrl-f
- Switch between recent tasks with Ctrl+Tab and Ctrl+Shift+Tab
- Carry the current access mode into new tasks and remember it between launches
- Fix OpenCode access-mode permissions and restore pending permission prompts when resuming sessions
- Show Codex file reads, listings, and searches as file activity instead of raw commands
- Keep long panel and background-work titles on one truncated line
- Increase the minimum UI text size for better legibility

## [0.1.13]

- Add Vercel Fx support
- Support DeepSeek Harness 0.1.1 without opening its web UI
- Collapse earlier activity groups when a running turn moves on to newer transcript output

## [0.1.12]

- Invoke Codex, Pi, and Oh My Pi skills with their native syntax
- Stream live output from Claude background tasks
- Steer the oldest queued follow-up with Command/Ctrl+Enter in an empty composer
- Fix model and reasoning option selection for Cursor
- Fix npm-installed provider detection on Windows
- Fix daemon terminal sessions hanging during shutdown
- Exclude copied history from forked Codex sessions from usage totals
- Keep separate Codex reasoning sections on separate lines

## [0.1.11]

- Highlight Markdown in the file editor, and toggle between source and a rendered preview
- Add UI and code font size settings
- macOS: Add "Open in.." button to open project folder in selected application

## [0.1.10]

- Add Kimi Code support
- Add Oh My Pi support
- Fix markdown table rendering

## [0.1.8]

- Fix `PATH` resolution on Windows

## [0.1.4]

- Fix text selection in diff view

## [0.1.3]

- Pin Codex and Claude commit message generation to cheap models: gpt-5.6-luna and claude-4.5-haiku
- Animate sidebars
- Render provider file edits as inline diffs in the transcript
- Fix claude task title generation

## [0.1.2]

- Fix regression: user bubble should fit its content width

## [0.1.1]

- Give nested Markdown the full message width
- Cap composer height and scroll overflow with an overlay scrollbar
- Keep drag-selecting text past the input bounds
- Fix char boundary panic when sliding the live reasoning window

## [0.1.0]

- Add standalone Waku daemon and browser client
- Add Linux support (X11 and Wayland, you need to build from source for now)
- Answer agent questions directly in the composer
- Redesign queued follow-ups as composer cards with per-message steering
- Add DeepSeek agent preset selection (Standard, Code, Minimal, and Creator)
- Add Claude context window and ultracode effort options
- Add /fast command to toggle fast mode for Codex
- Show the latest activity in live transcript headers
- Add soft wrapping and keyboard copy feedback
- Add terminal overlay scrollbar and measure cell width from the font
- Restore window position, size, and display across launches
- Contain wheel scrolling in activity and command output viewports
- Smooth streaming markdown and reduce CPU usage while streaming

## [0.0.13]

- Add DeepSeek Harness provider
- Render user message as Markdown and linkify bare URLs
- Share one resident OpenCode serve per workspace across sessions

## [0.0.12]

- Inherit the login-shell environment for provider commands
- Fix model traits across provider switches
- Keep branch change counts current and include untracked files
- Normalize SIGCHLD for provider children
- Fix Grok model discovery

## [0.0.11]

- Fix provider detection for CLIs installed through shell PATH managers such as
  nvm and fnm
- Show models registered by Pi extensions
- Fix the model picker closing when entering a space in search
- Fix duplicate transcript history when resuming ACP sessions

## [0.0.10]

- Fix crash in due to IME composition
- Fix typo

## [0.0.9]

- Add OpenCode Go support in usage popover
- Fix app icon
- Fix Cursor model detection

## [0.0.8]

- Initial release
