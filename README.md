# ComfyFleet

**ComfyFleet** 是面向 Windows 的 ComfyUI 多实例管理器：多实例启停、uv 虚拟环境、实例环境终端、环境诊断、版本与节点治理、内置控制台，以及局域网集群同步与快照恢复。

数据目录位于**安装目录下的 `data`**，便携式布局：卸载或搬走安装目录时数据跟着走。旧版数据目录会在正式安装首次启动时自动迁移，升级不会丢实例。

**技术栈**：Tauri 2 + Rust + React 19 + TypeScript + `portable-pty`（ConPTY）+ xterm.js

---

## 能力

| 能力 | 说明 |
|---|---|
| 实例登记 / 移除 | 生成 `instance.json` 与归属标记，原子写 |
| 控制台（真实 PTY） | 环境终端与 ComfyUI 日志；中文、ANSI、`\r` 进度条正常 |
| 启停 | 启动 / 停止（杀进程树）、端口冲突直接报错、就绪判定（TCP + `/system_stats` 200）、显存 / 队列轮询、孤儿进程清理、托盘启停全部 |
| 环境安装 | uv + torch + 引擎矩阵，按 CUDA 轨道选择 torch 版本，自动钉死 torchvision / torchaudio / torchcodec |
| 版本管理 | 稳定版 / 开发版切换、停在具体提交、分支更新、脏工作区与运行中拦截 |
| 节点管理 | `custom_nodes` 启停、git 安装与更新、删除；按节点补依赖，torch 系只提示不升降；更新 / 补依赖入口是名称旁的可点击徽标，安装表单只填仓库地址、目录名按仓库名自动生成；游离 HEAD 按远端默认分支判断与快进 |
| 模型资源管理器 | 概览卡片可切换实例 / 共享模型根，浏览子目录、文件大小与修改时间；从系统拖入文件或文件夹直接复制到当前目录，同名跳过 |
| 环境诊断 | 实测 Python / torch / CUDA / `pip check`，给出证据与建议 |
| 导入已有安装 | 只登记不复制；可指定 Python 环境并先检测 |
| 云端实例 | 只登记地址，直接打开浏览器，不显示本地启停 |
| 快照 | 版本、启动参数、节点、包清单；支持时间线与恢复，恢复前自动拍回滚点 |
| 操作日志 | 每个实例一份时间线，记录启停、装依赖、切版本、快照与同步等操作 |
| 局域网集群同步 | 源端创建带密码的集群，目标端选择本机实例接收；模型、包、本体、节点分阶段应用；URL wheel 不预下载，目标端直接按 URL 安装；wheel 文件名/METADATA 版本不一致时先由 uv 安装，被拒绝后自动用目标 venv 的 pip 重试，仍失败只提示不中断 |
| 加速库 | 按环境筛选可安装的 FlashAttention / SageAttention / Nunchaku / xformers / Triton wheel |
| 应用内更新 | 设置 → 更新：从 ModelScope 检查新版本，用户点确认后才下载安装，签名校验通过才允许安装；有实例在跑时直接拒绝 |

### 进度与控制台行为

- 安装/节点/切版本显示网络速度，同步显示局域网速度；不展示磁盘写入速度。
- 控制台按子进程原始输出显示，不再二次排版成进度方块；pip/tqdm/git 的 `\r` 进度和自定义轮子下载每秒最多更新一次，并原地刷新同一行，不再一次下载灌出几千行 `1% / 2% / 3%…`。
- 自定义轮子只在安装期间临时下载，装完立即删除；快照和同步记录原始下载 URL，后续恢复/重装按 URL 重新下载，不再长期占用 `shared/accel/downloads`。
- 安装步骤不设总时长上限：慢网络可以继续下载，是否终止由用户在安装弹窗里点“取消安装”决定；网络完全无响应时由 uv/pip 的 HTTP 超时报错。
- 控制台保持逐行输出，不会出现持续刷新的 tqdm 进度条。
- 实例页的控制台是底部悬浮层：任务运行时那枚 56×3 的把手保持原尺寸，只让线自身流动；展开后底部显示任务、阶段、步数、速度、排队、百分比和进度轨。空闲时是一枚静态细把手。鼠标进入把手才半屏浮起，点击整窗展开；只盖内容区，不盖顶栏、页签和底栏。终端页签不显示这层。
- 悬浮控制台头部的眼睛按钮控制「悬浮自动展开」：默认开启，鼠标进入把手即半屏浮起；关掉后悬停不再弹，点击长条直接整窗展开。关闭自动展开时长按长条可沿底边拖动，松手吸附左右边缘；开关与位置都会记住。
- ComfyUI 日志中的裸 `\r` 由后端拆分并节流，前端只写完整行。

---

## 界面

顶栏 40px：返回在左、标题居中、全局设置与窗口按钮在右。底栏 48px 常驻，只放操作与保存。实例页页签：概览 / 终端 / 版本 / 启动参数 / 存储 / 快照 / 日志 / 设置。概览 = 运行指标 + 并排的节点卡片与模型资源管理器（中间拖动柄可调宽，双击复位）；诊断是「节点」卡片里的弹窗，不占独立页签。全局设置左侧分类：外观 / 网络与镜像 / 存储 / 运行 / 同步与快照 / 操作日志 / 更新。

---

## 数据目录

数据目录 = **安装目录下的 `data`**：`config.json`、`instances\`、`shared\`、`git\`、`logs\`。

- **升级迁移**：正式安装首次启动时若发现旧版数据目录，会整体搬到 `data`；同盘 rename，跨盘复制后改名，新目录已初始化时不动旧数据。
- **卸载不删数据**：NSIS 卸载只删程序文件，`data` 保留。
- **不可写回退**：安装目录不可写时回退到 `%APPDATA%` 下的数据目录，并在日志中说明。
- **强制指定**：`COMFYFLEET_DATA_DIR=<绝对路径>` 可把数据目录钉到别处。
- **构建目录不迁移**：`pnpm tauri dev` 与 `cargo build` 的产物在 `src-tauri\target\` 下，不自动迁移真实数据。

---

## 应用内更新

设置 → **更新**：只检查、不自动装。用户点「立即更新并重启」后才会下载安装包，装完自动重启。

- **更新源**：ModelScope 模型仓库（默认 `yuanxinjie/AI-windows-whl` 的 `ComfyFleet/` 目录），
  地址在 `src-tauri/tauri.conf.json` 的 `plugins.updater.endpoints`。国内直连比 GitHub Releases 快；
  大文件会 302 到带签名的 CDN，updater 跟随重定向即可。
- **签名**：`tauri.conf.json` 里的 `plugins.updater.pubkey` 是公钥，私钥只在发布机上。
  公私钥对在 `%USERPROFILE%\.tauri\comfyfleet.key` + `.key.password`，**必须备份**：
  丢了就只能让用户重新手动装一次（详见下方“密钥丢了怎么办”）。
- **停实例是硬约束**：更新会重启本程序，正在跑的 ComfyUI 会被一起终止，
  因此 `app_update_install` 在有实例运行时直接返回 `INSTANCES_RUNNING`；
  安装环境 / 切版本 / 同步准备期间返回 `APP_BUSY`。

### 发版流程

```powershell
.\scripts\release.ps1 -Version 0.1.1 -Notes "本次改了什么"
```

发版说明有多行、或命令行里中文被 GBK 控制台转义时，把说明写进
`dist-update\notes.txt`（UTF-8），脚本会优先用它、忽略 `-Notes`。

脚本会：同步三处版本号 → 用 `~/.tauri/comfyfleet.key` 签名构建 → 生成
`dist-update\latest.json` 与 `ComfyFleet_0.1.1_x64-setup.exe` → 打印上传清单。
然后把这些文件传到 ModelScope 仓库的 `ComfyFleet/` 目录（网页上传，或 `-Upload` 走 SDK）。

`latest.json` 就是「有没有更新」的唯一依据：

```json
{
  "version": "0.1.1",
  "notes": "本次改了什么",
  "pub_date": "2026-11-11T08:00:00Z",
  "platforms": {
    "windows-x86_64": { "signature": "…", "url": "https://…/ComfyFleet_0.1.1_x64-setup.exe" }
  }
}
```

> **第一次必须手动装**：已经装着的 0.1.0 里没有 updater 代码，它不会自己检查更新。
> 要让在线更新生效，需要手动装一次 0.1.1（或更高），之后才走自动更新。

### 密钥丢了怎么办

私钥丢失后无法再签发合法更新包（签名校验会失败）。补救只有一条路：
生成新密钥对 → 把新公钥写进 `tauri.conf.json` → 发一个**手动安装**的新版本，
用户装过这个版本之后，才能继续接收后续在线更新。

---

## 开发

前置：Node 20+ / pnpm、Rust stable（MSVC）、VS Build Tools、WebView2

```powershell
pnpm install
pwsh -File scripts/fetch-mingit.ps1   # 打包资源：MinGit（38 MB，校验 SHA256）
pwsh -File scripts/fetch-uv.ps1       # 打包资源：uv.exe（54 MB，校验 SHA256）
pnpm tauri dev          # 开发
pnpm typecheck          # 前端类型检查
pnpm tauri build        # 出 NSIS 安装包
```

两个 `fetch-*.ps1` 下载的二进制不进 git（`.gitignore` 已排除），只跑 `pnpm tauri dev`
不需要它们；`pnpm tauri build` 缺文件会直接失败。

产物：`src-tauri/target/release/bundle/nsis/`

### 目录

```
src/                  React 前端
src/lib/api.ts        Tauri 命令封装（invoke / Channel）
src-tauri/src/
  commands/           app / settings / instance / pty / run / env / nodes / models / version ...
  config.rs           设置与实例模型、原子读写
  runtime.rs          进程、日志、就绪探测
  state.rs            共享状态
docs/                 技术评估、开发计划、快照设计
```

---

## 许可

GPL-3.0-only。随包分发的第三方组件（uv、MinGit、Python）许可见安装包内的第三方许可说明。
