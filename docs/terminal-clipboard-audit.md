# 终端剪贴板能力审核报告

- **项目**：Narel（上游源码与 crate 命名仍为 `waku`）桌面端内嵌终端
- **审核范围**：`src/terminal.rs`、`src/lib.rs`、`src/input.rs`、`src/app.rs`、`src/app/composer.rs`、`src/app/bottom_panel.rs`、`src/app/right_panel.rs`、`src/app/terminal_split.rs`、`crates/waku-core/src/terminal.rs`、`crates/waku-core/src/terminal/windows.rs`、`crates/waku-core/src/terminal_state.rs`、`crates/waku-core/src/attachments.rs`、`crates/waku-protocol/src/protocol.rs`、`Cargo.toml`
- **VT 引擎**：GPUI 桌面端为 `alacritty_terminal`；Web 端另用 `@wterm/ghostty`（见 `apps/web/src/components/right-panel.tsx`），移动端（iOS）另用 `libghostty`（见 `patches/expo-libghostty@0.8.1.patch`），后两者不在本报告范围内
- **方法**：静态代码审读 + 单元测试覆盖（`terminal::tests`）；端到端图形界面与实机 SSH/WSL 交互仍待运行时验证
- **日期**：2026-11-28

## 结论

**文本、图片与文件/目录剪贴板主链路已全部打通并具备安全防护：顶栏原生 `Edit` 菜单与快捷键/右键菜单全路径一致；Windows 下支持 `Ctrl+V`、`Shift+Insert`、`Ctrl+Insert`；远端终端已复用 `ImportAttachment` 支持普通文件与目录上传；bracketed paste 做了换行归一化与逃逸清洗，并对超大文本执行 16 MiB 上限与 64 KiB UTF-8 边界分块。协议层基于安全考量刻意保持 OSC 52 只写不读，暂不实现 OSC 5522。**

评级：

| 维度 | 评级 | 说明 |
| --- | --- | --- |
| 复制 | 完整 | 平台化快捷键（含 `Ctrl+Insert`）、顶栏 `Edit` 菜单、右键菜单、全选、选区入 composer 齐全 |
| 粘贴（文本） | 完整 | 支持 Windows `Ctrl+V` / `Shift+Insert`、顶栏 `Edit -> Paste`、换行归一化、bracketed paste 清洗与分块上限 |
| 粘贴（图片/文件） | 完整 | 全平台支持 BMP/TIFF→PNG 重编码、`ExternalPaths` 优先、本地路径引用与远端上传（图片 + 文件 + 目录） |
| 跨主机（SSH/远程 daemon） | 部分 | 文本、剪贴板图片、图片文件、普通文件与目录均可贴；OSC 52 读取出于安全默认不启用 |
| 协议兼容 | 部分 | 缺 OSC 52 读（刻意忽略）、OSC 5522、copy-on-select 等 |
| 安全性 | 良好 | 双重回放保护不污染剪贴板、bracketed paste 清洗 ESC / C1 CSI / `\x1b[201~`、禁止任意命令读系统剪贴板 |

## 覆盖矩阵

| 能力 | 状态 | 代码位置 |
| --- | --- | --- |
| 复制选区（Cmd+C / Ctrl+Shift+C / Ctrl+Insert） | 支持 | `src/terminal.rs:79-107`、`2663-2681` |
| 非 macOS 下 Ctrl+C 有选区即复制 | 支持 | `src/terminal.rs:94-103`、`2663-2677` |
| 全选（Cmd+A / Ctrl+Shift+A） | 支持 | `src/terminal.rs:70-77`、`2682-2690` |
| 右键菜单 Copy / Paste / Select all（含会话存活检查） | 支持 | `src/terminal.rs:3663-3711` |
| 顶栏原生 Edit 菜单 Copy / Paste / Select all | 支持 | `src/lib.rs:528-540`、`src/terminal.rs:3873-3877` |
| 选区“Add to composer” | 支持 | `src/terminal.rs:3694-3701` |
| 粘贴文本（Cmd+V / Ctrl+Shift+V / Windows Ctrl+V / Shift+Insert） | 支持 | `src/terminal.rs:109-140`、`2695-2700`、`3094-3115` |
| bracketed paste 换行归一化 + ESC/C1 CSI 清洗 | 支持 | `src/terminal.rs:4357-4369` |
| 超大粘贴 16 MiB 拦截 + 64 KiB UTF-8 边界分块 | 支持 | `src/terminal.rs:142-143`、`3102-3114`、`4372-4389` |
| 粘贴图片（全平台 BMP/TIFF→PNG，落盘后贴路径） | 支持 | `src/terminal.rs:1986-2046`、`3177-3197`、`Cargo.toml:179,188` |
| 双格式剪贴板优先取文件路径（保留原路径与格式） | 支持 | `src/terminal.rs:1895-1945` |
| 粘贴文件与目录（本地拖拽/复制路径） | 支持 | `src/terminal.rs:3122-3131`、`3870` |
| 远程主机粘贴/拖拽图片、普通文件与目录上传 | 支持 | `src/terminal.rs:2082-2125`、`3134-3155`、`src/app/composer.rs:4853` |
| OSC 52 写入（程序→系统剪贴板） | 支持 | `src/terminal.rs:1112-1117`、`2567-2569` |
| OSC 52 读取（程序←系统剪贴板） | **刻意忽略** | `src/terminal.rs:1118` |
| Kitty 剪贴板协议（OSC 5522） | **暂不实现** | 全仓无相关实现 |
| copy-on-select（选中即复制） | **不支持** | — |
| 中键粘贴（primary selection） | **不支持** | — |
| 复制非文本内容（图形/图片） | **不支持** | `new_string` 仅文本 |

## 已实现的能力

1. **平台化快捷键与习惯快捷键**。macOS 用 `Cmd+C/V/A`；Linux 用 `Ctrl+Shift+C/V/A`、`Ctrl+Insert`（复制）、`Shift+Insert`（粘贴），保留 `Ctrl+V` 发送 `^V`；Windows 在此基础上支持 `Ctrl+V` 直接粘贴（`terminal_copy_shortcut_for` / `terminal_paste_shortcut_for`，`src/terminal.rs:79-140`、`2663-2700`）。
2. **Ctrl+C 的智能分流**。非 macOS 下若存在选区，`Ctrl+C` 执行复制并清除选区；无选区时才交给 shell 发 SIGINT（`src/terminal.rs:2663-2677`），而 `Ctrl+Shift+C` / `Cmd+C` / `Ctrl+Insert` 复制后保留选区。
3. **右键菜单与顶栏原生 Edit 菜单**。`TerminalView` 挂载了 `input::Copy` / `input::Paste` / `input::SelectAll` 的 `on_action` 处理器（`src/terminal.rs:3873-3877`），顶栏 `Edit` 菜单（`src/lib.rs:528-540`）在终端聚焦时可直接触发；右键菜单中 `Paste` 与 `Select all` 均校验 `has_session && !exited`（`src/terminal.rs:3663-3711`）。
4. **粘贴内容分级（与 `input.rs` 对齐）**。当剪贴板首项为媒体类型时优先取非空 `ExternalPaths`（解决 Windows 复制图片文件同时携带 DIB 与文件路径时丢失原路径的问题），无路径时取 `Image`；否则优先取非空文本，再回退路径与图片（`src/terminal.rs:1895-1945`）。
5. **bracketed paste 清洗与换行归一化**。开启 `TermMode::BRACKETED_PASTE` 时将 `\r\n` 归一为 `\n`，剥离 `\x1b[201~`、`\x1b` 与 8-bit C1 CSI `\u{009b}` 后包裹 `\x1b[200~…\x1b[201~`；未开启时将 `\r\n` / `\n` 统一转换为 `\r`（`src/terminal.rs:4357-4369`）。IME 提交路径（`EntityInputHandler::replace_text_in_range`）保持直接写入字节。
6. **超大粘贴上限与 UTF-8 安全分块**。单次文本粘贴超过 `16 MiB` 时拒绝写入并弹出 `TerminalNotice` 提示（`terminal.paste_too_large`）；上限内文本仅包裹一层信封，并按 `64 KiB` 在 UTF-8 字符边界切块写入，避免单条 `Command::WriteTerminal` 逼近 `48 MiB` 协议帧上限或在多字节字符中间切断（`src/terminal.rs:3094-3115`、`4372-4389`）。
7. **图片粘贴与全平台 BMP/TIFF 转码**。`Cargo.toml:179,188` 在 Linux 与 Windows 均启用 `["png", "bmp", "tiff"]`（macOS 启用完整 `image`），剪贴板中的 BMP/TIFF 统一重编码为 PNG；本地落盘后贴路径，远端通过 `Command::StoreBlob` 上传。
8. **文件与目录粘贴（本地与远端全打通）**。本地 daemon 直接粘贴经 `join_shell_paths_for_target` 引用的路径；远端 daemon 走 `upload_pasted_paths`（`src/terminal.rs:2082-2125`、`3134-3155`），图片文件经 `StoreBlob` 上传，普通文件与目录复用 `attachment_upload_from_path` + `Command::ImportAttachment` 上传（支持单文件/目录 32 MiB、4096 文件限制与 1 小时空闲清理宽限期），失败项计入 `skipped` 并提示 `terminal.paste_files_skipped`。
9. **OSC 52 写入与双重回放保护**。远端程序（tmux、vim）可将文本写入系统剪贴板；客户端 `replaying` 标志（`src/terminal.rs:1085-1091`）与 daemon 侧 `ShadowEventProxy`（`crates/waku-core/src/terminal_state.rs:86-112`）双重丢弃历史回放中的剪贴板事件。
10. **粘贴前复位视图**。粘贴前清除选区、把视口滚回底部（非 alt screen 时），避免粘贴到不可见位置（`src/terminal.rs:1358-1368`）。

## 保留边界 / 暂不实现

### 1. OSC 52 读取（`Event::ClipboardLoad`）刻意保持忽略

`src/terminal.rs:1118` 对 `Event::ClipboardLoad(_, _)` 空实现，**不计划新增常驻 `allow` 开关**：

- **安全风险**：Narel 终端常由 Coding Agent 自动执行命令、测试与第三方脚本；若允许程序通过 `\x1b]52;c;?\x07` 读取系统剪贴板，任意终端输出或提示注入均可静默窃取本机剪贴板中的 Token/密码并回灌远端 stdin。
- **实用必要性低**：SSH / tmux / vim 场景下，向远端“粘贴进来”直接通过终端自身的 `BRACKETED_PASTE` 即可完成；“复制出来”已由 OSC 52 写入覆盖。

### 2. Kitty 剪贴板协议（OSC 5522）暂不实现

现有“图片落盘/上传 + 贴路径”对 Coding Agent CLI 与远端 shell 的兼容性更好；OSC 5522 在跨 SSH/tmux 场景收益有限，暂不投入。

### 3. 可选桌面交互细节（低优先级）

- `copy-on-select`（选中即复制）与 X11 中键粘贴（primary selection）当前未实现，视后续 Linux 用户反馈按需评估。
- OSC 52 写入（`Event::ClipboardStore`）沿用主流终端默认无确认行为，且已受双重回放保护约束。

## 测试覆盖

`src/terminal.rs` 中包含以下相关单元测试（全平台执行）：

- `terminal_clipboard_shortcuts_match_platform_conventions`（`6277`）：覆盖 macOS / Windows / Linux 下 `Cmd+C/V`、`Ctrl+C`（有/无选区）、`Ctrl+Shift+C/V`、`Ctrl+V`、`Ctrl+Insert`、`Shift+Insert`
- `pasted_bmp_is_re_encoded_as_png`（`6631`）：已移除非 Linux 限制，全平台验证 BMP→PNG 重编码
- `copied_files_and_directories_upload_and_missing_paths_are_skipped`（`6646`）：验证图片文件、普通文本文件与子目录上传及缺失路径跳过计数
- `quotes_terminal_paths_only_when_needed`（`6688`）：验证 POSIX/Windows 路径引用
- `extracts_terminal_paste_content_with_correct_priority`（`6719`）：验证文本、图片、路径以及 `[Image, ExternalPaths]` / `[Text, Image]` 双格式优先级
- `wraps_bracketed_paste_only_when_requested`（`6546`）与 `bracketed_paste_strips_injected_escape_sequences`（`7048`）：验证 `\r\n` 归一化、ESC / `\u{009b}` / `\x1b[201~` 剥离
- `utf8_byte_chunks_keep_multibyte_characters_intact`（`7063`）与 `terminal_paste_chunks_large_payloads_and_rejects_oversized_text`（`7076`）：验证多字节字符切块不断字、单层信封包裹与 16 MiB 超限拦截
- `edit_menu_actions_copy_paste_and_select_all_when_terminal_is_focused`（`7193`）：GPUI 窗口测试，验证聚焦终端时 `input::Copy` / `input::Paste` / `input::SelectAll` 可用且分发正确

## 限制

本报告的交互行为结论基于静态代码审读与单元测试（`PtyInput::Test` / `TerminalLauncher::Local`），尚未在运行中的桌面窗口内实测 Windows `Ctrl+V`、顶栏原生菜单点击及跨主机 SSH/WSL `ImportAttachment` 实机传输表现。
