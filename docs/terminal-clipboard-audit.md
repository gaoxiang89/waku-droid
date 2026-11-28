# 终端剪贴板能力审核报告

- **项目**：Narel（上游源码与 crate 命名仍为 `waku`）桌面端内嵌终端
- **审核范围**：`src/terminal.rs`、`src/lib.rs`、`src/input.rs`、`src/app/bottom_panel.rs`、`src/app/right_panel.rs`、`src/app/terminal_split.rs`、`crates/waku-core/src/terminal.rs`、`crates/waku-core/src/terminal/windows.rs`、`crates/waku-core/src/terminal_state.rs`、`crates/waku-protocol/src/protocol.rs`、`Cargo.toml`
- **VT 引擎**：GPUI 桌面端为 `alacritty_terminal`；Web 端另用 `@wterm/ghostty`（见 `apps/web/src/components/right-panel.tsx`），移动端（iOS）另用 `libghostty`（见 `patches/expo-libghostty@0.8.1.patch`），后两者不在本报告范围内
- **方法**：静态代码审读，未做运行时交互验证
- **日期**：2026-11-28

## 结论

**文本剪贴板主链路完整且安全（但未接通顶栏原生 `Edit` 菜单 Action）；本地图片/文件通过“落盘 + 贴路径”绕开协议缺口，远端（SSH/WSL daemon）仅上传剪贴板图片与图片文件，非图片文件一律跳过；协议级兼容不完整——OSC 52 只写不读，未实现 Kitty 剪贴板协议（OSC 5522）。**

评级：

| 维度 | 评级 | 说明 |
| --- | --- | --- |
| 复制 | 基本完整 | 平台化快捷键、右键菜单、全选、选区入 composer 齐全；顶栏 `Edit -> Copy / Select All` 未接通 |
| 粘贴（文本） | 基本完整 | 支持 bracketed paste 与逃逸清洗；缺顶栏 `Edit -> Paste`、Windows `Ctrl+V`、`Shift+Insert` |
| 粘贴（图片/文件） | 部分 | 本地支持图片与任意文件路径；Linux 桌面下 BMP/TIFF 不重编码为 PNG；远端仅上传图片文件，非图片文件跳过 |
| 跨主机（SSH/远程 daemon） | 部分 | 文本与图片可贴，非图片文件跳过不上传；OSC 52 读取不可用 |
| 协议兼容 | 部分 | 缺 OSC 52 读、OSC 5522、copy-on-select 等 |
| 安全性 | 良好 | 双重回放保护不污染剪贴板、bracketed paste 内容不能逃出信封 |

## 覆盖矩阵

| 能力 | 状态 | 代码位置 |
| --- | --- | --- |
| 复制选区（Cmd+C / Ctrl+Shift+C） | 支持 | `src/terminal.rs:53-60`、`2545-2564` |
| 非 macOS 下 Ctrl+C 有选区即复制 | 支持 | `src/terminal.rs:2545-2561` |
| 全选（Cmd+A / Ctrl+Shift+A） | 支持 | `src/terminal.rs:53-60`、`2566-2573` |
| 右键菜单 Copy / Paste / Select all | 支持（Paste 未校验会话状态） | `src/terminal.rs:3533-3580` |
| 顶栏原生 Edit 菜单 Copy / Paste / Select all | **不支持** | `src/lib.rs:528-540`、`src/terminal.rs:3647-3751`、`4464-4480` |
| 选区“Add to composer” | 支持 | `src/terminal.rs:3563-3570` |
| 粘贴文本（Cmd+V / Ctrl+Shift+V） | 支持 | `src/terminal.rs:2578-2585`、`2979-2987` |
| bracketed paste 包裹 + ESC 清洗 | 支持 | `src/terminal.rs:4227-4236` |
| 粘贴图片（macOS/Windows 下 BMP/TIFF→PNG，落盘后贴路径；Linux 保持原格式） | 部分（Linux 不转码） | `src/terminal.rs:1889-1953`、`3052-3072`、`Cargo.toml:179,188` |
| 粘贴文件（本地拖拽/复制路径） | 支持 | `src/terminal.rs:2997-3007`、`3745` |
| 远程主机粘贴/拖拽图片文件上传 | 支持 | `src/terminal.rs:1965-2004`、`3010-3031` |
| 远程主机粘贴/拖拽非图片文件上传 | **不支持（跳过并提示计数）** | `src/terminal.rs:1988-2004`、`3023-3028` |
| OSC 52 写入（程序→系统剪贴板） | 支持 | `src/terminal.rs:1029-1034`、`2449-2451` |
| OSC 52 读取（程序←系统剪贴板） | **不支持** | `src/terminal.rs:1035` |
| Kitty 剪贴板协议（OSC 5522） | **不支持** | 全仓无相关实现 |
| copy-on-select（选中即复制） | **不支持** | — |
| Shift+Insert / Ctrl+Insert | **不支持** | — |
| 中键粘贴（primary selection） | **不支持** | — |
| 复制非文本内容（图形/图片） | **不支持** | `new_string` 仅文本 |

## 已实现的能力

1. **平台化快捷键**。macOS 用 `Cmd+C/V/A`；其他平台用 `Ctrl+Shift+C/V/A`，把 `Ctrl+C` 留给 SIGINT（`terminal_clipboard_modifier_pressed`，`src/terminal.rs:53-60`、`2545-2585`）。
2. **Ctrl+C 的智能分流**。非 macOS 下若存在选区，`Ctrl+C` 执行复制并清除选区；无选区时才交给 shell 发 SIGINT（`src/terminal.rs:2545-2561`）。这与 Windows Terminal 的行为接近。
3. **右键菜单**。Copy / Add to composer / Paste / Select all，剪贴板无可粘贴内容时 Paste 置灰（`src/terminal.rs:3533-3580`）；`Paste` 仅检查 `can_paste` 而未校验 `has_session` / `!exited`（点击后在 `paste_from_clipboard` 内部对无会话安全空返回）。
4. **粘贴内容分级**。按首项图片 → 首项外部路径 → 文本的顺序优先取内容，并回退扫描其余 entry（`src/terminal.rs:1816-1856`）。
5. **bracketed paste 安全处理**。程序声明 `TermMode::BRACKETED_PASTE` 时包裹 `\x1b[200~…\x1b[201~`，同时剥除 `\x1b[201~` 与 ESC（`\x1b`），粘贴内容无法提前闭合信封执行命令（`src/terminal.rs:4227-4236`）；未开启该模式时，以及 IME 提交路径（`EntityInputHandler::replace_text_in_range`，`src/terminal.rs:3221-3238`），字节直接写入 PTY。
6. **图片粘贴**。macOS 与 Windows 下剪贴板中的 BMP/TIFF 会重新编码为 PNG（agent CLI 只接受 PNG/JPEG），写入临时文件后粘贴 shell 引用过的路径；Linux 因 `Cargo.toml:179` 的 `image` 仅启用 `["png"]`，BMP/TIFF 解码失败后按原格式落盘。远程 daemon 场景通过 `Command::StoreBlob` 先上传再贴远端路径（`src/terminal.rs:1889-1953`、`3052-3072`）。
7. **文件粘贴**。本地 daemon（`!daemon.is_remote()`）直接粘贴经 `join_shell_paths_for_target` 引用的本地路径（`src/terminal.rs:2997-3007`）；远程 daemon 走 `upload_pasted_paths`（`src/terminal.rs:1988-2004`、`3010-3031`），仅识别图片扩展名并上传到远端 `BlobStore`，非图片文件全部计入 `skipped` 并弹提示（`terminal.paste_files_skipped`）。
8. **OSC 52 写入**。远程程序（tmux、vim）能把内容写进系统剪贴板（`src/terminal.rs:1029-1034`、`2449-2451`）。
9. **双重回放保护**。增量原始字节重放时，客户端 `TerminalEventProxy` 通过 `replaying` 标志（`src/terminal.rs:665-683`、`1003-1009`）丢弃除标题/唤醒外的事件；滚动缓冲区溢出后由 daemon 侧合成快照时，`ShadowEventProxy`（`crates/waku-core/src/terminal_state.rs:86-112`）同样忽略剪贴板相关事件，历史内容不会覆盖用户剪贴板。
10. **粘贴前复位视图**。粘贴前清除选区、把视口滚回底部（非 alt screen 时），避免粘贴到不可见位置（`src/terminal.rs:1275-1285`）。

## 问题清单

### P1（中）顶栏原生 `Edit` 菜单在终端聚焦时无效

`src/lib.rs:528-540` 中应用菜单栏 `Edit -> Copy / Paste / Select All` 绑定的是 `input::Copy` / `input::Paste` / `input::SelectAll`（`src/input.rs:62-65`），而 `TerminalView`（`src/terminal.rs:3647-3751`）只在 `on_key_down` 中手写拦截按键，未注册对应 Action 的 `on_action` 处理器；同时 `shield_shell_keystrokes`（`src/terminal.rs:4464-4480`）还会将 `Waku` 根层的 `secondary-c`（`CopySelection`）在 `Terminal` 上下文中屏蔽为 `NoAction`。

- 影响：键盘快捷键正常，但点击顶栏菜单 `Edit -> Copy / Paste / Select All` 对聚焦终端完全无效。
- 附带细节：右键菜单的 `Paste`（`src/terminal.rs:3539-3574`）只检查 `can_paste` 而未检查 `has_session && !exited`，与 `Select all` 的禁用条件不一致。
- 建议：在 `TerminalView` 上补充 `on_action` 处理 `input::Copy` / `input::Paste` / `input::SelectAll`（并视需要处理右键 `Paste` 的会话存活检查）。

### P2（中）Windows 上 `Ctrl+V` 不粘贴

`Ctrl+Shift+V` 才是粘贴，`Ctrl+V` 会被原样送给 shell（`src/terminal.rs:2578-2585` 只认 `terminal_clipboard_modifier_pressed`）。静态代码已核实：应用层 `input::Paste`（`secondary-v`）仅绑定在 `TextInput` 上下文（`src/input.rs:107`），`Terminal` 聚焦时 `terminal_uses_text_input`（`src/terminal.rs:4238-4249`）因 `modifiers.control` 为真而返回 `false`，因此 `Ctrl+V` 目前会直接进入 `on_key_down` 并编码为 `0x16`（`^V`）或 Kitty 序列 `\x1b[118;5u` 发送给 PTY。

- 附带的不对称：`Ctrl+C` 在有选区时会复制，`Ctrl+V` 却不会粘贴；这与 Windows Terminal、VS Code 终端默认体验不一致。
- 关键权衡：若在 Windows 上无条件把 `Ctrl+V` 改为粘贴，会吞掉 Vim / Neovim 的 `Ctrl+V`（Visual Block 列块选择）以及 shell readline 的 `quoted-insert`（`^V`）。
- 建议：在 Windows 上支持 `Ctrl+V` 粘贴（保留 `Ctrl+Shift+V`），并提供配置开关或考虑在 `ALT_SCREEN` / Kitty 键盘协议激活时透传给程序。

### P3（中）OSC 52 读取被静默忽略

`src/terminal.rs:1035` 对 `Event::ClipboardLoad(_, _)` 直接空实现，因此 SSH/tmux/vim 里依赖读取系统剪贴板（OSC 52 query）的路径全部失效，用户只能退回终端自身的粘贴。

- 影响：远程开发体验；这类场景下“复制出去”可用，“粘贴进来”需换路径。
- 说明：忽略读取是常见的安全默认值（kitty 默认拒绝，Ghostty 默认询问），但当前实现既没有开关也没有提示。
- 建议：在项目设置（如 `~/.narel/settings.json` 或 `app.json`）中新增配置项（如 `terminal_clipboard_read`，取值 `deny` / `ask` / `allow`），默认 `deny`；`allow` 时把系统剪贴板内容通过 `Event::ClipboardLoad` 的 formatter 回写。

### P4（低）缺少习惯性快捷键与交互

- 无 `Shift+Insert` / `Ctrl+Insert`（Linux/Windows 用户的高频肌肉记忆）。
- 无 copy-on-select（iTerm2、Windows Terminal 均可选）。
- 无中键粘贴（X11 primary selection）。
- 建议：`Shift+Insert` / `Ctrl+Insert` 成本最低，可优先补。

### P5（低）超大粘贴缺少终端层阈值保护或分块

`paste()` 把整段文本一次性交给 `session.write`（`src/terminal.rs:2979-2987`）。桌面端正式终端（包括本地工作区，`src/app/bottom_panel.rs:919`、`src/app/right_panel.rs:4650`）全部通过 `TerminalLauncher::Daemon` 发送 `Command::WriteTerminal`（`TerminalLauncher::Local` 与 `PtyInput::Local` 仅测试路径使用）；daemon 侧在 Unix 用 `write_all_evented`（`crates/waku-core/src/terminal.rs:223`）、Windows 用独立输入线程 `write_all` + `flush`（`crates/waku-core/src/terminal/windows.rs:326`）写入 PTY，不会卡死 UI 线程。但协议层有 `MAX_WIRE_MESSAGE_BYTES = 48 MiB` 的单帧硬上限（`crates/waku-protocol/src/protocol.rs:34`，base64 编码后约合 ~36 MiB 原始粘贴），超过会直接失败，且数兆级别的单次写入缺少显式确认或分块保护。

- 建议：超过阈值（如 1 MiB）时弹出确认或分块发送，并在超限时给出提示。

### P6（低）远端终端不支持非图片文件粘贴上传，且 Linux 剪贴板 BMP/TIFF 不转 PNG

- **远端非图片文件被跳过**：`upload_pasted_paths`（`src/terminal.rs:1988-2004`）复用图片 `Command::StoreBlob` 通道，拖入或粘贴 `.txt`、`.rs`、`.pdf` 等非图片文件到 SSH/WSL 终端时全部计入 `skipped`，仅图片可上传。
- **Linux BMP/TIFF 不转码**：`Cargo.toml:179` 仅启用了 `image` 的 `["png"]` feature，Linux 下若剪贴板图像为 BMP 或 TIFF，`paste_image_as_png` 会解码失败并原样落盘。

### P7（低）未实现 Kitty 剪贴板协议（OSC 5522）

程序无法以 paste event 的形式拿到多 MIME 内容（图片、富文本、`text/uri-list`）。当前的“存文件 + 贴路径”在本地与远端图片场景更实用，但如果目标程序主动走 OSC 5522 协商剪贴板，这条路径走不通。

- 建议：保持现状即可；若要做，优先级低于 P1–P3。

### P8（信息）OSC 52 写入无用户确认

任何程序（含远程会话中的程序）都能静默覆盖系统剪贴板。这是多数终端的默认行为，且双重回放保护已经挡掉历史输出误写；若后续要收紧，可与 P3 的开关合并设计。

## 优点

- 粘贴内容在 `BRACKETED_PASTE` 下做了 ESC 与 `\x1b[201~` 清洗，直接阻断“粘贴即执行”的注入路径，并有对应测试（`src/terminal.rs:6771`）。
- 客户端 `replaying` 标志与 daemon 侧 `ShadowEventProxy` 双重拦截历史回放剪贴板事件，避免恢复会话时覆盖用户剪贴板。
- macOS/Windows 下剪贴板图片自动转 PNG 并落盘，规避了 agent CLI 只认 PNG/JPEG 的兼容问题。
- 远端场景对剪贴板图片与图片文件做了 `StoreBlob` 上传，对无法上传的非图片文件有明确的跳过计数提示。
- 复制/粘贴/全选/入 composer 均同时提供了面板内快捷键与右键菜单（仅顶栏原生 `Edit` 菜单待接通）。

## 测试覆盖

已有相关用例（均位于 `src/terminal.rs`）：

- `wraps_bracketed_paste_only_when_requested`（`6300`）
- `bracketed_paste_strips_injected_escape_sequences`（`6771`）
- `pasted_bmp_is_re_encoded_as_png`（`6387`，仅非 Linux 平台：`#[cfg(not(target_os = "linux"))]`）
- `extracts_terminal_paste_content_with_correct_priority`（`6468`）
- `linux_terminal_clipboard_shortcuts_preserve_ctrl_c_for_sigint`（`6124`，仅 Linux 平台：`#[cfg(target_os = "linux")]`）
- `copied_image_files_upload_and_other_files_are_skipped`（`6402`）与 `quotes_terminal_paths_only_when_needed`（`6437`）

建议补充：

1. 顶栏 `Edit` 菜单 Action（`input::Copy` / `input::Paste` / `input::SelectAll`）在聚焦 `TerminalView` 时的分发测试。
2. Windows 下 `Ctrl+V` 粘贴、`Ctrl+Shift+V` 仍粘贴，以及在未启用或透传模式下不吞掉 `^V` 的按键测试。
3. OSC 52 读取在 `deny` / `allow` 配置下的行为，以及回放期间始终保持忽略。
4. 超大粘贴的分块或确认阈值路径。

## 建议优先级

1. **P1** 接通顶栏原生 `Edit` 菜单 Action + 补齐右键 `Paste` 置灰条件——零交互副作用、补齐菜单一致性。
2. **P2** Windows `Ctrl+V` 粘贴——改动小、收益直接（需兼顾 Vim `^V` / readline 透传开关或模式判断）。
3. **P3** OSC 52 读取开关——解锁 SSH/tmux 场景，默认关闭不影响安全基线。
4. **P4** `Shift+Insert` / `Ctrl+Insert`——低成本补齐习惯。
5. **P5** 超大粘贴分块或确认阈值。
6. **P6 / P7** 按需补齐远端通用文件上传、Linux BMP/TIFF 转码与 OSC 5522。

## 限制

本报告基于静态代码审读，未在运行中的 app 内实测各平台快捷键与远程粘贴流程；静态代码已确认应用层 `KeyBinding` 未在 `Terminal` 上下文中拦截 Windows `Ctrl+V`（`input::Paste` 仅限 `TextInput`），但平台窗口层的按键投递细节及 SSH/WSL 实机端到端粘贴体验仍需运行时验证。
