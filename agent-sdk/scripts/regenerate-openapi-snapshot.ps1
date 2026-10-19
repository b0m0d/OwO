# regenerate-openapi-snapshot.ps1 — R3（§8.1 配套）：权威契约快照一键再生成。
#
# 背景：`route_contract_tests.rs` 以 include_str! 把 `clients/ts/openapi.json` 作为
# 权威基准，并断言快照路径集与 served `/openapi.json` **双向一致**。加/改路由后必须
# 同步快照，否则契约测试红。历史上每次手写一次性 sync 脚本（seven/eight/capabilities），
# 本脚本把「起临时服务 → 抓取 → 规范化写回 → 重新生成 TS 类型」收口为唯一入口。
#
# 用法（在 agent-sdk/ 目录）：
#   .\scripts\regenerate-openapi-snapshot.ps1                 # 用当前 debug 构建
#   .\scripts\regenerate-openapi-snapshot.ps1 -Build          # 先 cargo build -p owo-agent-cli
#   .\scripts\regenerate-openapi-snapshot.ps1 -SkipTsTypes    # 只写回快照，不跑 openapi-typescript
#
# 安全：服务端口固定为 127.0.0.1 上的动态空闲端口，数据根为临时目录，脚本结束即关闭；
# token 只经 dev 引导端点在本机 loopback 使用，不落任何文件/日志。
#requires -Version 5.1
[CmdletBinding()]
param(
    [switch]$Build,
    [switch]$SkipTsTypes,
    [int]$Port = 0,
    [string]$AgentRoot = ''
)

$ErrorActionPreference = 'Stop'
# §2.4 Rust 编译与测试资源安全红线：统一经 ci-shared 执行层（与 ci-gate/dev 同一实现）。
# 本脚本唯一的 cargo 调用是 debug 构建 owo-agent-cli；临时 serve 进程走 Start-Process，
# 不属编译段，端口/服务/快照写回逻辑保持原样。
. (Join-Path $PSScriptRoot "ci-shared.ps1")
Initialize-CiPath
$sdkRoot = if ($AgentRoot) { $AgentRoot } else { Split-Path -Parent $PSScriptRoot }
Set-Location $sdkRoot
$exe = Join-Path $sdkRoot 'target\debug\owo-agent.exe'

if ($Build -or -not (Test-Path $exe)) {
    Write-Host '[regal] 构建 debug 核心（含 ONNX Runtime 环境注入）…'
    . (Join-Path $sdkRoot 'scripts\resolve-ort.ps1')
    Resolve-OwoOrtEnv -Quiet
    # §2.4 红线 1：单包 debug 构建（cargo build -p owo-agent-cli）→ normal 档（-j 2）；
    # 非 release、非 workspace 全量，无需降为 1。原 `-q` 只压 cargo 进度行，保留。
    # §2.4 红线 10：成败用 $global:LASTEXITCODE 判定——统一入口在启动失败(1)/超时(124)/
    # 内存守护(137)时都写非零码，仍按原语义 throw（EAP=Stop 下 throw 即 exit≠0）。
    Invoke-CiCargo -Arguments @('build', '-q', '-p', 'owo-agent-cli') -Cwd $sdkRoot `
        -HeartbeatSec 30 -Label 'regal-build' -PolicyMode 'normal'
    if ($global:LASTEXITCODE -ne 0) { throw "cargo build 失败（exit=$global:LASTEXITCODE）" }
}
if (-not (Test-Path $exe)) { throw "找不到核心可执行文件：$exe（先加 -Build）" }

if ($Port -le 0) {
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $Port = $listener.LocalEndpoint.Port
    $listener.Stop()
}

$dataRoot = Join-Path ([IO.Path]::GetTempPath()) ("owo-openapi-regen-" + [Guid]::NewGuid().ToString('N').Substring(0, 8))
$workspace = Join-Path $dataRoot 'workspace'
New-Item -ItemType Directory -Force -Path $workspace | Out-Null
$errLog = Join-Path $dataRoot 'serve-stderr.log'

# 运行时依赖（§7 唯一入口）：ONNX Runtime 进程级注入；模型凭据仅从用户级环境变量
# 取用并注入本进程，绝不回显（AGENTS.md 红线）。
. (Join-Path $sdkRoot 'scripts\resolve-ort.ps1')
Resolve-OwoOrtEnv -Quiet
if (-not $env:OPENAI_API_KEY) {
    $key = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
    if ($key) { $env:OPENAI_API_KEY = $key }
}
if (-not $env:OPENAI_API_KEY) {
    throw '缺少 OPENAI_API_KEY（用户级环境变量）；serve 启动需要模型配置。'
}

$prevData = $env:OWO_AGENT_DATA
$env:OWO_AGENT_DATA = $dataRoot
$proc = $null
try {
    $proc = Start-Process -FilePath $exe `
        -ArgumentList @('serve', '--port', "$Port", '--workspace', $workspace) `
        -PassThru -WindowStyle Hidden -RedirectStandardError $errLog
    Write-Host "[regal] 临时服务 pid=$($proc.Id) port=$Port（数据根=临时目录）"

    $base = "http://127.0.0.1:$Port"
    $ready = $false
    for ($i = 0; $i -lt 100; $i++) {
        Start-Sleep -Milliseconds 200
        try {
            $health = Invoke-RestMethod -Uri "$base/health" -TimeoutSec 2
            if ($health) { $ready = $true; break }
        } catch { }
    }
    if (-not $ready) {
        $tail = if (Test-Path $errLog) { (Get-Content $errLog -Tail 12 -ErrorAction SilentlyContinue) -join ' | ' } else { '(无日志)' }
        throw "核心服务在 20s 内未就绪：$tail"
    }

    $specPath = Join-Path $dataRoot 'openapi.served.json'
    Invoke-WebRequest -Uri "$base/openapi.json" -OutFile $specPath -UseBasicParsing -TimeoutSec 20
    $servedBytes = (Get-Item $specPath).Length
    if ($servedBytes -lt 10000) { throw "served /openapi.json 仅 $servedBytes 字节，疑似异常" }

    $snapPath = Join-Path $sdkRoot 'clients\ts\openapi.json'
    Copy-Item $specPath $snapPath -Force
    Write-Host "[regal] 快照已写回：$snapPath（$servedBytes 字节）"
} finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    if ($prevData) { $env:OWO_AGENT_DATA = $prevData } else { Remove-Item Env:OWO_AGENT_DATA -ErrorAction SilentlyContinue }
    Start-Sleep -Milliseconds 300
    Remove-Item -Recurse -Force $dataRoot -ErrorAction SilentlyContinue
}

# 快照 = served /openapi.json 原样字节（可逐字节复现，不二次规范化）；
# 只做解析自检，防止半截写入污染权威契约。
$parsed = Get-Content $snapPath -Raw | ConvertFrom-Json
$pathCount = @($parsed.paths.PSObject.Properties.Name).Count
if ($pathCount -lt 50) { throw "快照解析异常：paths 仅 $pathCount 条" }
Write-Host "[regal] 快照解析通过：paths=$pathCount（含 /diagnostics/requests）"

if (-not $SkipTsTypes) {
    Push-Location (Join-Path $sdkRoot 'clients\ts')
    try {
        npm run --silent generate:local
        if ($LASTEXITCODE -ne 0) { throw 'openapi-typescript 生成失败' }
        npm run --silent typecheck
        if ($LASTEXITCODE -ne 0) { throw 'TS typecheck 失败' }
        Write-Host '[regal] schema.d.ts 已再生成并通过 typecheck'
    } finally { Pop-Location }
}
Write-Host '[regal] 完成：下一步跑 route 契约测试确认双向一致。'
