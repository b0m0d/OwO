# gate.ps1 — R5 一键回归门禁（主控收尾使用；R5 Agent 3 交付；R8 增加 UTF-8 校验与 workspace 全量测试开关）
# 顺序：UTF-8 校验 → fmt → clippy → test（默认 -p owo-agent-server；-WorkspaceTests 跑全量）→ node --check → 可选真实 eval。
# 参数：
#   -SkipClippy     跳过 clippy
#   -SkipTest       跳过 cargo test
#   -SkipNode       跳过 node --check
#   -SkipUtf8       跳过 UTF-8 扫描
#   -WorkspaceTests 跑 cargo test --workspace（默认只跑 server）
#   -WithRealEval   追加运行真实模型 eval（需 OPENAI_API_KEY；POST /eval/gate/run）
#   -ServerPort     服务端口（-WithRealEval 时用于 curl，默认 4098）
# 汇总退出码：任一失败打印失败清单并以非零退出。
param(
    [switch]$SkipClippy,
    [switch]$SkipTest,
    [switch]$SkipNode,
    [switch]$SkipUtf8,
    [switch]$WorkspaceTests,
    [switch]$WithRealEval,
    [int]$ServerPort = 4098
)

$ErrorActionPreference = "Continue"
# §2.4 资源红线执行层：统一经 ci-shared（与 ci-gate/dev 同一实现）——所有 cargo 调用走
# Invoke-CiCargo，显式注入 -j / --test-threads、启动前内存门 + 构建空闲门、30s 心跳。
. (Join-Path $PSScriptRoot "ci-shared.ps1")
Initialize-CiPath
# §7.2 收口：不再写死本机绝对路径（cargo 在 PATH 上直接复用，否则探测
# %USERPROFILE%\.cargo\bin）；ORT 统一经 resolve-ort.ps1 进程级解析注入，
# 与 dev.ps1 / ci-gate / package-desktop / build-installer 同源。
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
    if (Test-Path $cargoBin) { $env:PATH = "$cargoBin;$env:PATH" }
}
. (Join-Path $PSScriptRoot "resolve-ort.ps1")
try {
    $null = Resolve-OwoOrtEnv -NoDownload -Quiet
} catch {
    Write-Host "[gate] ONNX Runtime 解析失败：$($_.Exception.Message)" -ForegroundColor Red
    exit 2
}
$root = Split-Path -Parent $PSScriptRoot   # agent-sdk/
$failures = @()
$steps = @()

function Run-Step {
    param([string]$Name, [scriptblock]$Block, [string]$Cwd)
    Write-Host "==> $Name" -ForegroundColor Cyan
    Push-Location $Cwd
    try {
        # §2.4 红线 10：步骤开始前清空退出码——若本步的 cargo 经统一入口"启动失败"
        # （Invoke-CiLoggedCommand 会自行写非零码），不残留上一步的旧值；仍按
        # "$null -ne 0 才判失败" 的原语义分支，失败不会被改写成通过。
        # 注意：不能依赖 `& $Block` 隐式清空——块内若含 `return`（受控表达失败），
        # PowerShell 5.1 不会重置调用方的 $LASTEXITCODE，会把上一步的旧码误记为本步失败。
        $global:LASTEXITCODE = $null
        & $Block
        # $LASTEXITCODE 可能为 $null（本步未运行外部程序）；$null -ne 0 会误判失败。
        $exitCode = $LASTEXITCODE
        if ($null -ne $exitCode -and $exitCode -ne 0) {
            $script:failures += $Name
            Write-Host "    [FAIL] $Name" -ForegroundColor Red
        } else {
            Write-Host "    [OK]   $Name" -ForegroundColor Green
        }
    } catch {
        $script:failures += $Name
        Write-Host "    [FAIL] $Name : $_" -ForegroundColor Red
    } finally {
        Pop-Location
    }
    $script:steps += $Name
}

# 0) UTF-8 扫描（AGENTS.md 硬性要求：所有源文件 UTF-8；ps1 需 BOM）
if (-not $SkipUtf8) {
    Run-Step "UTF-8 校验（rs/ts/js/html/json/css/md/ps1）" {
        $bad = @()
        $exts = @(".rs", ".ts", ".js", ".html", ".json", ".css", ".md")
        $files = Get-ChildItem $root -Recurse -File |
            Where-Object { $_.FullName -notmatch "\\target\\" -and $_.FullName -notmatch "\\node_modules\\" -and $_.FullName -notmatch "\\.git\\" } |
            Where-Object { $exts -contains $_.Extension -or $_.Extension -eq ".ps1" }
        $strict = New-Object System.Text.UTF8Encoding($false, $true)
        foreach ($f in $files) {
            try {
                $bytes = [System.IO.File]::ReadAllBytes($f.FullName)
                $null = $strict.GetString($bytes)
                if ($f.Extension -eq ".ps1") {
                    if (-not ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF)) {
                        $bad += "$($f.FullName)（.ps1 需带 UTF-8 BOM）"
                    }
                }
            } catch {
                $bad += "$($f.FullName)（$($_.Exception.Message)）"
            }
        }
        Write-Host "    [utf8] scanned=$($files.Count) bad=$($bad.Count)"
        if ($bad.Count -gt 0) {
            Write-Host "    UTF-8 违规文件：" -ForegroundColor Red
            foreach ($b in $bad) { Write-Host "      - $b" -ForegroundColor Red }
            exit 1
        }
    } $root
}

# 1) fmt
Run-Step "cargo fmt --all -- --check" {
    # §2.4：fmt 非编译子命令（Protect-CiCargoArguments 实测不注入 -j），normal 档。
    # §2.4 红线 10：统一入口在"启动失败 / 超时(124) / 内存守护(137)"时都写非零
    # $global:LASTEXITCODE；本步用 return 表达成败，绝不把失败吞成通过。
    Invoke-CiCargo -Arguments @('fmt', '--all', '--', '--check') -Cwd $root `
        -HeartbeatSec 30 -Label 'gate-fmt' -PolicyMode 'normal'
    if ($global:LASTEXITCODE -ne 0) { return }
} $root

# 2) clippy（workspace 全量；--all-targets）
if (-not $SkipClippy) {
    Run-Step "cargo clippy --workspace --all-targets -- -D warnings" {
        # §2.4 红线 2：完整 workspace lint 派生大量 rustc → strict 档（-j 1 / 测试线程 1），
        # 不得为缩短门禁时间提高并发（红线 8）。原 `-D warnings` 失败语义经退出码保留。
        Invoke-CiCargo -Arguments @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings') -Cwd $root `
            -HeartbeatSec 30 -Label 'gate-clippy' -PolicyMode 'strict'
        # §2.4 红线 10：return 非零退出码——Run-Step 的 catch 分支会把本步记入失败清单，
        # 门禁最终 exit 1；内存守护(137)/超时(124)/启动失败同样走这里，不会记成通过。
        if ($global:LASTEXITCODE -ne 0) { exit $global:LASTEXITCODE }
    } $root
}

# 3) test（默认 server 全量；-WorkspaceTests 时跑 workspace 全量）
if (-not $SkipTest) {
    if ($WorkspaceTests) {
        Run-Step "cargo test --workspace" {
            # §2.4 红线 2：workspace 全量测试含 core 的 36 个集成测试文件与原生依赖链接
            # → strict 档（-j 1 / --test-threads=1 由参数归一层注入）。
            Invoke-CiCargo -Arguments @('test', '--workspace') -Cwd $root `
                -HeartbeatSec 30 -Label 'gate-test-workspace' -PolicyMode 'strict'
            if ($global:LASTEXITCODE -ne 0) { return }
        } $root
    } else {
        Run-Step "cargo test -p owo-agent-server" {
            # §2.4 红线 1：单包（server）定向测试 → normal 档（-j 2 / --test-threads=2）。
            Invoke-CiCargo -Arguments @('test', '-p', 'owo-agent-server') -Cwd $root `
                -HeartbeatSec 30 -Label 'gate-test-server' -PolicyMode 'normal'
            if ($global:LASTEXITCODE -ne 0) { return }
        } $root
    }
}

# 4) node --check（app.js + 全部面板）
if (-not $SkipNode) {
    $jsFiles = @("$root\desktop\web\app.js") + (Get-ChildItem "$root\desktop\web\panels" -Filter *.panel.js | ForEach-Object { $_.FullName })
    Run-Step "node --check ($($jsFiles.Count) 个 JS 文件)" {
        $bad = $false
        foreach ($f in $jsFiles) {
            node --check $f 2>&1 | Out-Host
            if ($LASTEXITCODE -ne 0) { $bad = $true }
        }
        if ($bad) { exit 1 }
    } $root
}

# 5) 可选真实 eval（走 HTTP 面，验证 /eval/gate/run 链路）
if ($WithRealEval) {
    if (-not $env:OPENAI_API_KEY) {
        Write-Host "==> 真实 eval 跳过：缺少 OPENAI_API_KEY" -ForegroundColor Yellow
    } else {
        Run-Step "真实 eval：POST /eval/gate/run（服务需已启动于 127.0.0.1:$ServerPort）" {
            $resp = curl.exe --max-time 300 -s -X POST "http://127.0.0.1:$ServerPort/eval/gate/run" -H "Content-Type: application/json" -d "{}"
            Write-Host $resp
            $json = $resp | ConvertFrom-Json
            if (-not $json.ok -and -not $json.skipped) { exit 1 }
        } $root
    }
}

# 汇总
Write-Host ""
Write-Host "==== 门禁汇总（$($steps.Count) 步）====" -ForegroundColor Cyan
foreach ($s in $steps) {
    $mark = if ($failures -contains $s) { "X" } else { "v" }
    Write-Host "  [$mark] $s"
}
if ($failures.Count -gt 0) {
    Write-Host "失败清单：" -ForegroundColor Red
    foreach ($f in $failures) { Write-Host "  - $f" -ForegroundColor Red }
    exit 1
}
Write-Host "全部通过" -ForegroundColor Green
exit 0
