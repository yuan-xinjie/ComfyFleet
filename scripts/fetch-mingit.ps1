# 下载打包用的 MinGit（38 MB），并校验 SHA256。
#
# 为什么不在仓库里直接放 zip：38 MB 的二进制进 git 会一直膨胀历史。
# 构建前跑一次即可；`cargo test` 的 git 测试在没有 zip 时会自动跳过。
#
# 用法：pwsh -File scripts/fetch-mingit.ps1

$ErrorActionPreference = 'Stop'

$version = '2.56.0.4'
$url = "https://github.com/git-for-windows/git/releases/download/v2.56.0.windows.4/MinGit-$version-64-bit.zip"
$sha256 = 'E3BF78351FE777FBA47DDEE5950082F8A98BB0F02604FDC7BA3EE2CB68FFA877'

$root = Split-Path -Parent $PSScriptRoot
$dir = Join-Path $root 'src-tauri\resources'
$out = Join-Path $dir 'mingit.zip'

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

Write-Host "下载 MinGit $version ..."
Invoke-WebRequest -Uri $url -OutFile $out -TimeoutSec 600 -UseBasicParsing

$actual = (Get-FileHash $out -Algorithm SHA256).Hash
if ($actual -ne $sha256) {
    Remove-Item $out -Force
    throw "SHA256 不匹配：期望 $sha256，实际 $actual"
}

$mb = [math]::Round((Get-Item $out).Length / 1MB, 1)
Write-Host "完成：$out（$mb MB，SHA256 校验通过）"
