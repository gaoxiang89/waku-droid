# Pilo 的 WSL / SSH / Remote WebUI 实现方案

本文总结 Pilo 如何实现两件容易混淆的事：

1. **远程开发环境**：Local / WSL / SSH 三种 Connection，让 Pi 运行在代码所在环境。
2. **Remote WebUI**：桌面端作为 Host，通过 HTTP + WebSocket 让手机或浏览器访问同一套工作区。

两者是叠加关系：WebUI 只是桌面 Host 的另一个前端，它背后访问的 Local / WSL / SSH 项目仍然走同一套 Connection 层。

## 1. 总体架构

```text
┌────────────────────┐   ┌──────────────────────────┐
│ 桌面 UI (Tauri)    │   │ 浏览器 / 手机 WebUI       │
│ src/App            │   │ src/remote (PWA)          │
└─────────┬──────────┘   └────────────┬─────────────┘
          │ Tauri commands/events     │ HTTP /api/v1 + WS /events
          │                           ▼
          │                  ┌──────────────────┐
          │                  │ Remote Host(axum)│  src-tauri/runtime/remote
          │                  └────────┬─────────┘
          ▼                           ▼
┌───────────────────────────────────────────────┐
│ Pilo Runtime (src-tauri/runtime)              │
│ ChatSessions · ServerManager · EventBus       │
└───────────────────────┬───────────────────────┘
                        │ pilo-protocol (protobuf 帧, stdio)
        ┌───────────────┼────────────────┐
        ▼               ▼                ▼
   Local 子进程   wsl.exe --exec    ssh <target> <cmd>
        └───────────────┴────────────────┘
                        ▼
              pilo-server（跑在目标环境内）
                        ▼
              pi --mode rpc（cwd = 项目目录）
```

核心思路：**不同步项目，也不在桌面端实现文件/进程能力。** 在目标环境里运行一个小型常驻代理 `pilo-server`，桌面端只通过 stdio 向它发 RPC。三种 Connection 的差别被压缩到“怎么拉起 `pilo-server` 并接上它的 stdin/stdout”这一件事上。

涉及的 crate 和目录：

| 位置 | 职责 |
| --- | --- |
| `crates/pilo-protocol` | 桌面端与 server 共用的帧协议、能力清单、`BuiltinExtension` 类型 |
| `crates/pilo-server` | 目标环境内的代理：文件、命令、会话索引、终端、预览端口、Pi 进程管理 |
| `src-tauri/src/domain/connection.rs` | `Connection` / `ConnectionKind` / `SshTarget` 数据模型 |
| `src-tauri/src/runtime/server_deploy.rs` (+ `server_deploy/target.rs`) | 探测平台、部署并拉起 `pilo-server` |
| `src-tauri/src/runtime/server_client/` | `ServerClient`（单连接 RPC）与 `ServerManager`（连接池） |
| `src-tauri/src/runtime/{wsl,ssh}.rs` | WSL 发行版枚举；SSH 参数、askpass、隧道 |
| `src-tauri/src/runtime/remote/` | Remote WebUI 的 HTTP/WS 服务、配对、鉴权 |
| `src/remote/` | WebUI 前端（PWA） |

## 2. Connection 模型

`ConnectionKind` 是带 `type` 标签的枚举（`domain/connection.rs`）：

```rust
enum ConnectionKind {
    Local,
    Wsl { distro: String },
    Ssh { target: SshTarget },        // ConfigHost { host } | Direct { hostname, port, user, identity_file, proxy_jump, .. }
}
```

附加字段：

- `pi_executable`：用户可为每个 Connection 指定 Pi 路径，缺省从目标环境的登录 shell `PATH` 中查找。
- `pi_runtime: Workspace | Local`：Pi 的运行位置，**只对 SSH 有意义**（见 §5）。WSL 保存时会校验必须为 `Workspace`。
- `SshAuthMethod`：`Agent`（默认）、`Password`、`Key`。

项目的稳定 ID 由 `connection_id + path` 生成，会话 Key 和各种 stream ID 都由它派生。

## 3. 统一的 pilo-server 通道

### 3.1 协议（`pilo-protocol`）

- 帧格式：`4 字节小端长度 + protobuf Envelope`，单帧上限 64 MiB。
- `Envelope` 有三种：`Request{id, method, params, binary}`、`Response{id, result, binary, error}`、`Event{stream_id, event, data, binary}`。
- JSON 负载放在 `json` 字段，**二进制附件独立传输**，不做 base64（文件读写、终端输出、Pi 的 RPC 行都走 binary）。
- `PROTOCOL_VERSION = 3`，`SERVER_CAPABILITIES` 列出必需方法（`fs.*`、`session.*`、`terminal.*`、`pi.start/send/stop`、`preview.ports`、`command.run`、`environment.inspect` 等）。

### 3.2 握手与版本兜底（`server_client.rs`）

`ServerClient::connect` 的流程：

1. `spawn_server` 拉起进程（见 §3.3）。
2. 启动 writer / reader / stderr 三个任务。
3. 发 `server.ping`（15 秒超时），校验 `protocol_version` 与能力集。
4. 协议不一致且复用了旧部署时，**强制重新部署一次后重试**，用来处理“两个 Pilo 版本抢同一个远端文件”的竞态。
5. 失败路径通过 `ConnectionDiagnostics` 分阶段记录：`prepare_server → platform_probe → deploy → upload → install → spawn → handshake`。

请求层细节：

- 请求 ID 自增，`pending` 表 + `oneshot` 返回；默认 75 秒超时。
- 事件按 `stream_id` 分发（`ServerEventHub`），每个 stream 有容量与字节上限；连接断开时广播 `server.disconnected`。
- 停止时先关闭 stdin，让 server 在 EOF 上清理 Pi/PTY 子进程，5 秒后才强杀，避免遗留孤儿进程。

### 3.3 连接池（`ServerManager`）

- 每个 Connection 一个 `ServerClient`，带 per-connection 锁防止并发建连。
- 连接配置的 `kind` 变了或连接已关闭就丢弃重建。
- 只读方法（`fs.read_dir`、`session.scan`、`environment.inspect` 等）在连接中断时**自动重连并重试一次**；写操作和带二进制的请求不重试。
- `test_connection` 不使用池，每次新建，保证测试的是当前配置和凭据。

## 4. 三种 Connection 如何拉起 pilo-server

`spawn_server`（`server_deploy.rs`）按 `ConnectionKind` 分派。

| | Local | WSL | SSH |
| --- | --- | --- | --- |
| 拉起方式 | 直接起子进程 | `wsl.exe --distribution <d> --exec /bin/sh -c ...` | `ssh <opts> <target> '/bin/sh -c ...'` |
| 二进制来源 | 打包资源；debug 构建用自身 `--pilo-server` | 部署到发行版 `~/.cache/pilo/server/pilo-server` | 同左，经 ssh 上传 |
| 平台探测 | 编译期 | `uname -s/-m`（必须是 Linux） | `uname -s/-m`（Linux 或 Darwin） |
| 部署方式 | 无 | 在 WSL 内用 `wslpath` 复制 Windows 侧文件 | 通过 ssh stdin 流式上传 |

### 4.1 WSL

1. **发行版枚举**：`wsl.exe --list --quiet`，5 秒超时。输出可能是 UTF-16LE（带或不带 BOM），`decode_wsl_text` 负责识别并解码，再去重、去 BOM。
2. **平台探测**：`wsl.exe ... --exec /bin/sh -c "uname -s; uname -m"`，非 Linux 直接报错。
3. **部署**：`deploy_server_wsl` 先把 Windows 资源路径规范化（去掉 `\\?\` 与 `\\?\UNC\` 前缀），然后在 WSL 内执行安装脚本：
   - 用 `wslpath -u` 把 Windows 路径转成 Linux 路径。
   - 复制到临时文件，`chmod 700`，执行 `--fingerprint` 校验。
   - 校验通过才 `mv` 到目标位置，失败则 `trap` 清理临时文件。
4. **拉起**：`wsl.exe --distribution <d> --exec /bin/sh -c 'PILO_SERVER_RUNTIME_DIR=$HOME/.cache/pilo/server exec $HOME/.cache/pilo/server/pilo-server'`，stdin/stdout 即协议通道。

WSL 的其他特点：

- 路径一律是发行版内的 Linux 路径，不做 `\\wsl$\...` 映射。
- 预览端口无需隧道，直接使用 `127.0.0.1:<port>`（依赖 WSL 的本地转发）。
- 外部编辑器集成：`open_in_zed` 调用 `zed.exe --wsl <distro> -- <path>`。
- 开发模式下 WSL/SSH 的 server 需要手动 `pnpm server:build`，用单独的 `pilo-dev` 目录，不污染正式版缓存。

### 4.2 SSH

1. **系统 OpenSSH**：直接调用本机 `ssh`，不嵌入 SSH 库。因此 `~/.ssh/config`、ssh-agent、`ProxyJump` 都自然生效。
2. **基础参数**（`ssh_base_args`）：`-T`、`BatchMode`、`ConnectTimeout=5`、`StrictHostKeyChecking=accept-new`（首次信任，已变更的 key 仍拒绝）、`RemoteCommand=none`。
3. **目标**：`ConfigHost { host }` 只传别名；`Direct` 展开 `-p/-l/-i/-J`。`Key` 方式会加 `IdentitiesOnly=yes`。
4. **注入防护**：`validate_destination` 拒绝以 `-` 开头、含空白或控制字符的主机/跳板；`validate_option_value` 校验用户名。远端脚本一律用 `shell_quote` + `/bin/sh -c`。
5. **密码认证**：
   - 密码存在系统凭据库（`keyring`），不落 SQLite。
   - 用 `SSH_ASKPASS=<Pilo 自身可执行文件>` + `SSH_ASKPASS_REQUIRE=force`，并设置 `PILO_SSH_ASKPASS_CONNECTION_ID`。ssh 回调 Pilo 时，`print_askpass_password_from_environment` 读取凭据库并打印密码。
   - 关闭 `BatchMode`，只允许 `password,keyboard-interactive`，`NumberOfPasswordPrompts=1`，stdin 置空。
   - askpass 的诊断行带固定前缀，错误分类前会先过滤掉。
6. **部署**（`deploy_server_ssh`）：一条 ssh 会话里完成“检查 → 上传 → 校验 → 安装”：
   - 脚本先运行远端已有二进制的 `--fingerprint`，与本地期望值（FNV-1a 64 位内容哈希）比较，一致就输出 `ready` 并退出。
   - 否则输出 `upload`，本地把二进制写入 ssh 的 stdin，远端 `cat > tmp`。
   - 本地核对已写入字节数等于文件大小，远端再次校验指纹，通过后原子 `mv` 安装，输出 `installed`。
   - 8 秒内没有状态行则杀掉 ssh，用其 stderr 报告真实原因。
7. **预览隧道**：`PreviewManager` 对 SSH 项目起 `ssh -N -L 127.0.0.1:<本地随机端口>:127.0.0.1:<远端端口>`，带 `ExitOnForwardFailure=yes`；Local/WSL 不需要隧道。

### 4.3 内容寻址部署

三种环境共用同一个思路：**以二进制指纹为准，不信任文件名或版本号**。

- 复用条件：远端 `pilo-server --fingerprint` 等于当前构建指纹。
- 安装条件：写入临时文件 → 校验指纹 → 原子替换。
- 发行/平台矩阵由 `ServerTarget`（Linux x64/arm64、Darwin arm64、Windows x64）与资源名 `pilo-server-<os>-<arch>` 决定，缺少对应资源时明确报错而不是用错误的二进制。
- 运行目录：正式 `~/.cache/pilo/server`，开发 `~/.cache/pilo-dev/server`；正式版会清理旧的 `server-v3`。

## 5. Pi 在哪里运行

### 5.1 默认：`pi_runtime = Workspace`

桌面端发 `pi.start`，参数包括项目路径、会话文件、provider/model/thinking、内置扩展等。`pilo-server` 在**目标环境内**执行：

- 通过登录 shell（`-ilc`）取得 `PATH`，解析 `pi` 可执行文件。
- 启动 `pi --mode rpc`，`cwd` 为项目目录，Unix 下 `process_group(0)` 便于整树清理。
- stdout 按行读取，作为 `pi.rpc_json` 事件（二进制附件）推给桌面端；stderr 作为 `pi.stderr` 事件。
- 桌面端写入 `pi.send`，server 写 Pi stdin。
- `pi.stop`：先关 stdin 触发 Pi 的 `session_shutdown`，1 秒后用 `kill -KILL -<pgid>`（Unix）或 `taskkill /T /F`（Windows）清理进程树。

Pi 的会话 JSONL 保存在目标环境，Pilo 只做索引（见 §7）。这符合“Pi JSONL 是唯一事实来源”的原则。

### 5.2 SSH 的备选：`pi_runtime = Local`

用于远端没有安装 Pi、但希望在远端项目上工作的情况：

- Pi 跑在**本机**，会话文件放在本机的“会话锚点”目录（每项目一个）。
- 内置扩展 `ssh-workspace.js`（`BuiltinExtension::SshWorkspace`）接管 Pi 的 `read/write/edit/bash/find/grep/ls` 工具，把它们改成通过 ssh 在远端执行。
- 启动时关闭 Pi 内置工具、扩展发现和上下文文件（`--no-builtin-tools`、`--no-extensions`、`--no-context-files`），避免本地环境泄漏。
- 配置通过环境变量 `PILO_SSH_WORKSPACE_CONFIG` 传入，包含 `remoteCwd`、`sshArgs`、`sshEnvironment`；server 会校验 `remoteCwd` 必须是 POSIX 绝对路径、参数不含 `\0`，并且启动前先 `env_remove` 以免继承别的会话配置。
- 传输：POSIX 使用 `ControlMaster=auto` + `ControlPersist=10m` 的 socket；Windows OpenSSH 不支持多路复用，改为保持一个长连接 ssh，通过 nonce 分帧复用。
- 路径映射 `toRemote` 把本地 cwd 下的相对路径换算为远端路径，并处理 Windows 下 `/etc/hosts` 被解析成 `C:\etc\hosts` 的情况；`~` 展开为远端 `$HOME`。
- 传输失败用一组 stderr 模式识别（`Permission denied`、`Connection closed` 等），统一报 `SSH workspace unavailable`。

`pi_workspace.rs` 的 `resolve_pi_runtime` / `resolve_session_project` 负责在两种模式之间换算项目：Local 模式下会把项目的 `connection` 替换成本机 Local，路径替换为本地锚点目录。会话监听、历史读取也都先经过这一步。

## 6. 运行时事件与多端共享

- `ChatSessions`（`runtime/chat_sessions/`）为每个 `sessionKey` 维护一个 `ServerPiSession`，含 `generation`、进程状态机（Stopped/Starting/Running/Stopping/Failed）、活跃 turn、压缩状态等。
- `ServerPiSession::spawn` 先订阅 stream 事件，再发 `pi.start`；启动失败会回滚并发出 `RuntimeError::SpawnFailed`。
- Pi 事件经 `event_sink` 转成 `RuntimeEvent`，进入全局 `RuntimeEventBus`。事件带**单调递增的 sequence**，并保留一个可回放的环形窗口。
- 桌面端通过 Tauri 事件消费；WebUI 通过 WebSocket 消费**同一条总线**。所以桌面和手机看到的是同一个正在运行的 Agent，而不是两个会话。

## 7. 会话索引与监听

- `session.scan / discover / read / search / delete` 都由目标环境内的 `pilo-server` 执行，桌面端只缓存索引（SQLite），不复制对话内容。
- `session.watch_start` 让 server 监听会话目录，推送 `session.changed`（带 `paths` / `removedPaths` / `full`）。桌面端的 `SessionWatcherManager` 据此做**定向增量索引**，失败时退化为全量刷新，并对重复失败做日志限频。
- 断线后 `ServerManager` 会重连，watcher 随之重新订阅。

## 8. Remote WebUI

### 8.1 Host 侧（`src-tauri/runtime/remote`）

- **服务**：`RemoteServerManager` 在桌面进程内用 axum 监听 `0.0.0.0:<port>`（默认端口持久化，最小 1024），优雅关闭、可随设置开关；启动时自动恢复。
- **静态资源**：同一个服务通过 `fallback(asset)` 托管 WebUI 构建产物，客户端路由使用 SPA fallback，只有带指纹的资源标记为 immutable。
- **访问地址**：`access.rs` 管理最多 32 个地址（LAN、公网反代等），要求为无凭据、无 query 的 http/https 根地址；`lan.rs` 枚举 IPv4 局域网地址（只暴露 IPv4，因为监听只绑 IPv4）。
- **配对**（`pairing.rs` / `auth.rs`）：
  1. Host 生成 32 字节随机一次性 secret，有效期 10 分钟，只存其 SHA-256；配对链接形如 `<base>/pair#pair=<secret>`（secret 放在 fragment，不进服务器日志）。
  2. 每个配置的地址可使用一次，并带 `requestId` 做幂等回执。
  3. 配对成功签发设备 token（32 字节随机，库中只存哈希，30 天有效），同时登记 `RemoteDevice`（名称、配对 IP、最近使用、撤销时间）。
  4. 修改端口会清空已配对设备；修改访问地址会使旧配对码失效并重新生成。
- **鉴权**：`require_auth` 读取 `Authorization: Bearer`，WebSocket 因为浏览器无法设置头，使用 `Sec-WebSocket-Protocol: pilo-token.<token>`，不把 token 放 URL。配对接口按 IP 做失败限流（60 秒内 5 次），设备 token 校验不限流，避免把 401 变成 429 锁死。
- **请求保护**：全局请求体 32 MiB、45 秒超时；聊天附件接口使用 10 MB 限制；失败日志只记路由模板与原因，不记 URL、query、header。

### 8.2 API

`/api/v1` 下的受保护路由大致分组：

| 分组 | 路由 |
| --- | --- |
| 启动 | `bootstrap`、`theme` |
| 会话 | `sessions`、`sessions/search`、`sessions/read-state`、`sessions/external-activity`、`sessions/title`、`sessions/delete`、`history`、`history/image` |
| 聊天 | `chat/start`、`chat/stop`、`chat/state`、`chat/rpc`、`chat/mcp-approve`、`chat/files` |
| 模型与功能 | `models`、`models/scope`、`next-prompt/*`、`cache-keep-alive/settings` |
| 文件 | `files/search`（仅文件名，用于 `@` 提及）、`files/image`（项目内图片，经 server 读取） |
| 事件 | `GET /events`（WebSocket） |

Handler 本身很薄：解析请求后调用与桌面端同一套 `chat_service`、`session_index`、`remote_fs`，后者再通过 `ServerManager` 访问 Local/WSL/SSH 的 `pilo-server`。

安全收敛点：

- `chat/rpc` 经 `validate_remote_rpc` 白名单（`prompt`、`steer`、`abort`、`set_model`、`fork`、`compact`、`extension_ui_response` 等），并限制图片数量（8 张）、单张 10 MB、总量 20 MB、只允许 PNG/JPEG/WebP/GIF。
- 聊天启动时，WebUI 只允许 `noSession` 的 `model-catalog` 探测扩展，不能注入任意内置扩展。
- 聊天附件上传的目的地由目标环境内的 server 决定，不接受 WebUI 指定路径。
- 当前 WebUI 没有终端、任意文件读写、命令执行入口。

### 8.3 事件流（`ws.rs`）

1. 连接后先发 `hello{deviceId, latestSequence}`。
2. 客户端带 `?after=<seq>` 时，Host 从总线窗口回放缺失事件；窗口已被覆盖则发 `resyncRequired`，客户端重新拉 `bootstrap`。
3. 事件按 16 ms 窗口、最多 64 条合批成 `{type:"events"}` 发送。
4. 接收端落后（`Lagged`）同样发 `resyncRequired`。
5. 每 15 秒复验 token，设备被撤销、认证代数变化或 Host 关闭时主动断开。
6. 浏览器无法发送 WS Ping，约定应用层 `{"type":"ping"}` / `{"type":"pong"}`。

### 8.4 WebUI 前端（`src/remote`）

- 入口 `src/main.tsx` 根据 `DESKTOP_RUNTIME` 选择 `App`（桌面）或 `RemoteApp`（WebUI），共享主题、i18n、聊天组件。
- 前端通过 `PiloClient` 接口抽象后端：桌面实现是 `tauri-pilo-client.ts`（Tauri 命令），WebUI 实现是 `web-pilo-client.ts`（`remote-client.ts` 里的 fetch + WebSocket）。共享聊天逻辑因此不关心自己跑在哪个端。
- 令牌存 `localStorage`（`pilo.remote.device-token.v1`），最后处理的事件序号存 `pilo.remote.sequence.v1`；遇到 401 会清除令牌并回到配对页。
- 连接恢复：浏览器上下线、页面回到前台（隐藏超过 1.5 秒）、`resyncRequired` 都会触发重连与快照刷新（`use-remote-connection.ts`、`use-remote-network-lifecycle.ts`）。
- PWA：`vite-plugin-pwa` 以 `injectManifest` 生成 Service Worker，预缓存入口、关键 chunk 与字体。
- 移动端：专用的抽屉导航、触控目标、软键盘 inset、safe-area 适配（`remote-mobile-navigation.tsx`、`use-remote-keyboard-inset.ts`）。

## 9. 关键设计取舍

| 取舍 | 做法 | 原因 |
| --- | --- | --- |
| 在目标环境放代理而不是直接 SSH 操作 | `pilo-server` 常驻，stdio RPC | 一次连接内复用，减少 ssh 往返；文件/会话/终端逻辑只写一份 |
| 复用系统 `ssh` / `wsl.exe` | 不嵌入 SSH 库 | 继承用户的 ssh config、agent、ProxyJump、已有信任 |
| 指纹而非版本号决定是否部署 | FNV-1a 内容哈希 | 不信任已有文件，避免陈旧或损坏的二进制 |
| 协议帧使用 protobuf + 独立二进制段 | `Envelope.binary` | 文件、终端、Pi 输出不需要 base64 |
| Pi 默认跑在目标环境 | `pi_runtime=Workspace` | 工具直接在代码所在处执行，无需同步 |
| SSH 可选 Local Pi | `ssh-workspace.js` 重写工具 | 远端没有 Pi 时仍可用 |
| 会话不复制 | 目标环境的 JSONL + 本地索引 | 避免双写和不一致 |
| WebUI 复用同一事件总线 | sequence + 回放 + `resyncRequired` | 多端看同一个 Agent，断线可恢复 |
| WebUI 能力收敛 | RPC 白名单、无终端 | 局域网暴露面最小化 |

## 10. 已知限制与注意点

- WSL 部署要求发行版是 Linux；SSH 目标仅支持 Linux 与 macOS（`uname` 为 `Linux` / `Darwin`）。
- 开发模式不会自动构建 WSL/SSH 的 server，需要先运行 `pnpm server:build`，否则连接会提示缺少开发运行时。
- `Local` Pi 运行时要求远端项目路径为 POSIX 绝对路径。
- Windows OpenSSH 无 ControlMaster，Local Pi 在 Windows 上走常驻 ssh 会话，行为与 POSIX 不同。
- SSH 首次连接使用 `accept-new`，主机密钥变化会被拒绝，需要用户在本机 `known_hosts` 中处理。
- Remote WebUI 默认是 HTTP；跨网络访问需要自行放在 HTTPS 反代之后，并配置公开访问地址。
- 修改 WebUI 端口会使所有已配对设备失效。

## 11. Pilo 里如何添加 WSL / SSH 连接

这一节描述“用户点添加按钮之后”发生了什么，以及每一层对应的代码。借鉴时可以按同样的分层拆。

### 11.1 数据流总览

```text
设置页 (connections-settings.tsx)
  │  invoke("wsl_connection_save" | "ssh_connection_save", …)
  ▼
Tauri 命令 (runtime/commands/connections/{wsl,ssh}.rs)   ← 只做校验 + 落库，不连接
  │  storage::upsert_connection        → SQLite `connections` 表 (kind_json)
  │  credentials::set_ssh_password     → 系统凭据库 (仅 SSH 密码认证)
  ▼
“测试连接” / “添加项目” 才会真正触发
  ServerManager.client(conn) → spawn_server → 探测平台 → 部署 → 握手
```

重点：**保存连接不会建立连接。** 连接是懒建立的，第一次发请求（测试、添加项目、打开会话）时才拉起 `pilo-server`。

### 11.2 添加 WSL

1. **列出发行版**：设置页打开“添加 WSL”对话框，仅在 Windows 上调用 `wsl_list_distributions`（内部 `wsl.exe --list --quiet`，见 §4.1）。列表里已添加的发行版显示为“已添加”。
2. **保存**：点“添加”后前端构造连接并调用 `wsl_connection_save`：

   ```ts
   // src/lib/api/projects.ts
   { id: `wsl:${distro}`, name: `WSL · ${distro}`, kind: { type: "wsl", distro } }
   ```

   - ID 由发行版名确定，所以重复添加是幂等的 upsert。
   - 后端 `ensure_wsl_connection` 校验：必须是 WSL 类型、`pi_runtime` 必须为 `workspace`、id 与 name 非空。
3. **测试**：`wsl_connection_test(distro)` 临时构造一个 `Connection`（`id = "wsl:<distro>"`），调用 `ServerManager::test_connection`。它**不走连接池**，每次新建连接、`server.ping`、再 `stop`，返回 `protocolVersion` 和 `serverVersion`。
4. **设置**：连接保存后可以改显示名、指定 `pi_executable`，并用 `connection_pi_probe`（`environment.pi_probe`）探测 Pi。

### 11.3 添加 SSH

SSH 的表单状态在 `connection-form.ts`，有两种模式：

| 模式 | `SshTarget` | 适用场景 |
| --- | --- | --- |
| `config` | `ConfigHost { host, authMethod }` | 已在 `~/.ssh/config` 里配好别名，Pilo 只传别名 |
| `direct` | `Direct { hostname, port, user, identityFile, proxyJump, authMethod }` | 在表单里填写主机、端口、用户、私钥、跳板机 |

注意：Pilo **不解析、不枚举** `~/.ssh/config`，`config` 模式需要用户自己输入 Host 别名。

保存流程（`ssh_connection_save`）：

1. 连接 ID 为 `ssh:<随机串>`（`emptySshConnectionForm`），编辑已有连接时沿用原 ID。
2. 前端校验：`key` 认证必须填私钥路径；`password` 认证首次必须填密码。
3. 后端按认证方式处理凭据：
   - `password`：新密码写入系统凭据库（`keyring`，service 为固定常量，account 为连接 ID）；未提供新密码但库里已有则沿用；两者都没有则报错。
   - 其他方式：删除可能残留的旧密码。
4. 写入 `connections` 表。`list` 时通过 `has_ssh_password` 返回“是否已存密码”，编辑时可用 `ssh_connection_password_get` 回填。
5. 删除连接时先停止该连接下所有项目的 Pi（`stop_connection_projects`），再删库记录和凭据。

测试有两个入口：

- `ssh_connection_test(id)`：测试已保存的连接。
- `ssh_connection_test_draft(connection, password)`：**保存前**直接测试表单草稿。密码认证时，把草稿 ID 改成 `<id>::draft:<序号>`，临时写入凭据库，测完立刻删除，避免污染真正的连接凭据。

### 11.4 添加项目（连接建立后的第一次真实使用）

| 连接类型 | 选目录方式 |
| --- | --- |
| Local | 原生目录选择器（`local_pick_project_directory`） |
| WSL / SSH | 通过连接的 `pilo-server` 浏览：`connection_fs_read_dir` → `fs.read_dir`，以 `project="/"` 加相对路径列目录 |

也可以用 `project_discover`：让 server 执行 `session.discover` 读取已有 Pi 会话头里的 `cwd`，列出“这个环境里 Pi 用过的目录”，最多 200 个。

`project::add` 调 `environment.inspect`，由 server 在目标环境里解析路径（支持 `~`）、`canonicalize`、检查是不是目录、探测 Pi 版本和 Git 分支，返回规范化后的 `cwd`，Pilo 用它生成项目 ID 并落库。

### 11.5 注册与 UI 接入清单

新增一种命令需要改这几处：

1. 在 `commands/connections/*.rs` 写 `#[tauri::command]`，并在 `commands/connections.rs`、`commands/mod.rs` 导出。
2. 在 `lib.rs` 的 `generate_handler!` 里注册（WSL/SSH 相关的有 `wsl_*`、`ssh_*`、`connection_*`、`local_connection_*`）。
3. 在 `src/lib/api/{connections,ssh-connections}.ts` 加 `invoke` 包装；写操作后调用 `notifyConnectionsChanged()` / `notifyProjectsChanged()` 让各处刷新。
4. 在 `connection-sections.tsx` / `connections-settings.tsx` 加 UI。
5. `listConnectionCatalog()` 并行加载 Local、WSL、SSH，用于首页环境列表。
6. WebUI 不提供添加连接，只展示 Host 上已配置、且 `shownInHome` 的连接和项目（见 §8）。

## 12. 在另一个项目中借鉴：移植指南

### 12.1 先决定借鉴到什么程度

| 方案 | 做法 | 适合 |
| --- | --- | --- |
| A. 只在远端执行命令 | 每次调用 `wsl.exe --exec` / `ssh host cmd`，用 stdout 拿结果 | 命令很少、无流式、无状态 |
| B. 常驻代理 + stdio RPC（Pilo 的做法） | 部署一个小二进制，保持一条长连接，在其上做请求/响应和事件流 | 需要文件、终端、长任务、事件推送 |
| C. B + 多端 WebUI | 在 B 之上加 HTTP/WS Host 和配对鉴权 | 需要手机/浏览器访问 |

只要需要流式输出或多个并发操作，建议直接走 B。下面按 B 讲。Pi 专属部分（`pi.*`、`ssh-workspace.js`、会话索引）都是业务层，可以整体替换成你自己的方法。

### 12.2 最小模块划分

| 模块 | 职责 | Pilo 对应 |
| --- | --- | --- |
| `Connection` 模型 | `Local / Wsl{distro} / Ssh{target}`，带稳定 ID | `domain/connection.rs` |
| 存储 | 连接表（`kind_json` 存枚举），密码走系统凭据库 | `storage/connections.rs`、`credentials.rs` |
| 命令构造 | 生成 `wsl.exe` / `ssh` 的参数与环境变量 | `wsl.rs`、`ssh.rs` |
| 部署 | 探测平台 → 指纹比对 → 上传/复制 → 原子安装 | `server_deploy.rs` |
| 协议 | 帧编解码 | `pilo-protocol` |
| 客户端 | 单连接 RPC：writer/reader、pending、超时、事件分发 | `server_client.rs`、`transport.rs` |
| 连接池 | 每连接一个 client、建连锁、重连重试 | `server_client/manager.rs` |
| 代理 | 目标环境里的服务端，处理 RPC | `crates/pilo-server` |
| 管理命令 | 列表/保存/测试/删除/浏览目录 | `commands/connections/*` |

最小可用的 workspace 结构：`protocol`（共享）、`server`（代理）、`app`（桌面/客户端），与 Pilo 一致，这样协议类型只写一份。

### 12.3 实施步骤

**步骤 1：数据模型与存储**

```rust
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConnectionKind {
    Local,
    Wsl { distro: String },
    Ssh { target: SshTarget },
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum SshTarget {
    ConfigHost { host: String, #[serde(default)] auth_method: SshAuthMethod },
    Direct { hostname: String, port: Option<u16>, user: Option<String>,
             identity_file: Option<String>, #[serde(default)] auth_method: SshAuthMethod,
             #[serde(default)] proxy_jump: Option<String> },
}
```

- 连接表只需 `id, name, kind_json, updated_at`；枚举整体序列化成 JSON，新增连接类型不用改表结构。
- 给新增字段加 `#[serde(default)]`，保证旧数据能反序列化（Pilo 有专门测试保证旧 SSH 目标默认 `Agent`）。
- ID 约定：`local`、`wsl:<distro>`、`ssh:<random>`。
- 项目 ID 用 `hash(connection_id, path)`，所有下游缓存都用它作 key。

**步骤 2：命令构造**

WSL，注意三点：用 `--exec` 绕过默认 shell 的 rc 输出；输出可能是 UTF-16LE；Windows 上要隐藏控制台窗口。

```rust
pub async fn list_wsl_distributions() -> Result<Vec<String>, String> {
    let mut cmd = Command::new("wsl.exe");
    cmd.args(["--list", "--quiet"]);
    hide_console_window(&mut cmd);                       // CREATE_NO_WINDOW (0x0800_0000)
    let out = timeout(Duration::from_secs(5), cmd.output()).await
        .map_err(|_| "timed out listing WSL distributions")?
        .map_err(|e| format!("failed to run wsl.exe: {e}"))?;
    Ok(decode_wsl_text(&out.stdout)                      // 含 NUL 或 BOM → 按 UTF-16LE 解码
        .lines().map(|l| l.trim().trim_start_matches('\u{feff}').trim())
        .filter(|l| !l.is_empty()).map(str::to_owned).collect())
}

fn wsl_exec(distro: &str, script: &str) -> Command {
    let mut cmd = Command::new("wsl.exe");
    cmd.args(["--distribution", distro, "--exec", "/bin/sh", "-c", script]);
    cmd
}
```

SSH，核心是一份“基础参数”，探测、部署、拉起、隧道都复用：

```rust
pub fn ssh_base_args(target: &SshTarget) -> Result<Vec<String>, String> {
    let password = matches!(target.auth_method(), SshAuthMethod::Password);
    let mut args = vec![
        "-T".into(),
        "-o".into(), format!("BatchMode={}", if password { "no" } else { "yes" }),
        "-o".into(), "ConnectTimeout=5".into(),
        "-o".into(), "StrictHostKeyChecking=accept-new".into(), // 首次信任，变更则拒绝
        "-o".into(), "RemoteCommand=none".into(),               // 忽略 config 里的 RemoteCommand
    ];
    if password {
        args.extend(["-o".into(), "PreferredAuthentications=password,keyboard-interactive".into(),
                     "-o".into(), "PubkeyAuthentication=no".into(),
                     "-o".into(), "NumberOfPasswordPrompts=1".into()]);
    }
    append_target_args(&mut args, target)?;   // ConfigHost: 只 push 别名；Direct: -p -l -i -J + 主机
    Ok(args)                                   // 约定：最后一个元素永远是目标主机
}
```

必须做的安全校验：主机名、跳板机不得以 `-` 开头，不得含空白或控制字符；端口不为 0；`Key` 方式必须有私钥路径并加 `IdentitiesOnly=yes`。“最后一个元素是主机”这个约定很有用：加隧道参数时把它 `pop` 出来，再插入 `-N -L ...` 后放回。

远端脚本一律包成 `/bin/sh -c '<quoted>'`，引用函数：

```rust
fn shell_quote(v: &str) -> String { format!("'{}'", v.replace('\'', "'\"'\"'")) }
fn wrap_posix_script(s: &str) -> String { format!("/bin/sh -c {}", shell_quote(s)) }
```

**步骤 3：SSH 密码认证（可选）**

不要把密码拼进命令行或环境变量。Pilo 的做法是让**应用自己当 askpass 程序**：

1. 设置 `SSH_ASKPASS=<当前可执行文件>`、`SSH_ASKPASS_REQUIRE=force`、自定义变量 `…_CONNECTION_ID=<id>`；Unix 下没有 `DISPLAY` 时补一个假值；stdin 置空。
2. `main()` 的第一行检查是否带有该环境变量，是则从凭据库读密码、`print!` 到 stdout、立刻退出，不启动 GUI。

```rust
fn main() {
    if my_lib::handle_ssh_askpass() { return; }   // 必须在任何初始化之前
    // ... 正常启动
}
```

密码用 `keyring` 存取，account 为连接 ID。草稿测试时用临时 ID，测完删除。

**步骤 4：平台探测与部署**

探测：`uname -s; uname -m`，用制表符分隔，映射到 `(os, arch)`，没有对应二进制就报“不支持”，不要猜。

部署脚本的关键点（WSL 与 SSH 通用，差别只在“二进制怎么到达远端”）：

```sh
set -e
dst="$HOME/.cache/<app>/server/<name>"
expected="<本地二进制指纹>"
actual=$("$dst" --fingerprint 2>/dev/null || true)
if [ "$force" = "false" ] && [ "$actual" = "$expected" ]; then echo ready; exit 0; fi
mkdir -p "$(dirname "$dst")"
tmp="$dst.tmp.$$"; trap 'rm -f "$tmp"' EXIT
# WSL: cp "$(wslpath -u "$1")" "$tmp"           ($1 为 Windows 路径)
# SSH: echo upload; cat > "$tmp"                 (本地把文件写进 ssh stdin)
chmod 700 "$tmp"
[ "$("$tmp" --fingerprint)" = "$expected" ] || { echo fingerprint_mismatch >&2; exit 43; }
mv -f "$tmp" "$dst"; trap - EXIT; echo installed
```

- 服务端二进制需要提供 `--fingerprint`（对自身文件做哈希并输出）。Pilo 用 64 位 FNV-1a，你也可以用 SHA-256。
- SSH 上传要核对写入字节数等于文件大小，再让远端校验指纹。
- 给“等待状态行”设超时（Pilo 为 8 秒），超时就杀掉 ssh 并带上 stderr 报错。
- Windows 路径传给 `wslpath` 前要去掉 `\\?\` 与 `\\?\UNC\` 前缀。
- 开发/正式用不同的远端目录，避免互相覆盖。

**步骤 5：拉起代理并接管 stdio**

```rust
// WSL
wsl_exec(distro, r#"APP_RUNTIME_DIR="$HOME/.cache/app/server" exec "$HOME/.cache/app/server/app-server""#)
// SSH
let mut cmd = ssh_command(&conn)?;          // = Command::new("ssh").args(ssh_base_args) + askpass 环境
cmd.arg(wrap_posix_script(r#"APP_RUNTIME_DIR=… exec "$HOME/.cache/app/server/app-server""#));

cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
```

- 用 `exec` 替换 shell，这样 stdin EOF 直接到达代理，信号也不会被中间 shell 吞掉。
- 用非登录的 `/bin/sh -c`，避免 `.bashrc`、`motd` 之类输出污染 stdout 协议流。
- **代理的 stdout 只能写协议帧**，日志一律写 stderr，由客户端单独读取并记录。
- 代理在 stdin EOF 时必须清理自己启动的所有子进程，然后退出（这是 stop 的前提）。

**步骤 6：协议与客户端**

最小协议：`4 字节小端长度 + 负载`。可以直接用 JSON，等有大二进制传输需求再换 protobuf。Pilo 选 protobuf 的原因是 `bytes` 字段能承载文件和终端数据而不用 base64。

```text
Request  { id, method, params, binary[] }
Response { id, result | error{code,message}, binary[] }
Event    { stream_id, event, data, binary[] }
```

客户端要有的部件：

1. `writer_loop`：从 `mpsc` 取请求写 stdin；编码失败只让该请求失败，I/O 失败则整条连接标记断开。
2. `reader_loop`：循环读帧；`Response` 按 `id` 找 `oneshot` 回复；`Event` 按 `stream_id` 分发；读到 EOF 就 `mark_disconnected`。
3. `pending: Map<id, oneshot::Sender>`，加一个 `Drop` 守卫，调用方被取消时也要移除。
4. 事件队列要**有界**（条数和字节数），溢出时发一个 `disconnected` 事件并丢弃该 stream，不要无限堆积。
5. 握手：`ping` 校验 `protocol_version` 与能力清单；版本不符且是复用的旧部署时，强制重装重试一次。
6. `stop`：关 stdin → 最多等 5 秒 → 再 `kill`。

**步骤 7：连接池**

```rust
pub async fn client(&self, conn: &Connection) -> Result<Arc<ServerClient>, String> {
    if let Some(c) = self.cached(conn).await { return Ok(c); }       // kind 相同且未关闭
    let lock = self.lock_for(&conn.id).await;                         // 每连接一把锁
    let _g = lock.lock().await;
    if let Some(c) = self.cached(conn).await { return Ok(c); }       // 拿锁后再查一次
    let c = ServerClient::connect(conn).await?;
    self.insert(conn, c.clone()).await;
    Ok(c)
}
```

- 只读方法白名单 + 连接已关闭 → 失效重连重试一次。写操作不要自动重试。
- `test_connection` 绕过池，每次新建。

**步骤 8：管理命令与 UI**

- WSL：`list_distributions`、`save`（幂等 upsert）、`test`、`remove`。
- SSH：`save`（含凭据）、`test`、`test_draft`、`remove`、`password_get`（可选）。
- 浏览目录：用代理的 `fs.read_dir`，让用户在远端目录树里选项目，而不是手输路径。
- 删除连接前先停止其下所有运行中的任务。
- WSL 入口只在 Windows 上显示。

**步骤 9：构建与分发代理二进制**

- 目标矩阵：Linux x64/arm64（`crt-static`，避免远端 glibc 版本问题）、macOS arm64、Windows x64。
- 打包时把各平台产物放进应用资源目录，命名 `<name>-<os>-<arch>`；运行时按探测结果选。
- 开发模式：Local 用应用自身带 `--server` 参数运行（避免资源陈旧）；WSL/SSH 要求先手动构建对应目标。
- 提供 `PATH_OVERRIDE` 之类的环境变量，方便调试时指向自定义二进制。

**步骤 10：诊断**

- 给每次连接分配 `attempt_id`，分阶段记录：`probe → deploy → upload → install → spawn → handshake`，每阶段记耗时和结果。
- 错误归类只输出固定标签（如 `authentication_rejected`、`host_key_verification_failed`、`connection_timed_out`），不要把远端 stderr 或凭据库错误原文写进日志。
- askpass 进程的诊断行加固定前缀，写到 stderr，客户端分类错误前先过滤掉。

### 12.4 要替换 / 可以删掉的 Pilo 专属部分

- 整个 `pi.*` 方法、`PiProcess`、`ServerPiSession`、`ChatSessions`。
- `ssh-workspace.js`、`BuiltinExtension`、`pi_runtime`、会话锚点目录。
- 会话索引与监听（`session.*`、`SessionWatcherManager`）。
- 预览隧道可按需保留：SSH 用 `-N -L` 本地转发，Local/WSL 直连。
- Remote WebUI（§8）整体是可选的。

你的项目需要的最小服务端方法集合通常是：`server.ping`、`environment.inspect`、`fs.read_dir`，再加你自己的业务方法。

### 12.5 易踩坑清单

1. **WSL 输出编码**：`wsl.exe --list` 是 UTF-16LE，直接 `from_utf8` 会得到带 NUL 的乱码。
2. **Windows 控制台闪窗**：所有子进程都要设置 `CREATE_NO_WINDOW`。
3. **stdout 被污染**：登录 shell、`motd`、rc 文件输出都会破坏帧格式。用 `--exec /bin/sh -c` 和 `ssh -T`，并且代理自己不要往 stdout 打日志。
4. **BatchMode 与主机密钥**：`BatchMode=yes` 会把“是否信任主机”提示变成直接失败，需要 `StrictHostKeyChecking=accept-new`。
5. **参数注入**：用户输入的主机名以 `-` 开头会被 ssh 当成选项。
6. **引号**：远端脚本里的路径和变量必须经过 `shell_quote`，不要用 `format!("... {path} ...")` 直接拼。
7. **孤儿进程**：停止时先关 stdin，让代理清理子进程；直接 `kill` ssh/代理会留下孤儿。设置 `kill_on_drop(true)` 作兜底。
8. **部署竞态**：两个客户端同时安装同一路径，必须“写临时文件 + 校验 + 原子 `mv`”。
9. **并发建连**：没有 per-connection 锁时，页面刷新瞬间会同时发起多次部署。
10. **背压**：事件队列无界会在慢消费者或断网时吃光内存。
11. **Windows OpenSSH 无 ControlMaster**：需要多次 ssh 调用时，在 Windows 上改成保持一个长连接。
12. **路径形态**：Windows 的 `\\?\` 前缀会让 `wslpath` 和很多 Node/Bun 加载器失败；WSL 项目使用发行版内的 Linux 路径，不做 `\\wsl$` 映射。
13. **保存连接不要联网**：保存只落库，连接放到测试或首次使用时再做，否则离线时无法编辑配置。
14. **草稿测试的凭据**：用临时 ID 写入、用完即删，避免覆盖已有连接的密码。

### 12.6 验证清单

- 单元测试：WSL 输出解码（UTF-16LE、UTF-8、去重）、`ssh_base_args`（ConfigHost、Direct、密码、拒绝非法主机）、`shell_quote`、旧数据反序列化、帧编解码往返、`uname` 探测解析。
- 集成测试：用本机 `sshd` 或容器验证部署的 `ready / installed / fingerprint_mismatch` 三条路径；断开 stdin 后代理及其子进程确实退出。
- 手动：WSL 多发行版、`~/.ssh/config` 别名、跳板机、密码认证、私钥认证、首次主机密钥、断网重连、代理版本升级后的自动重装。

## 13. 代码索引

- Connection 模型：`src-tauri/src/domain/connection.rs`
- 拉起与部署：`src-tauri/src/runtime/server_deploy.rs`、`server_deploy/target.rs`
- RPC 客户端与连接池：`src-tauri/src/runtime/server_client.rs`、`server_client/{manager,transport}.rs`
- WSL：`src-tauri/src/runtime/wsl.rs`、`runtime/commands/connections/wsl.rs`
- SSH：`src-tauri/src/runtime/ssh.rs`、`runtime/commands/connections/ssh.rs`、`runtime/credentials.rs`
- Pi 运行时切换：`src-tauri/src/runtime/pi_workspace.rs`
- Pi 会话封装：`src-tauri/src/runtime/server_pi.rs`、`runtime/chat_sessions/`
- 预览：`src-tauri/src/runtime/preview.rs`
- 服务端：`crates/pilo-server/src/{lib,environment,extensions}.rs`、`runtime_streams/pi.rs`、`extensions/ssh-workspace.js`
- 协议：`crates/pilo-protocol/src/{lib,pi_extensions}.rs`
- Remote Host：`src-tauri/src/runtime/remote/{manager,access,lan,pairing,auth}.rs`、`remote/api/{mod,handlers,middleware,validation,ws}.rs`
- WebUI：`src/remote/*`、`src/lib/api/{pilo-client,tauri-pilo-client}.ts`、`src/main.tsx`
