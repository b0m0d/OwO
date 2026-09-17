#requires -Version 5.1
<#
resolve-ort.ps1 — ONNX Runtime 原生依赖统一解析入口（重构方案 §7.2）。

职责（§7.2 五条，全部委托 init-dev-env.ps1 内的单一实现，本文件不新增
第二份探测逻辑）：
  1. 优先使用调用者显式设置的 SHERPA_ONNX_LIB_DIR / ORT_LIB_PATH；
  2. 其次稳定机器缓存 %LOCALAPPDATA%\OwO\Agent\runtime\<version>（与可删除
     的 target/ 解耦），再次历史 target\sherpa-onnx-prebuilt 布局（兼容）；
  3. 校验目录内实际存在 onnxruntime.lib 并达到最小尺寸，报告版本与 CRT 形态
     （官方 sherpa-onnx win-x64 **static-MT** 预构建、归档 sha256 固定）；
  4. 只向当前进程注入 SHERPA_ONNX_LIB_DIR / ORT_LIB_PATH / ORT_LIB_LOCATION，
     绝不写用户/机器级永久环境变量；
  5. 找不到时立即报可操作错误（附 init-dev-env -EnsureOrt 指引），不等链接
     阶段 LNK1120。

用法：
  dot-source： . scripts\resolve-ort.ps1 ; Resolve-OwoOrtEnv [-EnsureOrt] [-NoDownload]
  直接执行：   pwsh -File scripts\resolve-ort.ps1 [-EnsureOrt] [-NoDownload] [-Json]
               stdout = 解析出的 lib 目录（-Json 时输出元数据对象），exit 0；
               不可解析 exit 2（stderr 带可操作错误）。

调用面：开发构建（dev.ps1）、测试、CI（ci-gate.ps1）、桌面 sidecar 与
installer（package-desktop / build-installer）必须走本入口（或其背后的
init-dev-env 同名函数），禁止再复制探测逻辑。
#>
[CmdletBinding()]
param(
    # ⚠ dot-source 参数卫生（本文件实测踩坑）：dot-source 会把 param 变量
    # 注入**调用方**作用域——裸名 `Json` 曾以 [switch] 类型污染调用方的
    # `$json = ConvertTo-Json ...` 赋值（PowerShell 变量名不区分大小写且
    # 首次绑定定型，后续赋 string 触发 String→SwitchParameter 异常）；裸名
    # `EnsureOrt` 会静默重置 ci-gate 自己的 -EnsureOrt 开关。全部参数带
    # $Owo 前缀，CLI 面经 Alias 保持原样。
    # 缺失时允许从官方 release 下载并校验（sha256 固定）进稳定缓存。
    [Alias('EnsureOrt')]
    [switch]$OwoEnsureOrt,
    # 禁止任何下载（CI 保守模式）。
    [Alias('NoDownload')]
    [switch]$OwoNoDownload,
    # 输出 JSON 元数据而非纯路径。
    [Alias('Json')]
    [switch]$OwoJson,
    # 安静模式（dot-source 场景由调用方自己打印）。
    [Alias('Quiet')]
    [switch]$OwoQuiet
)

# 注意：本文件是「库 + CLI」双面入口。作为库被 dot-source 时**不得**改动
# 调用方的 $ErrorActionPreference（历史教训：Stop 泄漏进 ci-gate 会话后，
# cargo/npm 的正常 stderr 进度行被当作终止错误，三个 cargo 步骤与 TS 步骤
# 全部假红——门禁输出即证据，宁宽勿误杀）。Stop 仅在直接执行分支设置。
. (Join-Path $PSScriptRoot "init-dev-env.ps1")

function Resolve-OwoOrtSource {
    # 分类解析来源（报告用；判定与 Resolve-OwoOnnxRuntimeLib 的探测序一致）。
    param([string]$LibDir)
    $callerEnv = if ($env:SHERPA_ONNX_LIB_DIR) { $env:SHERPA_ONNX_LIB_DIR } else { $env:ORT_LIB_PATH }
    if ($callerEnv -and (Test-Path $callerEnv) -and ((Resolve-Path $callerEnv).Path -ieq (Resolve-Path $LibDir).Path)) {
        # 注：本函数应在注入前调用；注入后 callerEnv 已等于 LibDir，由调用方保证顺序。
        return "caller-env"
    }
    if ($LibDir -like "$($script:OwoRuntimeCache)*") { return "machine-cache" }
    if ($LibDir -like "$($script:OwoSdkRoot)\target\sherpa-onnx-prebuilt*") { return "legacy-target-cache" }
    return "other"
}

function Resolve-OwoOrtEnv {
    <# 解析 + 进程级注入 + 元数据报告。不可解析时 throw（消息可操作）。 #>
    [CmdletBinding()]
    param(
        [switch]$EnsureOrt,
        [switch]$NoDownload,
        [switch]$Quiet
    )
    # 来源分类必须在注入前采样（否则 caller-env 判定失真）。
    $probeBefore = Get-OwoOrtLibDir
    $source = if ($probeBefore) { Resolve-OwoOrtSource $probeBefore } else { $null }
    $lib = Resolve-OwoOnnxRuntimeLib -AllowDownload:($EnsureOrt -and -not $NoDownload)
    if ($source -eq $null) { $source = "downloaded-to-cache" }
    # §7.2.4 仅进程级注入。
    $env:SHERPA_ONNX_LIB_DIR = $lib
    $env:ORT_LIB_PATH = $lib
    $env:ORT_LIB_LOCATION = $lib
    $libBytes = (Get-Item (Join-Path $lib "onnxruntime.lib")).Length
    if (-not $Quiet) {
        Write-Host ("[ort] resolved ({0}) v{1} static-MT win-x64; onnxruntime.lib={2:N1}MB; source={3}; 进程级注入 SHERPA_ONNX_LIB_DIR/ORT_LIB_PATH/ORT_LIB_LOCATION" -f `
            $lib, $script:OwoSherpaVersion, ($libBytes / 1MB), $source) -ForegroundColor Green
    }
    return [pscustomobject]@{
        lib_dir            = $lib
        version            = $script:OwoSherpaVersion
        asset              = $script:OwoSherpaAsset
        archive_sha256     = $script:OwoSherpaSha256
        crt                = "static-MT"
        target             = $script:OwoRequiredTarget
        onnxruntime_bytes  = $libBytes
        source             = $source
    }
}

# 直接执行入口（sidecar/installer/人工排障共用同一命令面）。
if ($MyInvocation.InvocationName -ne ".") {
    try {
        $meta = Resolve-OwoOrtEnv -EnsureOrt:$OwoEnsureOrt -NoDownload:$OwoNoDownload -Quiet:$OwoQuiet
        if ($OwoJson) {
            $meta | ConvertTo-Json -Compress | Write-Output
        } else {
            Write-Output $meta.lib_dir
        }
        exit 0
    } catch {
        [Console]::Error.WriteLine("[ort] ERROR: $($_.Exception.Message)")
        exit 2
    }
}
