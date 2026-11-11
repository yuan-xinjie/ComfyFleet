# 下载打包用的 uv（uv.exe，约 54 MB），并校验 SHA256。
#
# 为什么不在仓库里直接放 exe：54 MB 的二进制进了 git 会永久留在历史里，
# 每次 clone 都要下载。与 fetch-mingit.ps1 同一套做法：构建前跑一次即可。
#
# 用法：pwsh -File scripts/fetch-uv.ps1

$ErrorActionPreference = 'Stop'

# 内置 uv 的版本；升级时同步改 tauri.conf.json 里对 resources/uv.exe 的引用说明。
$version = '0.8.8'
$url = "https://github.com/astral-sh/uv/releases/download/$version/uv-x86_64-pc-windows-msvc.zip"
# 解压后 uv.exe 的 SHA256（与项目此前内置的文件一致，可复现）
$sha256 = '4E5DD7C2D89C2937C2ABB80DD6123DD541112F9EC3C2D0411C284383A135DF8D'

$root = Split-Path -Parent $PSScriptRoot
$dir = Join-Path $root 'src-tauri\resources'
$out = Join-Path $dir 'uv.exe'

New-Item -ItemType Directory -Path $dir -Force | Out-Null

if (Test-Path $out) {
    $existing = (Get-FileHash $out -Algorithm SHA256).Hash
    if ($existing -eq $sha256) {
        Write-Host "已存在且校验通过：$out"
        return
    }
    Write-Host "已存在但校验不符，重新下载"
    Remove-Item $out -Force
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) "comfyfleet-uv-$version"
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
$zip = Join-Path $tmp 'uv.zip'

try {
    Write-Host "下载 uv $version ..."
    Invoke-WebRequest -Uri $url -OutFile $zip -TimeoutSec 600 -UseBasicParsing
    Expand-Archive -Path $zip -DestinationPath $tmp -Force

    $exe = Get-ChildItem $tmp -Recurse -Filter 'uv.exe' | Select-Object -First 1
    if (-not $exe) { throw "压缩包里没有 uv.exe：$zip" }

    $actual = (Get-FileHash $exe.FullName -Algorithm SHA256).Hash
    if ($actual -ne $sha256) {
        throw "SHA256 不匹配：期望 $sha256，实际 $actual"
    }

    Copy-Item $exe.FullName -Destination $out -Force
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

$mb = [math]::Round((Get-Item $out).Length / 1MB, 1)
Write-Host "完成：$out（$mb MB，SHA256 校验通过）"
