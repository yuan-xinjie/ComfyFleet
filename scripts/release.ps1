<#
.SYNOPSIS
  构建 ComfyFleet 的签名更新包，并生成可直接上传到 ModelScope 的 latest.json。

.DESCRIPTION
  在线更新三步走，这个脚本负责第 1、2 步，第 3 步（上传）给出清单或直接调用 SDK：

  1. 同步版本号：tauri.conf.json / Cargo.toml / package.json 三处必须一致，
     不一致时 updater 会认为「没有新版本」，用户永远看不到更新。
  2. 签名构建：用 ~/.tauri/comfyfleet.key 签出安装器 ComfyFleet_<版本>_x64-setup.exe
     与对应的 .exe.sig，并组装 latest.json。
  3. 上传到 ModelScope 仓库的 ComfyFleet/ 目录：
       latest.json
       ComfyFleet_<版本>_x64-setup.exe

.PARAMETER Version
  新版本号（如 0.1.1）。给了就顺手改掉三处版本文件；不给则用当前 tauri.conf.json 的版本。

.PARAMETER Notes
  发版说明，写进 latest.json 的 notes，会显示在「设置 → 更新」面板里。

.PARAMETER Repo
  ModelScope 仓库，默认 yuanxinjie/AI-windows-whl（和轮子仓库同一个，放 app/ 子目录）。

.PARAMETER Upload
  带上这个开关就调用 ModelScope Python SDK 直接上传；需要先 pip install modelscope，
  并设置环境变量 MODELSCOPE_API_TOKEN（不要把 token 写进脚本或仓库）。

.EXAMPLE
  .\scripts\release.ps1 -Version 0.1.1 -Notes "修复更新面板在某些主题下的对比度"
#>
param(
  [string]$Version,
  [string]$Notes = "",
  [string]$Repo = "yuanxinjie/AI-windows-whl",
  [string]$RepoPath = "ComfyFleet",
  [switch]$SkipBuild,
  [switch]$Upload
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$tauriConf = Join-Path $root 'src-tauri\tauri.conf.json'
$cargoToml = Join-Path $root 'src-tauri\Cargo.toml'
$pkgJson = Join-Path $root 'package.json'

function Write-TextNoBom([string]$path, [string]$text) {
  # 这几个文件都是无 BOM 的 UTF-8；PS5 的 Set-Content -Encoding UTF8 会加 BOM，
  # 会污染 JSON 首字符，这里统一走 .NET 写入。
  [System.IO.File]::WriteAllText($path, $text, (New-Object System.Text.UTF8Encoding($false)))
}

function Read-Text([string]$path) {
  return [System.IO.File]::ReadAllText($path)
}

# ---------------------------------------------------------------- 1. 版本号
if ($Version) {
  if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "版本号要写成 x.y.z（收到：$Version）"
  }
  $conf = Read-Text $tauriConf
  $conf = [regex]::Replace($conf, '("productName":\s*"ComfyFleet",\s*\r?\n\s*"version":\s*")[^"]+(")', "`${1}$Version`${2}")
  Write-TextNoBom $tauriConf $conf

  $cargo = Read-Text $cargoToml
  $cargo = [regex]::Replace($cargo, '(?m)^(version\s*=\s*")[^"]+(")', "`${1}$Version`${2}", 1)
  Write-TextNoBom $cargoToml $cargo

  $pkg = Read-Text $pkgJson
  $pkg = [regex]::Replace($pkg, '("version":\s*")[^"]+(")', "`${1}$Version`${2}", 1)
  Write-TextNoBom $pkgJson $pkg
}

$conf = Get-Content $tauriConf -Raw | ConvertFrom-Json
$ver = $conf.version
$confVer = $ver
$cargoVer = ([regex]::Match((Read-Text $cargoToml), '(?m)^version\s*=\s*"([^"]+)"')).Groups[1].Value
$pkgVer = (Get-Content $pkgJson -Raw | ConvertFrom-Json).version
if (($confVer -ne $cargoVer) -or ($confVer -ne $pkgVer)) {
  throw "版本号不一致：tauri.conf=$confVer, Cargo.toml=$cargoVer, package.json=$pkgVer。加 -Version 让脚本统一改。"
}
Write-Host "版本：$ver" -ForegroundColor Cyan

# 发版说明优先从 dist-update\notes.txt 读：命令行里的中文会被 GBK 控制台转义，
# 多行说明也不方便塞进参数。文件存在且非空时覆盖 -Notes。
$notesFile = Join-Path $root 'dist-update\notes.txt'
if (Test-Path $notesFile) {
  $fromFile = (Read-Text $notesFile).Trim()
  if ($fromFile) { $Notes = $fromFile }
}

# ---------------------------------------------------------------- 2. 签名密钥
$keyDir = Join-Path $env:USERPROFILE '.tauri'
$key = Join-Path $keyDir 'comfyfleet.key'
$pwFile = Join-Path $keyDir 'comfyfleet.key.password'
foreach ($f in @($key, $pwFile)) {
  if (-not (Test-Path $f)) {
    throw "缺少签名文件：$f`n先跑一次：pnpm tauri signer generate -w `"$key`""
  }
}
$env:TAURI_SIGNING_PRIVATE_KEY = $key
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = (Read-Text $pwFile).Trim()

# ---------------------------------------------------------------- 3. 构建
if (-not $SkipBuild) {
  # 直接调 tauri.js，不走 pnpm 的 .ps1 垫片：PS5 在 $ErrorActionPreference='Stop'
  # 下会把原生命令写给 stderr 的进度行当成 terminate error，直接中断脚本。
  # Start-Process 把 stdout/stderr 落到文件，用 exit code 判成败，最稳。
  $node = (Get-Command node -ErrorAction Stop).Source
  $cli = Join-Path $root 'node_modules\@tauri-apps\cli\tauri.js'
  if (-not (Test-Path $cli)) { throw "找不到 tauri CLI：$cli（先 pnpm install）" }
  $buildOut = Join-Path $env:TEMP 'comfyfleet-tauri-build.out.log'
  $buildErr = Join-Path $env:TEMP 'comfyfleet-tauri-build.err.log'
  $proc = Start-Process -FilePath $node -ArgumentList @($cli, 'build') `
    -WorkingDirectory $root -Wait -PassThru -NoNewWindow `
    -RedirectStandardOutput $buildOut -RedirectStandardError $buildErr
  if ($proc.ExitCode -ne 0) {
    Get-Content $buildErr -Tail 40 -ErrorAction SilentlyContinue | Write-Host
    throw "tauri build 失败（exit $($proc.ExitCode)），完整日志：$buildErr"
  }
}

# ---------------------------------------------------------------- 4. 收集产物
# Windows + NSIS 的 updater 产物就是安装器本身（外加 .exe.sig），没有 .nsis.zip。
# latest.json 里的 url 必须指向这个 exe，updater 下载后直接静默运行它。
$bundle = Join-Path $root 'src-tauri\target\release\bundle\nsis'
$setup = Get-ChildItem $bundle -Filter "ComfyFleet_$ver`_x64-setup.exe" -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $setup) {
  throw "没找到 $bundle\ComfyFleet_$ver`_x64-setup.exe。确认 build 成功，且 tauri.conf.json 里 bundle.createUpdaterArtifacts = true。"
}
$sig = "$($setup.FullName).sig"
if (-not (Test-Path $sig)) { throw "没找到签名文件：$sig（签名环境变量没生效？）" }

$url = "https://www.modelscope.cn/models/$Repo/resolve/master/$RepoPath/$($setup.Name)"
$manifest = [ordered]@{
  version   = $ver
  notes     = $Notes
  pub_date  = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
  platforms = [ordered]@{
    'windows-x86_64' = [ordered]@{
      signature = (Read-Text $sig).Trim()
      url       = $url
    }
  }
}

$outDir = Join-Path $root 'dist-update'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$manifestPath = Join-Path $outDir 'latest.json'
Write-TextNoBom $manifestPath ($manifest | ConvertTo-Json -Depth 5)
Copy-Item $setup.FullName -Destination $outDir -Force

Write-Host ""
Write-Host "已生成：" -ForegroundColor Green
Write-Host "  $manifestPath"
Write-Host "  $(Join-Path $outDir $setup.Name)"
Write-Host ""
Write-Host "清单里的下载地址：" -ForegroundColor Cyan
Write-Host "  $url"

# ---------------------------------------------------------------- 5. 上传
if ($Upload) {
  $token = $env:MODELSCOPE_API_TOKEN
  if (-not $token) { throw "设置了 -Upload 但没有 MODELSCOPE_API_TOKEN 环境变量" }
  $py = (Get-Command python -ErrorAction SilentlyContinue).Source
  if (-not $py) { throw "找不到 python，无法调用 ModelScope SDK" }
  $code = @"
from modelscope.hub.api import HubApi
api = HubApi()
api.login('$token')
api.upload_folder(
    repo_id='$Repo',
    folder_path=r'$outDir',
    path_in_repo='$RepoPath',
    commit_message='release $ver',
    repo_type='model',
    revision='master',
)
print('uploaded')
"@
  $tmp = Join-Path $env:TEMP "comfyfleet-upload-$ver.py"
  Write-TextNoBom $tmp $code
  # 同构建步骤：python 的 stderr 不能触发 terminate
  $prevEap = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  & $py $tmp
  $code = $LASTEXITCODE
  $ErrorActionPreference = $prevEap
  if ($code -ne 0) { throw "ModelScope 上传失败（exit $code）" }
  Write-Host "已上传到 https://www.modelscope.cn/models/$Repo" -ForegroundColor Green
} else {
  Write-Host ""
  Write-Host "下一步：把 dist-update 里的两个文件上传到 ModelScope 仓库的 $RepoPath/ 目录" -ForegroundColor Yellow
  Write-Host "  https://www.modelscope.cn/models/$Repo/files"
  Write-Host "（网页「上传文件」即可；也可以带 -Upload 走 SDK 自动传）"
}
