# 编译脚本

一键编译入口：

```powershell
.\scripts\build.ps1
```

双击 `scripts\build.cmd` 也可以，效果是“保持当前版本编译一次”，不会自动部署。

## 常用命令

```powershell
# 保持当前版本，只编译安装包
.\scripts\build.ps1

# 版本号补丁位 +1，例如 0.1.5 -> 0.1.6
.\scripts\build.ps1 -BumpPatch

# 编译并替换本地运行版本（D:\project\ComfyFleet）
.\scripts\build.ps1 -BumpPatch -Deploy

# 指定版本和发版说明
.\scripts\build.ps1 -Version 0.1.6 -Notes "修复 ..." -Deploy

# 编译并上传到 ModelScope
$env:MODELSCOPE_API_TOKEN = '你的 token'
.\scripts\build.ps1 -BumpPatch -Upload
```

## 需要准备什么

### 1. 基础环境

- Node.js 和 pnpm 已安装，项目依赖已执行过 `pnpm install`
- Rust 和 cargo 已安装（默认在 `%USERPROFILE%\.cargo\bin`）
- Windows PowerShell 5.1 或 PowerShell 7

### 2. 更新签名密钥

脚本默认读取这两个文件，不需要把私钥写进仓库：

```text
%USERPROFILE%\.tauri\comfyfleet.key
%USERPROFILE%\.tauri\comfyfleet.key.password
```

如果换机器，先生成新密钥：

```powershell
pnpm tauri signer generate -w "$env:USERPROFILE\.tauri\comfyfleet.key"
```

生成后把公钥填到 `src-tauri\tauri.conf.json` 的 `plugins.updater.pubkey`，私钥和密码文件放到上面两个路径。

### 3. 版本号

- 不改版本号：直接运行脚本，适合本地测试
- 发布新版本：加 `-BumpPatch`，或加 `-Version x.y.z`
- 版本号会同步写入 `tauri.conf.json`、`Cargo.toml`、`package.json`，不能只改一处

### 4. 发版说明

默认读取 `dist-update\notes.txt`。也可以命令行传：

```powershell
.\scripts\build.ps1 -Version 0.1.6 -Notes "第一条`n第二条"
```

### 5. 本地部署目录

`-Deploy` 默认替换：

```text
D:\project\ComfyFleet\comfyfleet.exe
```

目录不同就加 `-DeployDir "你的目录"`。

### 6. ModelScope 上传（可选）

只有加 `-Upload` 才需要设置环境变量：

```powershell
$env:MODELSCOPE_API_TOKEN = '...'
```

不要把 token 写进脚本或提交到仓库。

## 产物

编译完成后在 `dist-update` 下：

```text
ComfyFleet_<版本>_x64-setup.exe
latest.json
```

`latest.json` 里的下载地址默认指向：

```text
https://www.modelscope.cn/models/yuanxinjie/AI-windows-whl/resolve/master/ComfyFleet/
```

发布时需要把安装包和 `latest.json` 一起上传到 ModelScope 的 `ComfyFleet/` 目录。

## 注意

- `build.ps1` 使用 UTF-8 with BOM 保存，Windows PowerShell 5.1 才能正确读取中文。用编辑器修改后请保持该编码。
- 安装包有 Tauri updater 签名，但没有 Windows Authenticode 签名，首次运行可能触发 SmartScreen。
