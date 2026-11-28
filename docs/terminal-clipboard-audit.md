# 终端剪贴板能力审核报告

- **项目**：Waku（内部代号 `narel`）桌面端内嵌终端
- **审核范围**：`src/terminal.rs`、`crates/waku-core/src/terminal.rs`、`crates/waku-core/src/terminal_state.rs`、`src/app/terminal_split.rs`
- **VT 引擎**：桌面端为 `alacritty_terminal`；移动端（iOS）另用 `libghostty`（见 `patches/expo-libghostty@0.8.1.patch`，与本报告无关）
- **方法**：静态代码审读，未做运行时交互验证
- **日期**：2026-11-28

## 结论

**文本剪贴板：完整且安全。图片/文件：以“落盘 + 贴路径”绕开协议缺口，对 agent CLI 反而更实用。协议级兼容：不完整——OSC 52 只写不读，未实现 Kitty 剪贴板协议（OSC 5522）。**

评级：

| 维度 | 评级 | 说明 |
| --- | --- | --- |
| 复制 | 完整 | 平台化快捷键、右键菜单、全选、选区入 composer |
| 粘贴（文本） | 完整 | 支持 bracketed paste，且做了逃逸清洗 |
| 粘贴（图片/文件） | 完整 | BMP→PNG、临时文件、远程上传、失败提示 |
| 跨主机（SSH/远程 daemon） | 部分 | 粘贴方向可用；OSC 52 读取不可用 |
| 协议兼容 | 部分 | 缺 OSC 52 读、OSC 5522、copy-on-select 等 |
| 安全性 | 良好 | 回放不污染剪贴板、粘贴内容不能逃出信封 |

## 覆盖矩阵

| 能力 | 状态 | 代码位置 |
| --- | --- | --- |
| 复制选区（Cmd+C / Ctrl+Shift+C） | 支持 | `src/terminal.rs:53-60`、`2545-2564` |
| 非 macOS 下 Ctrl+C 有选区即复制 | 支持 | `src/terminal.rs:2545-2561` |
| 全选（Ctrl+Shift+A） | 支持 | `src/terminal.rs:2566-2573` |
| 右键菜单 Copy / Paste / Select all | 支持 | `src/terminal.rs:3533-3580` |
| 选区“Add to composer” | 支持 | `src/terminal.rs:3563-3570` |
| 粘贴文本（Cmd+V / Ctrl+Shift+V） | 支持 | `src/terminal.rs:2578-2585`、`2979-2987` |
| bracketed paste 包裹 + ESC 清洗 | 支持 | `src/terminal.rs:4227-4236` |
| 粘贴图片（BMP→PNG，落盘后贴路径） | 支持 | `src/terminal.rs:1889-1960`、`3052-3072` |
| 粘贴文件（拖拽/复制路径） | 支持 | `src/terminal.rs:2997-3032`、`3745` |
| 远程主机粘贴时先上传文件 | 支持 | `src/terminal.rs:3010-3031` |
| OSC 52 写入（程序→系统剪贴板） | 支持 | `src/terminal.rs:1029-1034`、`2449-2451` |
| OSC 52 读取（程序←系统剪贴板） | **不支持** | `src/terminal.rs:1035` |
| Kitty 剪贴板协议（OSC 5522） | **不支持** | 全仓无相关实现 |
| copy-on-select（选中即复制） | **不支持** | — |
| Shift+Insert / Ctrl+Insert | **不支持** | — |
| 中键粘贴（primary selection） | **不支持** | — |
| 复制非文本内容（图形/图片） | **不支持** | `new_string` 仅文本 |

## 已实现的能力

1. **平台化快捷键**。macOS 用 `Cmd+C/V`；其他平台用 `Ctrl+Shift+C/V`，把 `Ctrl+C` 留给 SIGINT（`terminal_clipboard_modifier_pressed`，`src/terminal.rs:53-60`）。
2. **Ctrl+C 的智能分流**。非 macOS 下若存在选区，`Ctrl+C` 执行复制并清除选区；无选区时才交给 shell 发 SIGINT（`2545-2561`）。这与 Windows Terminal 的行为接近。
3. **右键菜单**。Copy / Add to composer / Paste / Select all，剪贴板无可粘贴内容时 Paste 置灰（`3533-3580`）。
4. **粘贴内容分级**。按 图片 → 文件路径 → 文本 的优先级取内容，并回退扫描其余 entry（`1816-1856`）。
5. **bracketed paste 安全处理**。程序声明 `BRACKETED_PASTE` 时包裹 `\x1b[200~…\x1b[201~`，同时剥除 ESC 与结束标记，粘贴内容无法提前闭合信封执行命令（`4227-4236`）。
6. **图片粘贴**。剪贴板中的 BMP 会重新编码为 PNG（agent CLI 只接受 PNG/JPEG），写入临时文件后粘贴 shell 引用过的路径；远程 daemon 场景先上传再贴远端路径（`1889-1960`、`3052-3072`）。
7. **文件粘贴**。拖拽或复制的文件粘贴为引用路径；远程场景先上传，跳过的文件数量会提示（`2997-3032`）。
8. **OSC 52 写入**。远程程序（tmux、vim）能把内容写进系统剪贴板（`1029-1034`、`2449-2451`）。
9. **回放保护**。重放历史输出时，除标题类事件外一律丢弃，历史内容不会重新触发剪贴板写入、设备查询或通知（`1003-1009`）。这是很多终端会踩的坑。
10. **粘贴前复位视图**。粘贴前清除选区、把视口滚回底部（非 alt screen 时），避免粘贴到不可见位置（`1275-1285`）。

## 问题清单

### P1（中）OSC 52 读取被静默忽略

`src/terminal.rs:1035` 对 `Event::ClipboardLoad(_, _)` 直接空实现，因此 SSH/tmux/vim 里 `clipboard=unnamed`、`"+y` 等依赖读取系统剪贴板的路径全部失效，用户只能退回终端自身的粘贴。

- 影响：远程开发体验；这类场景下“复制出去”可用，“粘贴进来”需换路径。
- 说明：忽略读取是常见的安全默认值（kitty 默认拒绝，Ghostty 默认询问），但当前实现既没有开关也没有提示。
- 建议：新增设置（如 `amp.terminal.clipboardRead`，取值 `deny`/`ask`/`allow`），默认 `deny`；`allow` 时把系统剪贴板内容通过 `Event::ClipboardLoad` 的 formatter 回写。

### P2（中）Windows 上 Ctrl+V 不粘贴

`Ctrl+Shift+V` 才是粘贴，`Ctrl+V` 会被原样送给 shell（`2578-2585` 只认 `terminal_clipboard_modifier_pressed`）。Windows Terminal 与 VS Code 终端都是 `Ctrl+V` 直接粘贴，这里与平台习惯不一致。

- 附带的不对称：`Ctrl+C` 在有选区时会复制，`Ctrl+V` 却不会粘贴。
- 建议：仅在 Windows 上把 `Ctrl+V` 绑定到粘贴（`Ctrl+Shift+V` 保留），`Ctrl+Shift+key` 仍按现有逻辑透传给 shell；或做成可配置项。

### P3（低）未实现 Kitty 剪贴板协议（OSC 5522）

程序无法以 paste event 的形式拿到多 MIME 内容（图片、富文本、`text/uri-list`）。当前的“存文件 + 贴路径”是更好的兼容路线，但如果目标程序主动读剪贴板（部分 agent 会这么做），这条路径走不通。

- 建议：保持现状即可；若要做，优先级低于 P1。

### P4（低）缺少习惯性快捷键与交互

- 无 `Shift+Insert` / `Ctrl+Insert`（Linux/Windows 用户的高频肌肉记忆）。
- 无 copy-on-select（iTerm2、Windows Terminal 均可选）。
- 无中键粘贴（X11 primary selection）。
- 建议：`Shift+Insert` 成本最低，可优先补。

### P5（低）粘贴无大小上限

`paste()` 把整段文本一次性交给 `session.write`（`2979-2987`）。UI 线程不受影响（本地走 channel，远端走 `Command::WriteTerminal` 通知，daemon 侧 `write_all_evented` 处理部分写与背压），但超大粘贴会作为单条协议消息发送，缺少显式上限或分块。

- 建议：超过阈值（如 1 MiB）时确认或分块发送，并在超限时给出提示。

### P6（信息）OSC 52 写入无用户确认

任何程序（含远程会话中的程序）都能静默覆盖系统剪贴板。这是多数终端的默认行为，且回放保护已经挡掉最危险的一类误写；若后续要收紧，可与 P1 的开关合并设计。

## 优点

- 粘贴内容做了 ESC 与结束标记清洗，直接阻断“粘贴即执行”的注入路径，并有对应测试（`6771`）。
- 历史回放不触发剪贴板写入，避免恢复会话时覆盖用户剪贴板。
- 图片自动转 PNG 并落盘，规避了 agent CLI 只认 PNG/JPEG 的兼容问题。
- 远程场景对图片与文件都做了上传，粘贴语义在本地与远端保持一致。
- 复制/粘贴/全选/入 composer 都同时提供了快捷键与右键菜单，符合“鼠标可达的操作键盘也可达”的要求。

## 测试覆盖

已有相关用例：

- `wraps_bracketed_paste_only_when_requested`（`6300`）
- `bracketed_paste_strips_injected_escape_sequences`（`6771`）
- `pasted_bmp_is_re_encoded_as_png`（`6387`）
- `extracts_terminal_paste_content_with_correct_priority`（`6468`）
- `linux_terminal_clipboard_shortcuts_preserve_ctrl_c_for_sigint`（`6124`）
- 粘贴路径引用与上传（`6428`、`6438`）

建议补充：

1. Windows 下 `Ctrl+V` 粘贴、`Ctrl+Shift+V` 仍粘贴、且 `Ctrl+V` 在未启用时不吞掉 `^V`。
2. OSC 52 读取在 `deny`/`allow` 两种配置下的行为，以及回放期间始终保持忽略。
3. 超大粘贴的分块或确认路径。

## 建议优先级

1. **P2** Windows `Ctrl+V` 粘贴——改动最小、收益最直接。
2. **P1** OSC 52 读取开关——解锁 SSH/tmux 场景，默认关闭不影响安全基线。
3. **P4** `Shift+Insert`——低成本补齐习惯。
4. **P5** 粘贴大小上限或分块。
5. **P3** OSC 5522——非必要，暂缓。

## 限制

本报告基于静态代码审读，未在运行中的 app 内实测各平台快捷键与远程粘贴流程；结论以代码路径为准，若存在平台层拦截（例如 GPUI 在 Windows 上对 `Ctrl+V` 的预处理），需要运行时验证后修正。
