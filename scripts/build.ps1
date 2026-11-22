<#
.SYNOPSIS
  一键编译 ComfyFleet：同步版本、构建签名安装包、可选部署到本地。

.DESCRIPTION
  这个脚本是 scripts/release.ps1 的外层入口，负责三件事：
  1. 确认版本号（可自动递增 patch），并调用 release.ps1 构建安装包；
  2. 校验安装包与 latest.json，打印路径和 SHA-256；
  3. 加 -Deploy 时把 release 版 exe 复制到本地运行目录，备份旧版并重启应用。

  签名私钥默认只从 %USERPROFILE%\.tauri 读取，不需要写进这个脚本：
    %USERPROFILE%\.tauri\comfyfleet.key
    %USERPROFILE%\.tauri\comfyfleet.key.password

.PARAMETER Version
  目标版本号，例如 0.1.5。必须和当前版本不同才会形成新版本更新。

.PARAMETER BumpPatch
  在当前版本上自动加一个 patch，例如 0.1.4 -> 0.1.5。

.PARAMETER Notes
  发版说明，写入 dist-update\notes.txt；不传则沿用已有 notes.txt。

.PARAMETER Deploy
  构建成功后部署到本地目录并启动应用。

.PARAMETER DeployDir
  本地运行目录，默认 D:\project\ComfyFleet。

.PARAMETER Upload
  构建成功后调用 release.ps1 的 ModelScope 上传；需要 MODELSCOPE_API_TOKEN。

.EXAMPLE
  # 只构建当前版本
  .\scripts\build.ps1

.EXAMPLE
  # 自动升到 0.1.5，构建并替换本地运行版本
  .\scripts\build.ps1 -BumpPatch -Deploy

.EXAMPLE
  # 明确指定版本，只构建签名安装包
  .\scripts\build.ps1 -Version 0.1.5 -Notes "修复模型目录同名时无法区分"
#>
param(
  [string]$Version,
  [switch]$BumpPatch,
  [string]$Notes,
  [switch]$Deploy,
  [string]$DeployDir = 'D:\project\ComfyFleet',
  [switch]$Upload
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$tauriConf = Join-Path $root 'src-tauri\tauri.conf.json'
$releaseScript = Join-Path $PSScriptRoot 'release.ps1'
$notesPath = Join-Path $root 'dist-update\notes.txt'
$releaseExe = Join-Path $root 'src-tauri\target\release\comfyfleet.exe'

function Read-Text([string]$path) {
  return [System.IO.File]::ReadAllText($path)
}

function Write-TextNoBom([string]$path, [string]$text) {
  [System.IO.File]::WriteAllText($path, $text, (New-Object System.Text.UTF8Encoding($false)))
}

if (-not (Test-Path $releaseScript)) {
  throw "找不到构建脚本：$releaseScript"
}

# release.ps1 使用 cargo；非交互运行时 PATH 里经常没有 cargo。
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
  $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
  if (Test-Path (Join-Path $cargoBin 'cargo.exe')) {
    $env:Path = "$cargoBin;$env:Path"
  } else {
    throw "找不到 cargo：请安装 Rust，并把 %USERPROFILE%\.cargo\bin 放进 PATH"
  }
}

$currentVersion = (ConvertFrom-Json (Read-Text $tauriConf)).version
if ($Version -and $BumpPatch) {
  throw "Version 和 BumpPatch 只能选一个"
}

if ($BumpPatch) {
  $parts = $currentVersion.Split('.')
  if ($parts.Count -ne 3) { throw "当前版本不是 x.y.z：$currentVersion" }
  $Version = "$($parts[0]).$($parts[1]).$([int]$parts[2] + 1)"
} elseif (-not $Version) {
  $Version = $currentVersion
}

if ($Version -notmatch '^\d+\.\d+\.\d+$') {
  throw "版本号要写成 x.y.z（收到：$Version）"
}

if ($Notes) {
  $notesDir = Split-Path -Parent $notesPath
  if (-not (Test-Path $notesDir)) {
    New-Item -ItemType Directory -Force -Path $notesDir | Out-Null
  }
  Write-TextNoBom $notesPath $Notes
}

Write-Host "目标版本：$Version（当前：$currentVersion）" -ForegroundColor Cyan

# release.ps1 负责版本号写入、签名、构建、latest.json 和可选的 ModelScope 上传。
$releaseArgs = @{ Version = $Version }
if ($Upload) { $releaseArgs.Upload = $true }
& $releaseScript @releaseArgs

$installer = Join-Path $root ("dist-update\ComfyFleet_$Version" + '_x64-setup.exe')
if (-not (Test-Path $installer)) {
  throw "构建结束但没找到安装包：$installer"
}

$hash = (Get-FileHash $installer -Algorithm SHA256).Hash
Write-Host ""
Write-Host "安装包：$installer" -ForegroundColor Green
Write-Host "SHA-256：$hash"
Write-Host "更新清单：$(Join-Path $root 'dist-update\latest.json')"

if ($Deploy) {
  if (-not (Test-Path $DeployDir)) {
    New-Item -ItemType Directory -Force -Path $DeployDir | Out-Null
  }
  $dest = Join-Path $DeployDir 'comfyfleet.exe'
  if (-not (Test-Path $releaseExe)) {
    throw "找不到 release exe：$releaseExe"
  }

  # Windows 不允许覆盖正在运行的 exe，先结束旧进程。
  $running = Get-Process -Name comfyfleet -ErrorAction SilentlyContinue
  if ($running) {
    Write-Host "停止正在运行的 ComfyFleet..." -ForegroundColor Yellow
    $running | Stop-Process -Force
    foreach ($attempt in 1..20) {
      if (-not (Get-Process -Name comfyfleet -ErrorAction SilentlyContinue)) { break }
      Start-Sleep -Milliseconds 250
    }
  }

  $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
  $backup = $null
  if (Test-Path $dest) {
    $backup = "$dest.bak-$stamp"
    Copy-Item -LiteralPath $dest -Destination $backup -Force
  }
  Copy-Item -LiteralPath $releaseExe -Destination $dest -Force
  $exeHash = (Get-FileHash $dest -Algorithm SHA256).Hash
  $proc = Start-Process -FilePath $dest -WorkingDirectory $DeployDir -PassThru
  Start-Sleep -Seconds 3
  $proc.Refresh()

  Write-Host ""
  Write-Host "已部署：$dest" -ForegroundColor Green
  Write-Host "本地 SHA-256：$exeHash"
  Write-Host "进程：PID $($proc.Id)，Responding=$($proc.Responding)"
  if ($backup) { Write-Host "旧版备份：$backup" }
}
