# MuRecorder 构建脚本：cargo build --release → 复制到 ShawMuOCR/Libs/x64
# 用法：powershell -ExecutionPolicy Bypass -File scripts\build.ps1 [-Configuration release|debug]
param(
    [ValidateSet("release", "debug")]
    [string]$Configuration = "release",
    # ShawMuOCR 主工程根目录。必须用绝对路径显式给出：
    # 早期实现用 "..\ShawMuOCR" 相对本核心目录推算，而核心在 C:\Rust\MuRecorder、
    # 上一级是 C:\Rust，于是静默复制到 C:\Rust\ShawMuOCR\Libs\x64 这个影子目录，
    # 真正的 C:\ShawMuOCR\Libs\x64 里始终残留旧 DLL —— "复制成功"与"实际生效"脱节。
    [string]$ShawMuOCRDir = "C:\ShawMuOCR"
)

$ErrorActionPreference = "Stop"

$coreDir = Split-Path -Parent $PSScriptRoot
$destDir = Join-Path $ShawMuOCRDir "Libs\x64"
$dllName = "recorder_core.dll"

if (-not (Test-Path (Join-Path $ShawMuOCRDir "ShawMuOCR.csproj"))) {
    throw "ShawMuOCRDir 不像主工程根目录（找不到 ShawMuOCR.csproj）: $ShawMuOCRDir"
}

Write-Host "[MuRecorder] cargo build ($Configuration) ..."
cargo build --$Configuration
if ($LASTEXITCODE -ne 0) { throw "cargo build 失败" }

$src = Join-Path $coreDir "target\$Configuration\$dllName"
if (-not (Test-Path $src)) { throw "找不到产物: $src" }

if (-not (Test-Path $destDir)) { New-Item -ItemType Directory -Path $destDir -Force | Out-Null }
$dest = Join-Path $destDir $dllName
Copy-Item $src $dest -Force

# 复制后立即校验哈希：DLL 被占用/权限不足时 Copy-Item 可能"成功"但内容未更新，
# 会让上层测试继续加载旧产物，排查成本极高。
$srcHash = (Get-FileHash $src -Algorithm SHA256).Hash
$destHash = (Get-FileHash $dest -Algorithm SHA256).Hash
if ($srcHash -ne $destHash) {
    throw "复制后哈希不一致（目标可能被占用）: $dest`n  src=$srcHash`n dest=$destHash"
}
Write-Host "[MuRecorder] 已复制到 $dest（SHA256 $($srcHash.Substring(0,12))…）"

# 构建后自检：DLL 导出 recorder_version（避免复制了残缺产物）
# dumpbin 不在 PATH 时按 vswhere → 已知 VS 安装路径 → PATH 顺序查找
function Resolve-Dumpbin {
    $cmd = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        $vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
        if ($vsPath) {
            $candidate = Get-ChildItem (Join-Path $vsPath "VC\Tools\MSVC") -Directory -ErrorAction SilentlyContinue |
                Sort-Object Name -Descending |
                ForEach-Object { Join-Path $_.FullName "bin\Hostx64\x64\dumpbin.exe" } |
                Where-Object { Test-Path $_ } | Select-Object -First 1
            if ($candidate) { return $candidate }
        }
    }
    # 兜底：本机 VS 2026 非标准安装路径（未被 vswhere 登记）
    $fallback = Get-ChildItem "C:\Program Files\Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe" -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending | Select-Object -First 1
    if ($fallback) { return $fallback.FullName }
    return $null
}

$dumpbin = Resolve-Dumpbin
if ($dumpbin) {
    $exports = & $dumpbin /exports $src 2>$null
    if ($LASTEXITCODE -eq 0) {
        # 注意：$exports 是行数组，直接 -notmatch 会把"非匹配行"当真值导致全量误判，
        # 必须先拼接为单一文本再做包含判断。
        $exportText = ($exports -join "`n")
        $need = @("recorder_create", "recorder_start", "recorder_stop", "recorder_get_stats", "recorder_get_last_error", "recorder_destroy", "recorder_version", "recorder_has_hardware_encoder")
        $missing = $need | Where-Object { $exportText -notmatch [regex]::Escape($_) }
        if ($missing) { throw "DLL 缺少导出: $($missing -join ', ')" }
        Write-Host "[MuRecorder] 导出符号自检通过（8/8）"
    } else {
        Write-Host "[MuRecorder] 警告：dumpbin 执行失败，跳过导出自检" -ForegroundColor Yellow
    }
} else {
    Write-Host "[MuRecorder] 警告：未找到 dumpbin，跳过导出自检" -ForegroundColor Yellow
}
