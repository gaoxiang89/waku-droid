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

## [0.1.48]

- Make Git Graph branches easy to read at a glance: branch and tag capsules now sit ahead of each commit subject along the graph divider, match their lane colors, and combine synced local and remote branches into one pill with a toolbar jump menu for branches and tags
- Scale the Git Graph column with the panel and window: widening the Git Graph panel expands lane spacing and node proportions automatically, with a draggable column divider (double-click to reset) and cleaner rounded metro connectors with crossing bridges
- Improve Git Graph commit filtering and detail headers: typing in the filter box jumps to matching commits with match counts and Enter/Shift+Enter navigation, and commit headers now display their branch and tag capsules

## [0.1.47]

- Stop a turn that failed before answering from looking finished: when Antigravity aborts after an executor or trajectory-store error, Waku shows the provider's own error from its server log instead of an empty reply, and finds the CLI where the Windows installer places it
- Keep imported sessions imported: after a restart, Waku no longer offers native conversations you already imported as new sessions
- List your Droid models without a probe session: discovery reads the installed CLI's catalog, including custom models and their reasoning levels, instead of starting a throwaway session in your account
- Keep provider CLIs working after Waku has run inside an agent sandbox: a resident background service that inherited the sandbox's file restriction is replaced rather than reused across sessions

## [0.1.46]

- See how fast the model is writing: the live turn shows an estimated tokens-per-second rate beside its working indicator, and each finished turn keeps its average in the "Worked for" summary. The estimate comes from streamed text and reasoning, so tool-call pauses do not drag it down

## [0.1.45]

- Add Google Antigravity as a coding agent through the installed `agy` CLI and its ACP server, using the Google sign-in `agy` already has
- Pick Antigravity's account models and thinking level: Waku reads the available model families, keeps each family's own effort choices, and maps saved variant ids onto the family and level the CLI accepts
- Answer Antigravity's multiple-choice questions above the composer, including under Full access, and show the provider's warning when an Allow always answer carries one
- Recover turns Antigravity reports as successful when the provider actually returned an API or quota error, with a localized message instead of a misleading answer
- Resume Antigravity tasks with their conversation intact, and import their native session history alongside the other ACP agents

## [0.1.44]

- Switch agents mid-task between turns: pick a different coding agent at any point without restarting the task. The transcript, workspace files, queued follow-ups, and checkpoints all stay intact
- Context handoff across agents: when switching to a different agent, the incoming model receives a tailored summary of earlier turns, executed commands, and modified files within its available context budget
- Separate conversation history per agent: each provider maintains its own resume cursor, so switching back to a previous agent resumes its native session rather than starting from scratch
- Agent attribution and independent rewinds: mixed transcripts clearly label each assistant response with its agent, and rewinding or forking a turn counts only the active agent's turns without corrupting other providers' history
- Consolidate navigation with a prominent "New Task" button in the sidebar header

## [0.1.43]

- Keep terminal text on its grid: CJK, emoji, and Nerd Font icons no longer push the rest of the line sideways, so the cursor, the selection, and the IME candidate window stay where the text actually is. Box-drawing and block characters are drawn as exact geometry, so TUI borders and panels meet without seams, and the bundled icon font renders on Windows and Linux without installing it
- Stop a flooding command from taking the app down with it: terminal output that outruns the connection is dropped and resynchronized instead of closing the socket, so agent turns and other terminals keep running
- Restore a terminal running a full-screen app — `vim`, `htop`, a full-screen agent TUI — with the screen underneath it and the shell's current directory intact
- Cut idle terminal cost: a pane no longer wakes the UI forty times a second while nothing is happening, and the bottom panel no longer rebuilds whenever another part of the window repaints
- Scroll terminal history from the keyboard with Shift+PageUp, Shift+PageDown, Shift+Home, and Shift+End
- Paste safely: pasted text can no longer break out of the paste envelope and run commands
- Read agent terminal output correctly: commands that print non-ASCII text no longer garble it, and the last lines of a command are no longer lost when it exits
- Inherit the whole Windows shell environment from the PowerShell profile, including proxies and toolchain variables beyond `PATH`

## [0.1.42]

- Restore a terminal after reconnect or a desktop restart and keep typing: live output is re-armed on attach, a colliding create no longer silences the existing shell, and Windows ConPTY input survives resize instead of going dead while the snapshot still looks alive. Closing a Windows terminal releases the console, and a terminal still opens when Windows will not let the shell join a job
- Restart a terminal whose shell had exited when the session comes back, and detect a shell that exits on its own on Windows ConPTY so the pane stops claiming to be live
- Move focus between split terminal panes with Alt+arrows, and reach mouse-aware terminal apps with the middle and right buttons. A new split opens in the directory the shell is actually in, and splitting the same way again shares that pane evenly
- Place CJK input-method candidates at the text caret on Windows, link and quote Windows paths with backslashes, and keep colors aligned on CJK text, box drawing, and emoji. File links open at the reported line and column, and beam and underline cursors follow the program
- Typing or pasting while scrolled up returns to the live prompt, and dragging a selection past the edge scrolls the history. On Windows and Linux, Ctrl+C copies when text is selected and still interrupts the shell when it is not
- Cut terminal overhead: a finished terminal frees its replay buffer, output takes one less copy, and snapshot rebuilds stop allocating per cell. Reconnect snapshots no longer pad history with trailing spaces, and very wide terminals can resize past the old column limit

## [0.1.40]

- Refresh the Files panel and the Review diff on their own while you work, using the workspace host's native filesystem events for local, SSH, and WSL workspaces; a workspace that cannot be watched says so and keeps Refresh as the recovery path
- Archive a workspace from its menu to hide it without losing anything, and restore it from the archived-workspaces list in the sidebar options
- Delete a workspace with its Waku session history and drafts after a confirmation that spells out what stays: project files on disk and provider-native history are never touched, and a workspace with running tasks must stop them first
- Show the newest notifications first with the rest a scroll away, keep each row on its own lines as the queue grows, and dismiss an opened notification so the next one moves up into its place

## [0.1.39]

- Add a VS Code-style changed-file tree to the Review panel and the Git commit page: folders sort before files, single-child folder chains merge into one row, and rows show vertical indent guides
- Add tree/list view toggles and collapse-all/expand-all buttons to both changed-file lists, with keyboard operation and localized tooltips (English, Simplified Chinese, Japanese)

## [0.1.38]

- Prompt to end incompatible daemon sessions and upgrade when connecting to a remote host speaking an older protocol version

## [0.1.37]

- Restore terminal sessions across reconnects and desktop restarts, keeping their running processes, output, and pane layout
- Move the New Task button to the conversation toolbar
- Improve Markdown rendering and text selection, including streamed formatting, tables, linked images, and math
- Remove the Environment section and its Git actions from the notification panel

## [0.1.36]

- Collect finished, failed, and waiting tasks, approval and input requests, and terminal alerts in a notification bell in the title bar; click an entry to jump to its task or terminal
- Show images in Markdown — transcripts, the file preview, and skills — loaded from the host the document lives on, so relative, workspace-root, and absolute paths work for SSH and WSL tasks too; web images are not fetched

## [0.1.35]

- Collect the sidebar, bottom terminal, and right panel toggles into a single title bar, and give the Files tree a pane of its own beside the workspace
- Open a commit from the Git graph as a full page — its message, changed files, and diff — with the arrow keys walking the files and Escape or Back returning to history
- Split the right panel's terminal into panes with the same chords, dividers, and context menu as the bottom panel, and keep the pane you clicked as the one splits and closes act on, focusing its neighbor rather than the first pane
- Add terminal output to the composer from the terminal's context menu, carrying the scrollback line numbers the selection came from
- Pin, archive, and collapse tasks in the sidebar, with archived tasks staying searchable
- Update ACP agents for structured diffs, images and other multimodal input, MCP servers, file and terminal access, and forking a session into a new one
- Resolve `~/` links in the transcript against your home directory instead of the workspace
- Close a deleted task's review and background-work tabs from the panel that is currently open

## [0.1.34]

- Right-click uncommitted files in the Git graph to stage, unstage, or discard them, see staged and unstaged badges on each file, and fetch, pull, or pop the last stash from the uncommitted-changes menu
- Create files and folders inline from the Files panel
- Open files at an exact spot: transcript links and the command palette's new Files section accept `:line`, `:line:column`, and `#L42C10` suffixes and land the caret there
- Search workspace files from the command palette and open them in the Files panel
- Keep the Skills and Usage settings pages per connected host, and scope workspace panel state (branches, git graph, files, terminal commands) to the host it belongs to
- Split or close terminal panes from the terminal's right-click menu

## [0.1.33]

- Expand any file in the Git graph's commit details to read its diff inline, and Ctrl/Cmd-click a second commit to compare the two
- Right-click commits, uncommitted changes, and branch, remote, or tag labels in the Git graph to check out, create branches and tags, cherry-pick, revert, merge, rebase, reset, stash, discard, push, rename, or delete; anything destructive asks first, and Shift+F10 opens the menu from the keyboard
- Report mouse clicks and scrolling to terminal apps that ask for them, support the kitty keyboard protocol and focus reports, answer color queries with the theme's real colors, and flag terminal panels that need attention when an agent there asks for it

## [0.1.32]

- Keep the bottom panel's terminal buttons on one row instead of letting them stack and overflow, and light up the documented split, resize, and close shortcuts there on Windows, including Windows Terminal's Shift+Alt+=, Shift+Alt+-, and Shift+Alt+arrow chords
- Recognize inline code file paths in the transcript as links that open the file in the Files panel

## [0.1.31]

- Keep workspace panels (Files, Git, and both terminals) across sessions tied to the workspace directory, and reorder header toggle buttons to lead with the bottom terminal

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
