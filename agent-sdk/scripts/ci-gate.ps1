# ci-gate.ps1 — agent-sdk CI 核心门禁（PR/Merge 工作流与本地验证共用；幂等、只读为主）
# 步骤顺序：utf8 → permission-ctor → forbidden-files → fmt → clippy → test →
#          route-contract → node → ts → desktop(src-tauri: fmt/clippy/test)
# 约定：块内通过退出码或 $global:LASTEXITCODE = 1 表达失败（避免 throw 吞掉已捕获输出）；
#       前置条件缺失用 return 实现受控跳过（绿色 + SKIP 说明）。
# 用法：
#   powershell -ExecutionPolicy Bypass -File scripts\ci-gate.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\ci-gate.ps1 -Step fmt
#   powershell -ExecutionPolicy Bypass -File scripts\ci-gate.ps1 -Step clippy -LogDir "$env:TEMP\ci-gate-logs"
# 参数：
#   -Step <词>           只运行 Id 包含该词的步骤（CI 拆分步骤与本地排查用；-Step desktop 跑壳三步）
#   -SkipFmt/-SkipClippy/-SkipTest/-SkipNode/-SkipTs/-SkipRouteContract/-SkipUtf8/-SkipPermissionCtor/-SkipDesktop
#   -ServerOnly          workspace 测试退化为只测 owo-agent-server 单包
#   -LogDir <目录>       把每步输出与 summary.json 落盘（诊断 artifact）
# 退出码：0 = 全过；1 = 存在失败步骤。
param(
    [string]$Step = "",
    [switch]$SkipFmt,
    [switch]$SkipClippy,
    [switch]$SkipTest,
    [switch]$SkipNode,
    [switch]$SkipTs,
    [switch]$SkipRouteContract,
    [switch]$SkipUtf8,
    [switch]$SkipPermissionCtor,
    # R3：桌面壳（src-tauri 独立 workspace）三步；本地快速跑主仓时可跳过。
    [switch]$SkipDesktop,
    [switch]$ServerOnly,
    # §7.2：新终端直跑 CI 时允许把缺失的 ORT 下载进稳定缓存（默认保守：
    # 探测失败即快速报错，不等链接阶段 LNK1120）。
    [switch]$EnsureOrt,
    [string]$LogDir = ""
)

$ErrorActionPreference = "Continue"
. (Join-Path $PSScriptRoot "ci-shared.ps1")
Initialize-CiPath

# §7.2：ORT 统一解析入口（进程级注入；与 dev/package/build-installer 同源）。
. (Join-Path $PSScriptRoot "resolve-ort.ps1")
try {
    $null = Resolve-OwoOrtEnv -EnsureOrt:$EnsureOrt -NoDownload:(-not $EnsureOrt) -Quiet
} catch {
    Write-Host "[ci] ONNX Runtime 解析失败：$($_.Exception.Message)" -ForegroundColor Red
    exit 2
}

$script:ciStepFilter = $Step.ToLowerInvariant()
$root = Get-CiRepoRoot

# 0) UTF-8 扫描（AGENTS.md 硬性要求：源文件 UTF-8；.ps1 需带 UTF-8 BOM）
if (-not $SkipUtf8) {
    Invoke-CiStep -Name "UTF-8 校验（rs/ts/js/html/json/css/md/ps1）" -Id "utf8" -Cwd $root -LogDir $LogDir -Block {
        $bad = @()
        $exts = @(".rs", ".ts", ".js", ".html", ".json", ".css", ".md", ".ps1")
        $files = Get-ChildItem $root -Recurse -File |
            Where-Object { $_.FullName -notmatch "\\target\\" -and $_.FullName -notmatch "\\node_modules\\" -and $_.FullName -notmatch "\\.git\\" -and $_.FullName -notmatch "\\scratch-" } |
            Where-Object { $exts -contains $_.Extension }
        $strict = New-Object System.Text.UTF8Encoding($false, $true)
        foreach ($f in $files) {
            try {
                $bytes = [System.IO.File]::ReadAllBytes($f.FullName)
                $null = $strict.GetString($bytes)
                if ($f.Extension -eq ".ps1" -and -not ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF)) {
                    $bad += "$($f.FullName)（.ps1 需带 UTF-8 BOM）"
                }
            } catch {
                $bad += "$($f.FullName)（$($_.Exception.Message)）"
            }
        }
        Write-Host ("    [utf8] scanned={0} bad={1}" -f $files.Count, $bad.Count)
        if ($bad.Count -gt 0) {
            $badList = $bad | Sort-Object -Unique
            Write-Host "    UTF-8 违规文件：" -ForegroundColor Red
            foreach ($b in $badList) {
                Write-Host ("      - {0}" -f $b) -ForegroundColor Red
                Write-Output $b
            }
            $global:LASTEXITCODE = 1
        } else {
            $global:LASTEXITCODE = 0
        }
    }
}

# 0.5) PermissionRequest 构造检查（重构方案 3.1：业务模块禁止直接拼装 request_id；
#      除 permissions.rs 与协议类型定义外一律经 PermissionRequest::new 创建））
if (-not $SkipPermissionCtor) {
    Invoke-CiStep -Name "构造检查（PermissionRequest { 只允许出现在 permissions.rs）" -Id "permission-ctor" -Cwd $root -LogDir $LogDir -Block {
        # 合法出现点：permissions.rs（定义/求值）、owo-agent-protocol（协议类型定义）、
        # `-> PermissionRequest {`（返回类型）、`SseEvent::PermissionRequest {`（模式匹配）。
        $allowed = @(
            (Join-Path $root "crates\owo-agent-core\src\permissions.rs"),
            (Join-Path $root "crates\owo-agent-protocol\src\lib.rs")
        )
        $violations = @()
        $rsFiles = Get-ChildItem (Join-Path $root "crates") -Recurse -Filter *.rs |
            Where-Object { $_.FullName -notmatch "\\target\\" }
        foreach ($f in $rsFiles) {
            if ($allowed -contains $f.FullName) { continue }
            $lineno = 0
            foreach ($line in [System.IO.File]::ReadAllLines($f.FullName)) {
                $lineno++
                if ($line -match "PermissionRequest\s*\{") {
                    # 返回类型与模式匹配不是字面量构造，跳过（防误报）。
                    if ($line -match "->\s*PermissionRequest\s*\{" -or $line -match "::\s*PermissionRequest\s*\{") { continue }
                    $violations += "$($f.FullName):$lineno"
                }
            }
        }
        Write-Host ("    [permission-ctor] scanned={0} violations={1}" -f $rsFiles.Count, $violations.Count)
        if ($violations.Count -gt 0) {
            Write-Host "    业务模块直接构造 PermissionRequest（应改用构造函数）：" -ForegroundColor Red
            foreach ($v in $violations) {
                Write-Host ("      - {0}" -f $v) -ForegroundColor Red
                Write-Output $v
            }
            $global:LASTEXITCODE = 1
        } else {
            $global:LASTEXITCODE = 0
        }
    }
}

# 0.6) forbidden-files 检查（重构方案 10.6：拒绝编辑器 workspace、密钥文件、日志、
#      临时数据库与构建缓存进入源码目录；误放文件由人工移出，本步骤防止回潮；已忽略运行产物跳过）
if (-not $SkipPermissionCtor) {
    Invoke-CiStep -Name "forbidden-files（.code-workspace/.log/.db+密钥/构建缓存不得进源码目录）" -Id "forbidden-files" -Cwd $root -LogDir $LogDir -Block {
        $forbidden = @()
        $allFiles = Get-ChildItem $root -Recurse -File |
            Where-Object { $_.FullName -notmatch "\\target\\" -and $_.FullName -notmatch "\\node_modules\\" -and $_.FullName -notmatch "\\.git\\" -and $_.FullName -notmatch "\\scratch-" }
        # 源码扩展名：文件名含 credentials 之类是合法模块命名，不做密钥匹配；
        # 但 .json/.ps1/.md 等仍可能承载真实密钥（保持检查）。
        $sourceExts = @(".rs", ".ts", ".js", ".css", ".html")
        $skipped = 0
        foreach ($f in $allFiles) {
            $name = $f.Name
            # PS 5.1 无 [IO.Path]::GetRelativePath：手动算根相对路径（大小写不敏感）。
            $full = $f.FullName
            $rel = $full
            if ($full.StartsWith($root, [System.StringComparison]::OrdinalIgnoreCase)) {
                $rel = $full.Substring($root.Length).TrimStart('\', '/')
            }
            # 被 git 忽略的运行产物（sim/logs、.owo-agent/、基线/门禁日志等）不在源码目录。
            git -C $root check-ignore -q $full 2>$null
            if ($LASTEXITCODE -eq 0) { $skipped++; continue }
            $lower = $name.ToLowerInvariant()
            if ($lower -match "\.code-workspace$") { $forbidden += "$rel（编辑器 workspace 误入源码）" }
            elseif ($lower -match "\.log$" -or $lower -match "\.log\.\d+$") { $forbidden += "$rel（日志文件误入源码）" }
            elseif ($lower -match "\.(db|sqlite|sqlite3|db-wal|db-shm)$") { $forbidden += "$rel（临时数据库误入源码）" }
            elseif ($sourceExts -notcontains $f.Extension -and $lower -match "(api[_-]?key|secret|\.pem|\.pfx|credential|id_rsa|id_ed25519)") { $forbidden += "$rel（疑似密钥/凭据文件）" }
        }
        Write-Host ("    [forbidden-files] scanned={0} skipped-ignored={1} violations={2}" -f $allFiles.Count, $skipped, $forbidden.Count)
        if ($forbidden.Count -gt 0) {
            Write-Host "    forbidden 文件：" -ForegroundColor Red
            foreach ($ff in $forbidden) {
                Write-Host ("      - {0}" -f $ff) -ForegroundColor Red
                Write-Output $ff
            }
            $global:LASTEXITCODE = 1
        } else {
            $global:LASTEXITCODE = 0
        }
    }
}

# 1) fmt
if (-not $SkipFmt) {
    Invoke-CiStep -Name "cargo fmt --all -- --check" -Id "fmt" -Cwd $root -LogDir $LogDir -Block {
        cargo fmt --all -- --check
    }
}

# 2) clippy（workspace 全量；--locked 保证与 Cargo.lock 一致）
if (-not $SkipClippy) {
    Invoke-CiStep -Name "cargo clippy --workspace --all-targets --locked -- -D warnings" -Id "clippy" -Cwd $root -LogDir $LogDir -Block {
        cargo clippy --workspace --all-targets --locked -- -D warnings
    }
}

# 3) test（默认 workspace 全量；-ServerOnly 时只测 owo-agent-server 单包）
if (-not $SkipTest) {
    $testName = if ($ServerOnly) { "cargo test -p owo-agent-server --locked" } else { "cargo test --workspace --locked" }
    $testId = if ($ServerOnly) { "test-server" } else { "test" }
    Invoke-CiStep -Name $testName -Id $testId -Cwd $root -LogDir $LogDir -Block {
        if ($ServerOnly) {
            cargo test -p owo-agent-server --locked
        } else {
            cargo test --workspace --locked
        }
    }
}

# 4) 路由契约（HTTP 契约面，AGENTS.md 要求同步 route_contract_tests）
if (-not $SkipRouteContract) {
    Invoke-CiStep -Name "cargo test -p owo-agent-server --test route_contract_tests --locked" -Id "route-contract" -Cwd $root -LogDir $LogDir -Block {
        cargo test -p owo-agent-server --test route_contract_tests --locked
    }
}

# 5) Node 语法检查（app.js + core/*.js + views/*.view.js + 全部 panel）
if (-not $SkipNode) {
    Invoke-CiStep -Name "node --check（app.js + core/*.js + views/*.view.js + panels/*.panel.js）" -Id "node" -Cwd $root -LogDir $LogDir -Block {
        $jsFiles = @(Join-Path $root "desktop\web\app.js")
        $coreJs = Get-ChildItem (Join-Path $root "desktop\web\core") -Filter *.js -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName }
        if ($coreJs) { $jsFiles += @($coreJs) }
        $views = Get-ChildItem (Join-Path $root "desktop\web\views") -Filter *.view.js -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName }
        if ($views) { $jsFiles += @($views) }
        $panels = Get-ChildItem (Join-Path $root "desktop\web\panels") -Filter *.panel.js -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName }
        if ($panels) { $jsFiles += @($panels) }
        Write-Host ("    [node] files={0}" -f $jsFiles.Count)
        $bad = $false
        foreach ($f in $jsFiles) {
            node --check $f
            if ($global:LASTEXITCODE -ne 0) { $bad = $true }
        }
        if ($bad) { $global:LASTEXITCODE = 1 } else { $global:LASTEXITCODE = 0 }
    }
}

# 6) TS 类型检查（clients/ts；package-lock.json 锁定依赖）
if (-not $SkipTs) {
    Invoke-CiStep -Name "TS 类型检查（clients\ts npm run typecheck）" -Id "ts" -Cwd $root -LogDir $LogDir -Block {
        Push-Location (Join-Path $root "clients\ts")
        try {
            if (-not (Test-Path (Join-Path $root "clients\ts\node_modules\typescript"))) {
                npm ci --ignore-scripts
                if ($global:LASTEXITCODE -ne 0) { return }
            }
            npm run typecheck
        } finally {
            Pop-Location
        }
    }
}

# 7) 桌面壳（src-tauri 独立 workspace）：stage → fmt → clippy → test
#    R3（§8.1/§8.2）补位：桌面壳此前不在任何门禁内——R2 把 OWO_API_VERSION 改为
#    re-export 后，壳的 build.rs 文本抓取静默失效（壳根本编译不过）却无人报警。
#    冷启动/错误恢复的真实验收必须有壳侧编译与单测护栏。
if (-not $SkipDesktop) {
    $shellDir = Join-Path $root "desktop\tauri\src-tauri"
    $shellPresent = Test-Path (Join-Path $shellDir "Cargo.toml")
    if (-not $shellPresent) {
        Write-Host "    [desktop] 无 src-tauri Cargo.toml，跳过壳侧四步"
    }
    Invoke-CiStep -Name "桌面壳随包 core 预置（externalBin 命名+身份+SHA-256）" -Id "desktop-stage" -Cwd $root -LogDir $LogDir -Block {
        if (-not $shellPresent) { $global:LASTEXITCODE = 0; return }
        # Tauri 硬要求 binaries/owo-agent-<triple>.exe 存在。历史上开发者用手工复制
        # 旧 core 救火，制造了安装包错包与开发期同目录劫持（build-installer 还曾用错
        # 文件名，tauri-build 根本不读）。门禁内自动预置 = 每次都校验该产物来自当前
        # 源码、命名正确、哈希一致，等价于把那条门禁视野外的隐藏路径纳入门禁。
        . (Join-Path $PSScriptRoot "stage-desktop-sidecar.ps1")
        try {
            $null = Stage-OwoDesktopSidecar -Configuration debug
            $global:LASTEXITCODE = 0
        } catch {
            Write-Host "    [desktop-stage] $($_.Exception.Message)"
            $global:LASTEXITCODE = 1
        }
    }
    Invoke-CiStep -Name "桌面壳 cargo fmt --check（src-tauri）" -Id "desktop-fmt" -Cwd $shellDir -LogDir $LogDir -Block {
        if (-not $shellPresent) { $global:LASTEXITCODE = 0; return }
        cargo fmt --check
    }
    Invoke-CiStep -Name "桌面壳 cargo clippy --all-targets --locked -- -D warnings（src-tauri）" -Id "desktop-clippy" -Cwd $shellDir -LogDir $LogDir -Block {
        if (-not $shellPresent) { $global:LASTEXITCODE = 0; return }
        cargo clippy --all-targets --locked -- -D warnings
    }
    Invoke-CiStep -Name "桌面壳 cargo test --locked（src-tauri）" -Id "desktop-test" -Cwd $shellDir -LogDir $LogDir -Block {
        if (-not $shellPresent) { $global:LASTEXITCODE = 0; return }
        cargo test --locked
    }
}

Write-CiSummary -LogDir $LogDir