<#
verify-desktop-r4-ui.ps1 — R4（§4.3 全局状态条 / §4.6 诊断台账 / §4.10 真机清单）验收。

事实源全部是**真机**：真实 Tauri 壳 + 真实 WebView2 + 真实 sidecar，DOM 结论只从
WebView2 CDP 取，请求计数只从服务端 /diagnostics/requests ledger 取（§8.1 口径）。

三组断言：
  ① §4.3：五段（后台/工作区/模型/权限/当前任务）真的存在、真是 button、真的可聚焦、
     点击真的换页，且工作区段只给「名称 + 路径摘要」不回显绝对路径。
  ② §4.6：设置页台账八项条目齐备；四类计数自洽（health+auth+events+business = returned，
     与服务端窗口口径一致）；导出脱敏包结构齐备；**页面文本与导出包双双**不得出现
     本地绝对路径/授权头/原始 query。
  ③ §8.2 回归护栏：状态条与台账不改首屏口径（业务请求 ≤5、无同路由重复）。
  ④ §4.10：三档窗口（900×600 / 1280×720 / 1920×1080）状态条不溢出 + 截图像素验真。

用法（在 agent-sdk/ 目录）：
  .\scripts\verify-desktop-r4-ui.ps1                    # 全量
  .\scripts\verify-desktop-r4-ui.ps1 -SkipScreenshots   # 只跑 DOM 断言
退出码：0 = 全部断言通过；1 = 存在失败断言或异常（取证文件仍会写出）。
#>
[CmdletBinding()]
param(
    [switch]$SkipScreenshots,
    [string]$EvidenceDir = "",
    [string]$Stamp = ""
)

$ErrorActionPreference = 'Stop'
$sdkRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot "desktop-acceptance-common.ps1")
if (-not $Stamp) { $Stamp = Get-Date -Format 'yyyyMMdd-HHmmss' }
if (-not $EvidenceDir) {
    $EvidenceDir = Join-Path $sdkRoot "docs\qa\evidence\r4-desktop-ui-$Stamp"
}
New-Item -ItemType Directory -Force -Path $EvidenceDir | Out-Null

$results = New-Object System.Collections.ArrayList
function Add-Check {
    param([string]$Name, [bool]$Pass, [string]$Detail)
    $null = $results.Add([pscustomobject]@{ name = $Name; pass = $Pass; detail = $Detail })
    $mark = if ($Pass) { 'PASS' } else { 'FAIL' }
    Write-Host ("[{0}] {1} — {2}" -f $mark, $Name, $Detail)
}
function Get-CdpJson {
    # CDP 求值统一「返回 JSON 文本再反序列化」：拿不到就是 null，绝不猜默认值。
    param([int]$Port, [string]$Expression, [int]$TimeoutSec = 8)
    $raw = Invoke-OwoCdpEval -Port $Port -Expression $Expression -TimeoutSec $TimeoutSec
    if (-not $raw) { return $null }
    try { return ($raw | ConvertFrom-Json) } catch { return $null }
}
function Test-NoForbiddenEcho {
    <# §4.6 禁止回显清单：本地绝对路径 / 授权头 / 配对秘密 / 原始 URL query。 #>
    param([string]$Text)
    $hits = @()
    if ($null -eq $Text) { return @('（无文本）') }
    if ($Text -match '[A-Za-z]:\\') { $hits += '盘符绝对路径' }
    if ($Text -match '\\\\[^\s\\]+\\') { $hits += 'UNC 路径' }
    if ($Text -match '/(?:Users|home)/[^\s"]+') { $hits += 'POSIX 家目录路径' }
    if ($Text -match '(?i)bearer\s+[A-Za-z0-9._\-]{6,}') { $hits += '授权头' }
    if ($Text -match '\?[^\s"]+=') { $hits += '原始 URL query' }
    return $hits
}

# ---- DOM 事实表达式（一次取全，避免「多取几次总有一次对上」）----
$exprStatusBar = @'
(function () {
  var host = document.getElementById("globalStatusBar");
  if (!host) return JSON.stringify({ present: false, count: 0, segments: [] });
  var segs = [];
  var nodes = host.querySelectorAll("[data-owo-status]");
  for (var i = 0; i < nodes.length; i++) {
    var el = nodes[i];
    var r = el.getBoundingClientRect();
    var v = el.querySelector(".owo-status-value");
    segs.push({
      key: el.getAttribute("data-owo-status"),
      tag: el.tagName,
      value: v ? (v.textContent || "").trim() : "",
      tone: el.getAttribute("data-tone") || "",
      target: el.getAttribute("data-target") || "",
      title: el.getAttribute("title") || "",
      aria: el.getAttribute("aria-label") || "",
      width: Math.round(r.width),
      height: Math.round(r.height),
      focusable: (el.tabIndex >= 0 && !el.disabled)
    });
  }
  var barRect = host.getBoundingClientRect();
  return JSON.stringify({
    present: true,
    count: nodes.length,
    segments: segs,
    barRight: Math.round(barRect.right),
    barHeight: Math.round(barRect.height),
    innerWidth: window.innerWidth,
    bodyClass: document.body.className
  });
})()
'@

$exprRouteState = @'
(function () {
  var active = document.querySelector("[data-rail-target].active");
  var header = document.getElementById("routeHeader");
  return JSON.stringify({
    rail: active ? active.getAttribute("data-rail-target") : "",
    body: document.body.className,
    header: header ? (header.innerText || "").replace(/\s+/g, " ").trim().slice(0, 80) : "",
    hash: location.hash
  });
})()
'@

$exprFocusPermission = @'
(function () {
  var el = document.querySelector('#globalStatusBar [data-owo-status="permission"]');
  if (!el) return JSON.stringify({ found: false });
  el.focus();
  return JSON.stringify({
    found: true,
    active: document.activeElement === el,
    tag: document.activeElement.tagName,
    key: document.activeElement.getAttribute("data-owo-status")
  });
})()
'@

$exprLedgerPage = @'
(function () {
  var root = document.getElementById("diagnosticsLedger");
  if (!root) return JSON.stringify({ present: false });
  var text = (root.innerText || "").replace(/\s+/g, " ");
  var cards = {};
  var nodes = root.querySelectorAll(".owo-ledger-card");
  for (var i = 0; i < nodes.length; i++) {
    var s = nodes[i].querySelector("span");
    var b = nodes[i].querySelector("b");
    if (s) cards[s.textContent.trim()] = b ? b.textContent.trim() : "";
  }
  function num(key) {
    var raw = cards[key];
    if (raw == null) return null;
    var n = parseInt(String(raw).replace(/[^0-9]/g, ""), 10);
    return isNaN(n) ? null : n;
  }
  var rows = root.querySelectorAll(".owo-ledger-table tbody tr");
  var bundle = null;
  if (window.OwoDiagnosticsLedger && root.__owoLedgerState) {
    try {
      bundle = JSON.stringify(window.OwoDiagnosticsLedger.buildExportBundle(root.__owoLedgerState.data || {}));
    } catch (e) {
      bundle = "__throw:" + e.message;
    }
  }
  var head = text.match(/最近请求数量：\s*([0-9]+)\s*条（窗口累计\s*([0-9]+)，环形容量\s*([0-9]+)）/);
  var exportBtn = document.getElementById("owoLedgerExport");
  var refreshBtn = document.getElementById("owoLedgerRefresh");
  var updated = document.getElementById("owoLedgerUpdated");
  return JSON.stringify({
    present: true,
    textLen: text.length,
    text: text.slice(0, 1200),
    health: num("health 探测"),
    auth: num("auth 引导"),
    events: num("events（SSE）"),
    business: num("业务请求"),
    returned: head ? parseInt(head[1], 10) : null,
    total: head ? parseInt(head[2], 10) : null,
    cap: head ? parseInt(head[3], 10) : null,
    hasSlow: /慢请求 Top/.test(text),
    hasPercentile: /按路由模板聚合/.test(text),
    hasSources: /来源/.test(text),
    hasRestart: /core 重启与引导/.test(text),
    hasSse: /SSE 事件流/.test(text),
    hasExportButton: !!exportBtn,
    exportLabel: exportBtn ? (exportBtn.textContent || "").trim() : "",
    hasRefreshButton: !!refreshBtn,
    rowCount: rows.length,
    bundle: bundle,
    note: updated ? (updated.textContent || "").trim() : ""
  });
})()
'@

$exprExportState = @'
(function () {
  var updated = document.getElementById("owoLedgerUpdated");
  var pre = document.getElementById("owoLedgerBundle");
  return JSON.stringify({
    note: updated ? (updated.textContent || "").trim() : "",
    bundleVisible: !!(pre && !pre.hidden),
    bundleLen: pre ? (pre.textContent || "").length : 0
  });
})()
'@

$clickGenerate = @'
(function () {
  var el = document.getElementById("owoLedgerExport");
  if (!el) return "missing";
  el.click();
  return "clicked";
})()
'@

$clickWorkspace = @'
(function () {
  var el = document.querySelector('#globalStatusBar [data-owo-status="workspace"]');
  if (!el) return "missing";
  el.click();
  return "clicked";
})()
'@

$clickSettings = @'
(function () {
  var el = document.querySelector('[data-rail-target="settings"]');
  if (!el) return "missing";
  el.click();
  return "clicked";
})()
'@

$clickRefresh = @'
(function () {
  var el = document.getElementById("owoLedgerRefresh");
  if (!el) return "missing";
  el.click();
  return "clicked";
})()
'@

# ---- 启动：真实壳（私有环境 + 验收模式 + 内嵌前端新鲜度门，全走共享原语）----
$run = New-OwoAcceptanceRun -SdkRoot $sdkRoot -Scenario 'r4-ui' -Stamp $Stamp -PrepareWorkspace
Write-Host "[r4-ui] 私有运行根=$($run.run_root)"
Write-Host "[r4-ui] 随包 core=$($run.launched_sha256) 身份=$($run.identity)"
if (-not $SkipScreenshots) { Add-OwoDesktopWin32 }

$shellProc = $null
$exitCode = 1
try {
    $shellProc = Start-OwoAcceptanceShell -Run $run
    Write-Host "[r4-ui] 桌面壳 pid=$($shellProc.Id)"

    $ready = Wait-OwoCoreReady -LogDir $run.log_dir -TimeoutSec 45 -ExcludePid 0
    if (-not $ready) {
        Add-Check '壳在 45s 内拉起 sidecar 并报 core_ready' $false '未读到 core_ready 行'
        throw 'sidecar 未就绪，终止后续断言'
    }
    Add-Check '壳在 45s 内拉起 sidecar 并报 core_ready' $true "pid=$($ready.pid) port=$($ready.port)"
    $base = "http://127.0.0.1:$($ready.port)"

    $cdpPort = Resolve-OwoCdpPort -DataDir $run.webview_data_dir -RequestedPort ([int]$run.cdp_requested) -TimeoutSec 25
    Add-Check 'DOM 事实通道（WebView2 CDP）建立' ($cdpPort -gt 0) "requested=$($run.cdp_requested) effective=$cdpPort"
    if ($cdpPort -le 0) {
        $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'cdp-diagnostic.json' `
            -InputObject (Get-OwoWebviewDiagnostics -RequestedPort ([int]$run.cdp_requested) -DataDir $run.webview_data_dir)
        throw 'CDP 不可达，UI 断言无法取证'
    }

    $ui = $null
    $uiDeadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $uiDeadline) {
        $ui = Get-OwoVisibleUiText -Port $cdpPort -TimeoutSec 5
        if ($ui -and $ui.composerVisible) { break }
        Start-Sleep -Milliseconds 500
    }
    Add-Check '首屏 20s 内出现可输入任务框（§4.10 基线）' ([bool]($ui -and $ui.composerVisible)) `
        "composerVisible=$($ui.composerVisible) health=`"$($ui.healthText)`""
    $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'ui-first-screen.json' -InputObject $ui

    $token = ""
    if (Test-Path -LiteralPath $run.token_file) { $token = (Get-Content $run.token_file -Raw).Trim() }
    $boot = Get-OwoLedgerFacts -Base $base -Token $token -Label 'r4-ui-boot'
    Add-Check '状态条上线不改首屏口径：首次可输入前业务请求 ≤5' `
        (($null -ne $boot.web_business) -and ($boot.web_business -le 5)) `
        "web_business=$($boot.web_business) routes=$($boot.web_business_routes -join ',')"
    Add-Check '首屏无同路由重复请求（状态条 1s 定时器不得发 HTTP）' ($boot.web_route_repeats.Count -eq 0) `
        "repeats=$($boot.web_route_repeats -join ',')"
    $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'ledger-boot.json' -InputObject $boot

    # ================= ① §4.3 全局状态条 =================
    $bar = $null
    $barDeadline = (Get-Date).AddSeconds(10)
    while ((Get-Date) -lt $barDeadline) {
        $bar = Get-CdpJson -Port $cdpPort -Expression $exprStatusBar
        if ($bar -and $bar.present -and $bar.count -eq 5) { break }
        Start-Sleep -Milliseconds 400
    }
    $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'status-bar.json' -InputObject $bar
    Add-Check '状态条容器存在且五段齐全（§4.3）' (($null -ne $bar) -and [bool]$bar.present -and $bar.count -eq 5) `
        "count=$($bar.count) present=$($bar.present) bar_height=$($bar.barHeight)"
    $segKeys = @()
    $segTags = @()
    $segFocus = @()
    $segAria = @()
    $emptyValues = @()
    $byKey = @{}
    foreach ($seg in $bar.segments) {
        $segKeys += [string]$seg.key
        $segTags += [string]$seg.tag
        $segFocus += [string]$seg.focusable
        $segAria += [string]$seg.aria.Length
        $byKey[[string]$seg.key] = $seg
        if ([string]::IsNullOrWhiteSpace($seg.value) -or $seg.value -eq '—') { $emptyValues += [string]$seg.key }
    }
    Add-Check '五段 key 与 §4.3 列举一致（顺序即呈现顺序）' `
        (($segKeys -join ',') -eq 'backend,workspace,model,permission,task') "keys=$($segKeys -join ',')"
    $tagsOk = (($segTags -join ',') -eq 'BUTTON,BUTTON,BUTTON,BUTTON,BUTTON')
    $focusOk = (($segFocus -join ',') -eq 'True,True,True,True,True')
    Add-Check '每段都是真 button 且键盘可聚焦（不是不可交互的文字灯）' ($tagsOk -and $focusOk) `
        "tags=$($segTags -join ',') tabbable=$($segFocus -join ',')"
    Add-Check '每段都有非空事实值（无占位破洞）' ($emptyValues.Count -eq 0) "empty=$($emptyValues -join ',')"
    Add-Check '每段都带中文用途提示（aria-label 非空）' `
        ((@($segAria | Where-Object { [int]$_ -gt 6 }).Count) -eq 5) "aria_lengths=$($segAria -join ',')"
    Add-Check '后台段反映核心就绪（ready → 可用）' ($byKey['backend'].value -match '可用') `
        "backend=`"$($byKey['backend'].value)`" tone=$($byKey['backend'].tone)"
    Add-Check '工作区段显示已选项目名（预置工作区生效）' `
        ($byKey['workspace'].value -match 'project') "workspace=`"$($byKey['workspace'].value)`""
    $wsText = [string]$byKey['workspace'].value + ' ' + [string]$byKey['workspace'].title
    $wsLeaks = @(Test-NoForbiddenEcho -Text $wsText)
    Add-Check '工作区段只给名称+路径摘要，不回显绝对路径' ($wsLeaks.Count -eq 0) "hits=$($wsLeaks -join ',')"

    $null = Invoke-OwoCdpEval -Port $cdpPort -Expression $clickWorkspace -TimeoutSec 8
    Start-Sleep -Milliseconds 1200
    $afterWs = Get-CdpJson -Port $cdpPort -Expression $exprRouteState
    Add-Check '点击工作区段真的换页（活动导航切到 projects）' `
        (($null -ne $afterWs) -and $afterWs.rail -eq 'projects') `
        "rail=$($afterWs.rail) header=`"$($afterWs.header)`""
    $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'status-bar-click-workspace.json' -InputObject $afterWs

    $focus = Get-CdpJson -Port $cdpPort -Expression $exprFocusPermission
    Add-Check '状态条段可用键盘聚焦（focus 后 activeElement 即该段）' `
        (($null -ne $focus) -and [bool]$focus.active -and $focus.key -eq 'permission') `
        "active=$($focus.active) key=$($focus.key) tag=$($focus.tag)"

    # ================= ② §4.6 诊断请求台账 =================
    $null = Invoke-OwoCdpEval -Port $cdpPort -Expression $clickSettings -TimeoutSec 8
    $ledger = $null
    $ledgerDeadline = (Get-Date).AddSeconds(16)
    while ((Get-Date) -lt $ledgerDeadline) {
        $ledger = Get-CdpJson -Port $cdpPort -Expression $exprLedgerPage -TimeoutSec 12
        # 「已加载」的判据必须是服务端事实：cap=512 只在真实响应里存在，骨架态没有。
        if ($ledger -and $ledger.present -and [int]$ledger.cap -eq 512) { break }
        Start-Sleep -Milliseconds 600
    }
    $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'ledger-page.json' -InputObject $ledger
    Add-Check '设置页诊断台账已加载（不是静态占位/空白）' `
        (($null -ne $ledger) -and [bool]$ledger.present -and [int]$ledger.cap -eq 512) `
        "returned=$($ledger.returned) cap=$($ledger.cap) note=`"$($ledger.note)`""
    $sectionsOk = [bool]($ledger.hasSlow -and $ledger.hasPercentile -and $ledger.hasSources -and `
        $ledger.hasRestart -and $ledger.hasSse -and $ledger.hasExportButton -and $ledger.hasRefreshButton)
    Add-Check '§4.6 八项条目齐备（数量/四类/慢请求/P50-P95/来源/重启/SSE/导出）' $sectionsOk `
        "slow=$($ledger.hasSlow) p50p95=$($ledger.hasPercentile) src=$($ledger.hasSources) restart=$($ledger.hasRestart) sse=$($ledger.hasSse) export=$($ledger.hasExportButton) refresh=$($ledger.hasRefreshButton)"
    $bucketSum = [int]$ledger.health + [int]$ledger.auth + [int]$ledger.events + [int]$ledger.business
    Add-Check '四类计数自洽：health+auth+events+business = 窗口返回条数' `
        ($bucketSum -eq [int]$ledger.returned) `
        "health=$($ledger.health) auth=$($ledger.auth) events=$($ledger.events) business=$($ledger.business) returned=$($ledger.returned)"
    Add-Check '台账窗口口径与服务端一致（returned ≤ total，cap=512）' `
        (([int]$ledger.returned -le [int]$ledger.total) -and ([int]$ledger.cap -eq 512)) `
        "returned=$($ledger.returned) total=$($ledger.total) cap=$($ledger.cap)"
    Add-Check '台账真的聚合出数据行（慢请求/聚合表非空）' ([int]$ledger.rowCount -ge 3) "rows=$($ledger.rowCount)"
    $pageLeaks = @(Test-NoForbiddenEcho -Text ([string]$ledger.text))
    Add-Check '台账页面文本不回显绝对路径/授权头/原始 query' ($pageLeaks.Count -eq 0) "hits=$($pageLeaks -join ',')"

    $bundleText = [string]$ledger.bundle
    $bundleKeys = @('"schema"', '"buckets"', '"slowest"', '"routes"', '"sources"', '"sse"', '"redaction"', '"bootstraps"', '"server_active_connections"')
    $missingKeys = @()
    foreach ($key in $bundleKeys) { if (-not $bundleText.Contains($key)) { $missingKeys += $key } }
    Add-Check '一键导出的脱敏诊断包结构齐备（含重启近似与 SSE 计数）' ($missingKeys.Count -eq 0) `
        "missing=$($missingKeys -join ',') head=$($bundleText.Substring(0, [Math]::Min(60, $bundleText.Length)))"
    $bundleLeaks = @(Test-NoForbiddenEcho -Text $bundleText)
    Add-Check '导出包同样不回显禁止项（路径/授权头/query/instance 全文）' ($bundleLeaks.Count -eq 0) `
        "hits=$($bundleLeaks -join ',')"

    $null = Invoke-OwoCdpEval -Port $cdpPort -Expression $clickGenerate -TimeoutSec 12
    Start-Sleep -Milliseconds 1200
    $afterExport = Get-CdpJson -Port $cdpPort -Expression $exprExportState
    Add-Check '「生成脱敏诊断包」一键出包并在页面展开（按钮不是装饰）' `
        (($null -ne $afterExport) -and ($afterExport.note -match '已生成脱敏诊断包') -and [bool]$afterExport.bundleVisible -and [int]$afterExport.bundleLen -gt 200) `
        "note=`"$($afterExport.note)`" visible=$($afterExport.bundleVisible) len=$($afterExport.bundleLen)"
    $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'ledger-export.json' -InputObject $afterExport

    $beforeRefresh = [int]$ledger.returned
    $null = Invoke-OwoCdpEval -Port $cdpPort -Expression $clickRefresh -TimeoutSec 12
    Start-Sleep -Milliseconds 1800
    $refreshed = Get-CdpJson -Port $cdpPort -Expression $exprLedgerPage -TimeoutSec 12
    Add-Check '「刷新台账」可重复取数（returned 不小于上一次）' `
        (($null -ne $refreshed) -and ([int]$refreshed.returned -ge $beforeRefresh)) `
        "before=$beforeRefresh after=$($refreshed.returned)"

    # ================= ③ §4.10 三档窗口 =================
    if (-not $SkipScreenshots) {
        foreach ($size in @(@(900, 600), @(1280, 720), @(1920, 1080))) {
            $targetW = $size[0]; $targetH = $size[1]
            $winShot = Get-OwoValidatedWindow -ProcessId $shellProc.Id -TimeoutSec 8 -RequireVisible `
                -MinWidth ([Math]::Min(800, $targetW)) -MinHeight ([Math]::Min(520, $targetH))
            if (-not $winShot.ok) {
                Add-Check "窗口 ${targetW}x${targetH}：先验窗口身份" $false "fail=$($winShot.fail_reasons -join ';')"
                continue
            }
            $null = Set-OwoWindowGeometry -Hwnd $winShot.hwnd -Width $targetW -Height $targetH
            Start-Sleep -Milliseconds 1200
            $barFit = Get-CdpJson -Port $cdpPort -Expression $exprStatusBar -TimeoutSec 10
            $fits = ($null -ne $barFit) -and ([int]$barFit.barRight -le ([int]$barFit.innerWidth + 2))
            Add-Check "状态条在 ${targetW}x${targetH} 下不横向溢出且仍五段" `
                ($fits -and [int]$barFit.count -eq 5) `
                "bar_right=$($barFit.barRight) inner_width=$($barFit.innerWidth) count=$($barFit.count)"
            $shot = Join-Path $EvidenceDir ("r4-ui-${targetW}x${targetH}.png")
            try {
                $winShot2 = Get-OwoValidatedWindow -ProcessId $shellProc.Id -TimeoutSec 8 -RequireVisible
                if (-not $winShot2.ok) { throw "截图前窗口验证失败：$($winShot2.fail_reasons -join ';')" }
                $null = Save-OwoWindowShot -Hwnd $winShot2.hwnd -Path $shot
                $metric = Test-OwoScreenshot -Path $shot -ExpectedWidth $winShot2.width -ExpectedHeight $winShot2.height
                Add-Check "截图 ${targetW}x${targetH}（可解码/尺寸/字节/像素比例/方差全检）" ([bool]$metric.ok) `
                    "$shot（$($winShot2.width)x$($winShot2.height)） $(Get-OwoScreenshotMetricLine $metric)"
            } catch {
                Add-Check "截图 ${targetW}x${targetH}（可解码/尺寸/字节/像素比例/方差全检）" $false $_.Exception.Message
            }
        }
    }

    $fail = @($results | Where-Object { -not $_.pass })
    $report = [pscustomobject]@{
        scenario        = 'r4-desktop-ui'
        stamp           = $Stamp
        identity        = $run.identity
        checks          = $results.Count
        passed          = ($results.Count - $fail.Count)
        failed          = $fail.Count
        boot_business   = $boot.web_business
        ledger_returned = $ledger.returned
        ok              = ($fail.Count -eq 0)
        run_root        = $run.run_root
        items           = $results
    }
    $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'r4-ui-report.json' -InputObject $report
    Write-Host ("[r4-ui] 断言 {0}/{1} 通过（证据：{2}）" -f ($results.Count - $fail.Count), $results.Count, $EvidenceDir)
    $exitCode = if ($fail.Count -eq 0) { 0 } else { 1 }
}
catch {
    Write-Host "[r4-ui] 异常终止：$($_.Exception.Message)" -ForegroundColor Red
    $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'r4-ui-aborted.json' -InputObject ([pscustomobject]@{
            error    = $_.Exception.Message
            checks   = $results
            run_root = $run.run_root
        })
    $exitCode = 1
}
finally {
    if ($shellProc) {
        Stop-OwoShellTree -ProcessId $shellProc.Id -RunRoot $run.run_root
        Write-Host "[r4-ui] 壳进程树已回收（pid=$($shellProc.Id)）"
    }
}
exit $exitCode
