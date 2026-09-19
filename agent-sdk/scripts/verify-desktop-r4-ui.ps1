<#
verify-desktop-r4-ui.ps1 — R4（§4.3 全局状态条 / §4.6 诊断台账 / §4.10 真机清单）验收。

事实源全部是**真机**：真实 Tauri 壳 + 真实 WebView2 + 真实 sidecar，DOM 结论只从
WebView2 CDP 取，请求计数只从服务端 /diagnostics/requests ledger 取（§8.1 口径）。

各组断言（条数以 summary.json 为准，此处只列口径；注释里写死条数必然过期）：
  ① §4.3：五段（后台/工作区/模型/权限/当前任务）真的存在、真是 button、真的可聚焦、
     点击真的换页，且工作区段只给「名称 + 路径摘要」不回显绝对路径。
  ②.5 §4.5：权限中心——服务端事实（四维矩阵 / 摘要来源 / 风险清单 / 展开规则 / 落盘）
     与 DOM 渲染**分开断言**，完全访问二次确认走真路径（默认草稿→给解释，改成不受限→
     开卡），整页请求数由台账差值锁死（禁轮询）。
  ② §4.4：工作区字段在壳内转只读 + 原生目录选择器入口就绪（指向 choose_project_directory），
     文案不再引导手输/粘贴绝对路径。**只验接线，不点原生对话框**（Win32 模态框不在
     本窗口客户区内，CDP/截图都覆盖不到——去点它只会把自动化卡死）。
     §4.6「最近一次 core 重启」必须取壳侧 generation（Tauri IPC → api-client → 台账），
     不得退化成 /auth/token 引导次数近似。
  ③ §4.6：设置页台账八项条目齐备；四类计数自洽（health+auth+events+business = returned，
     与服务端窗口口径一致）；导出脱敏包结构齐备；**页面文本与导出包双双**不得出现
     本地绝对路径/授权头/原始 query。
  ④ §8.2 回归护栏：状态条与台账不改首屏口径（业务请求 ≤5、无同路由重复）。
  ⑤ §4.10：三档窗口（900×600 / 1280×720 / 1920×1080）状态条不溢出 + 截图像素验真。

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
  function kvValue(label) {
    var list = root.querySelectorAll(".owo-ledger-kv");
    for (var i = 0; i < list.length; i++) {
      var s = list[i].querySelector("span");
      if (s && s.textContent.trim() === label) {
        var v = list[i].querySelector("strong");
        return v ? v.textContent.trim() : null;
      }
    }
    return null;
  }
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
    restartFact: kvValue("最近一次 core 重启"),
    bootCount: kvValue("窗口内引导次数"),
    hasAuthoritativeCaveat: /取自壳侧权威快照/.test(text),
    hasApproximationCaveat: /以 \/auth\/token 引导次数近似/.test(text),
    authCardHintNative: /壳注入 token 时为 0/.test(text),
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

# §4.4 表单控件规范：文件夹必须走原生目录选择器，禁止要求手输完整路径。
# 这里**只验接线，绝不点击按钮**：rfd 弹出的是 Win32 模态框，不属于本窗口
# 客户区，CDP 与截图都覆盖不到；验收去点它只会把自己卡到超时（那是人工
# 交互项，见执行记录 §8）。判定的是"壳内确实把输入框转成了只读 + 原生入口"。
$exprFolderPicker = @'
(function () {
  var input = document.getElementById("workspace");
  var button = document.getElementById("chooseWorkspace");
  var hint = document.getElementById("workspacePickerHint");
  if (!input || !button) return JSON.stringify({ present: false });
  var picker = window.OwoFolderPicker || null;
  return JSON.stringify({
    present: true,
    readOnly: input.readOnly === true,
    ariaReadonly: input.getAttribute("aria-readonly"),
    inputMode: input.dataset ? (input.dataset.owoFolderPicker || "") : "",
    buttonMode: button.dataset ? (button.dataset.owoFolderPicker || "") : "",
    buttonLabel: (button.textContent || "").trim(),
    buttonDisabled: button.disabled === true,
    buttonFocusable: button.tabIndex !== -1,
    hint: hint ? (hint.textContent || "").trim() : "",
    moduleReady: !!(picker && typeof picker.pick === "function"),
    nativeAvailable: !!(picker && picker.isNativeAvailable(window)),
    command: picker ? String(picker.PICK_COMMAND || "") : ""
  });
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

# ---- §4.5 权限中心：DOM 事实表达式（真机上这一页到底长什么样、发了几次请求）----
$exprPermissionsPage = @'
(function () {
  var table = document.querySelector(".owo-perm-table");
  var rows = table ? table.querySelectorAll("tbody tr") : [];
  var dims = [];
  for (var i = 0; i < rows.length; i++) {
    var head = rows[i].querySelector("th");
    var cells = rows[i].querySelectorAll("td");
    if (cells.length < 3) continue;
    dims.push({
      key: (head && head.textContent || "").trim(),
      effective: (cells[0].textContent || "").trim(),
      source: (cells[1].textContent || "").trim(),
      summary: (cells[2].textContent || "").trim()
    });
  }
  function kvValue(label) {
    var list = document.querySelectorAll(".owo-perm-kv");
    for (var i = 0; i < list.length; i++) {
      var s = list[i].querySelector("span");
      if (s && (s.textContent || "").trim() === label) {
        var strong = list[i].querySelector("strong");
        if (strong) return (strong.textContent || "").trim();
        var code = list[i].querySelector("code");
        if (code) return (code.textContent || "").trim();
        var rest = (list[i].textContent || "").replace((s.textContent || ""), "").trim();
        return rest;
      }
    }
    return null;
  }
  var seen = {};
  var approvalNodes = document.querySelectorAll('[data-perm-action="approval"]');
  for (var j = 0; j < approvalNodes.length; j++) {
    seen[(approvalNodes[j].getAttribute("data-approval") || "")] = 1;
  }
  var confirmCard = document.querySelector("[data-perm-confirm]");
  var bodyText = document.body.innerText || "";
  return JSON.stringify({
    present: !!table,
    dims: dims,
    dimCount: dims.length,
    profile: kvValue("当前档位"),
    pendingEmpty: !!document.querySelector('[data-perm-empty="pending"]'),
    grantsEmpty: !!document.querySelector('[data-perm-empty="grants"]'),
    decisionCount: approvalNodes.length,
    approvalScopes: Object.keys(seen).sort().join(","),
    hasRequestFullAccess: !!document.querySelector('[data-perm-action="request-full-access"]'),
    confirmOpen: !!confirmCard,
    confirmText: confirmCard ? (confirmCard.innerText || "").replace(/\s+/g, " ").slice(0, 400) : "",
    // 风险清单在页面上常驻（§4.5.2 预览）；只有确认卡内的才算三要素，必须按卡内作用域数。
    riskItems: confirmCard ? confirmCard.querySelectorAll(".owo-perm-risk li").length : 0,
    pageRiskItems: document.querySelectorAll(".owo-perm-risk li").length,
    durationOptions: confirmCard ? confirmCard.querySelectorAll('[data-perm-durations="1"] input').length : 0,
    confirmHasScope: confirmCard ? /"filesystem"/.test(confirmCard.innerText || "") : false,
    nonConfigurable: document.querySelectorAll("[data-perm-nonconfigurable]").length,
    emptyStateMarks: document.querySelectorAll("[data-perm-empty]").length,
    textLen: bodyText.length,
    text: bodyText.slice(0, 1600)
  });
})()
'@

$clickPermissionsRail = @'
(function () {
  var btn = document.querySelector('[data-rail-target="permissions"]');
  if (!btn) return "no-rail-button";
  btn.click();
  return "clicked";
})()
'@

$clickPermAction = @'
(function () {
  var btn = document.querySelector('[data-perm-action="__PERM_ACTION__"]');
  if (!btn) return "missing";
  btn.click();
  return "clicked";
})()
'@

# 把某一维改成指定值并触发 change：完全访问确认卡只有在草稿真的含
# 不受限维度时才会出现，直接用默认草稿点按钮等于点一个无效路径。
$setPermDimension = @'
(function () {
  var sel = document.querySelector('select[name="perm-dimension-__PERM_DIM__"]');
  if (!sel) return "missing-select";
  sel.value = "__PERM_VALUE__";
  sel.dispatchEvent(new Event("change", { bubbles: true }));
  return sel.value;
})()
'@

# 状态条权限段必须真的落在权限中心（§4.5 落地前它被临时降级到设置页）。
$clickStatusBarPermission = @'
(function () {
  var seg = document.querySelector('[data-owo-status="permission"]');
  if (!seg) return "no-segment";
  seg.click();
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
    # 不能"五段一出现就取样"：后台段的 ready→可用 折叠比骨架渲染晚（实测抓到
    # 第一帧 backend="检查中" 就把瞬时态当成结论）。这里等到后台段落定（或超时），
    # 拿落定帧做断言；真坏的情况仍会在窗口内失败（不是把断言改成永不失败）。
    $bar = $null
    $barReadyAt = $null
    $barDeadline = (Get-Date).AddSeconds(24)
    while ((Get-Date) -lt $barDeadline) {
        $bar = Get-CdpJson -Port $cdpPort -Expression $exprStatusBar
        if ($bar -and $bar.present -and ($bar.count -eq 5)) {
            $probeSeg = @($bar.segments | Where-Object { [string]$_.key -eq 'backend' })
            if (($probeSeg.Count -gt 0) -and ([string]$probeSeg[0].value -match '可用')) {
                $barReadyAt = Get-Date
                break
            }
        }
        Start-Sleep -Milliseconds 400
    }
    $barSettleNote = if ($null -ne $barReadyAt) { '已落定' } else { '未落定（保持超时前最后一帧）' }
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
        "backend=`"$($byKey['backend'].value)`" tone=$($byKey['backend'].tone) settle=$barSettleNote"
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

    # ================= ①.5 §4.4 文件夹原生选择器接线 =================
    $picker = Get-CdpJson -Port $cdpPort -Expression $exprFolderPicker -TimeoutSec 10
    $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'folder-picker.json' -InputObject $picker
    Add-Check '§4.4 工作区输入框在壳内转只读（禁止要求手输完整路径）' `
        (($null -ne $picker) -and [bool]$picker.present -and [bool]$picker.readOnly -and `
            ($picker.ariaReadonly -eq 'true') -and ($picker.inputMode -eq 'native')) `
        "readOnly=$($picker.readOnly) aria=$($picker.ariaReadonly) mode=$($picker.inputMode)"
    Add-Check '§4.4 原生选择器入口就绪并指向壳侧命令（不是浏览器降级桩）' `
        (([bool]$picker.moduleReady) -and [bool]$picker.nativeAvailable -and `
            (-not [bool]$picker.buttonDisabled) -and ($picker.buttonMode -eq 'native') -and `
            ($picker.command -eq 'choose_project_directory')) `
        "module=$($picker.moduleReady) native=$($picker.nativeAvailable) disabled=$($picker.buttonDisabled) command=$($picker.command) label=`"$($picker.buttonLabel)`""
    Add-Check '§4.4 工作区文案不再引导粘贴绝对路径（口径改为原生选择器）' `
        ($picker.hint -match '原生目录选择器') "hint=`"$($picker.hint)`""

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
    # §4.6「最近一次 core 重启」必须有权威口径：壳侧 generation 经 Tauri IPC →
    # api-client 归一 → 台账渲染。任何一环断了都会静默退化成"壳未上报 + 近似"，
    # 所以这里要求的是**不出现**近似文案。
    Add-Check '§4.6 重启口径取壳侧权威代际（不以 /auth/token 引导次数近似）' `
        (($ledger.restartFact -match '^第 [0-9]+ 代') -and [bool]$ledger.hasAuthoritativeCaveat -and `
            (-not [bool]$ledger.hasApproximationCaveat)) `
        "restart=`"$($ledger.restartFact)`" bootstraps=$($ledger.bootCount) auth_card=$($ledger.authCardHintNative)"
    $pageLeaks = @(Test-NoForbiddenEcho -Text ([string]$ledger.text))
    Add-Check '台账页面文本不回显绝对路径/授权头/原始 query' ($pageLeaks.Count -eq 0) "hits=$($pageLeaks -join ',')"

    $bundleText = [string]$ledger.bundle
    $bundleKeys = @('"schema"', '"buckets"', '"slowest"', '"routes"', '"sources"', '"sse"', '"redaction"', '"bootstraps"', '"server_active_connections"', '"shell_generation"', '"shell_attempt"')
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

    # 模型段两份真相核对（R4-BUG-05）：壳侧 get_provider_status 在密钥经 sidecar
    # 注入时会误报 unset；设置页从 core 水合回灌后，模型段必须改说权威事实、不得标黄。
    $barAfterSettings = Get-CdpJson -Port $cdpPort -Expression $exprStatusBar -TimeoutSec 10
    $modelSeg = $null
    $backendSeg = $null
    foreach ($seg in $barAfterSettings.segments) {
        if ($seg.key -eq 'model') { $modelSeg = $seg }
        if ($seg.key -eq 'backend') { $backendSeg = $seg }
    }
    Add-Check '模型段不与后台段自相矛盾（core 就绪时不说「未配置」）' `
        (($backendSeg.tone -eq 'ok') -and ($modelSeg.tone -ne 'bad') -and ($modelSeg.tone -ne 'warn')) `
        "backend=$($backendSeg.value)/$($backendSeg.tone) model=`"$($modelSeg.value)`"/$($modelSeg.tone)"
    Add-Check '模型段回灌 core 实际生效模型（设置页水合）' `
        ($modelSeg.value -match 'glm|GLM|bigmodel|ollama|本地') `
        "model=`"$($modelSeg.value)`""
    $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'status-bar-after-settings.json' -InputObject $barAfterSettings

    # ================= ②.5 §4.5 权限中心（页面 + 按需加载 + 完全访问三要素）=================
    # 先拿服务端事实（这一步自身会记一次台账，必须在台账基线之前做完），
    # 再取基线，之后 DOM 侧只允许出现"渲染这一页"产生的那 1 次 overview。
    $permFacts = $null
    try {
        $permFacts = Invoke-OwoCoreApi -Base $base -Path '/permissions/overview' -Token $token
    } catch {
        Add-Check '§4.5.1 服务端 /permissions/overview 可用' $false $_.Exception.Message
    }
    $factDims = @($permFacts.dimensions)
    $factExpanded = @($permFacts.expanded)
    Add-Check '§4.5.3 服务端给出四个维度（文件系统/命令/网络/持久化）' `
        ((@($factDims | ForEach-Object { [string]$_.key }) -join ',') -eq 'filesystem,command,network,persistence') `
        "keys=$(($factDims | ForEach-Object { $_.key }) -join ',')"
    Add-Check '§4.5.1 每维摘要与来源由服务端给出（前端不重算）' `
        ((Get-OwoCount @($factDims | Where-Object { [string]::IsNullOrWhiteSpace([string]$_.summary) -or (-not @('profile','spec') -contains [string]$_.source) })) -eq 0) `
        "sources=$(($factDims | ForEach-Object { "$($_.key):$($_.source)" }) -join ' ')"
    Add-Check '§4.5.2 服务端预生成完全访问风险清单与四条展开规则（确认卡要素来自服务端）' `
        (((Get-OwoCount @($permFacts.full_access.risk_notes)) -ge 4) -and ((Get-OwoCount $factExpanded) -eq 4) -and ($permFacts.grants_persisted -eq $true)) `
        "risk_notes=$(Get-OwoCount @($permFacts.full_access.risk_notes)) expanded=$(Get-OwoCount $factExpanded) grants_persisted=$($permFacts.grants_persisted)"

    # 台账是唯一 HTTP 真相：进出这一页的请求差值必须能对上，
    # "看起来加载出来了"不代表没有偷偷轮询（本页禁止 setInterval）。
    $ledgerBeforePerm = Get-OwoLedgerFacts -Base $base -Token $token -Label 'r45-before-permissions'
    $null = Invoke-OwoCdpEval -Port $cdpPort -Expression $clickPermissionsRail -TimeoutSec 8
    $perm = $null
    $permDeadline = (Get-Date).AddSeconds(18)
    while ((Get-Date) -lt $permDeadline) {
        $perm = Get-CdpJson -Port $cdpPort -Expression $exprPermissionsPage -TimeoutSec 12
        if ($perm -and $perm.present) { break }
        Start-Sleep -Milliseconds 600
    }
    $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'permissions-page.json' -InputObject $perm
    Add-Check '§4.5 权限中心可挂载并渲染出维度矩阵' `
        (($null -ne $perm) -and [bool]$perm.present) `
        "present=$($perm.present) text_len=$($perm.textLen)"
    Add-Check '§4.5.3 四维矩阵齐备且每维都有生效值（不是空占位）' `
        (([int]$perm.dimCount -eq 4) -and (@($perm.dims | Where-Object { [string]::IsNullOrWhiteSpace($_.effective) -or ($_.effective -eq '未知') }).Count -eq 0)) `
        "dims=$(($perm.dims | ForEach-Object { "$($_.key)=$($_.effective)/$($_.source)" }) -join ' ')"
    Add-Check '§4.5.1 服务端事实逐维落到 DOM（摘要与来源不是前端自造）' `
        ((@($perm.dims | Where-Object { [string]::IsNullOrWhiteSpace($_.summary) -or [string]::IsNullOrWhiteSpace($_.source) -or ($_.source -eq '来源未标注') }).Count -eq 0) -and (@($perm.dims | ForEach-Object { $_.key }) -join ',' -eq '文件系统,命令执行,网络访问,授权有效期')) `
        "sources=$(($perm.dims | ForEach-Object { $_.source }) -join ',') labels=$(($perm.dims | ForEach-Object { $_.key }) -join ',')"
    Add-Check '§4.5 当前档位回显中文可读（不是裸枚举）' `
        ($perm.profile -match '工作区|只读|自动|全') "profile=`"$($perm.profile)`""
    Add-Check '§4.5 空列表走空态文案（不是 0 行假装有数据）' `
        (([bool]$perm.pendingEmpty) -and ([bool]$perm.grantsEmpty)) `
        "pending_empty=$($perm.pendingEmpty) grants_empty=$($perm.grantsEmpty) marks=$($perm.emptyStateMarks)"
    # 完全访问走真路径：默认草稿不含不受限维度 → 点「申请」必须给出解释而非静默；
    # 把命令/网络改成不受限后再点 → 确认卡出现，范围 + 时长 + 风险三要素齐备。
    $null = Invoke-OwoCdpEval -Port $cdpPort -Expression ($clickPermAction.Replace('__PERM_ACTION__', 'request-full-access')) -TimeoutSec 8
    $permBenign = Get-CdpJson -Port $cdpPort -Expression $exprPermissionsPage -TimeoutSec 12
    Add-Check '§4.5.2 无需确认时明确告知原因，不留静默死控件' `
        ((-not [bool]$permBenign.confirmOpen) -and ($permBenign.text -match '不含不受限')) `
        "confirm_open=$($permBenign.confirmOpen) text_match=$($permBenign.text -match '不含不受限')"
    foreach ($dim in 'command', 'network') {
        $null = Invoke-OwoCdpEval -Port $cdpPort -TimeoutSec 8 -Expression (
            $setPermDimension.Replace('__PERM_DIM__', $dim).Replace('__PERM_VALUE__', 'unrestricted'))
    }
    $null = Invoke-OwoCdpEval -Port $cdpPort -Expression ($clickPermAction.Replace('__PERM_ACTION__', 'request-full-access')) -TimeoutSec 8
    $permConfirm = Get-CdpJson -Port $cdpPort -Expression $exprPermissionsPage -TimeoutSec 12
    $null = Save-OwoEvidenceJson -Dir $EvidenceDir -Name 'permissions-full-access-confirm.json' -InputObject $permConfirm
    Add-Check '§4.5.2 完全访问二次确认卡出现，且范围/时长/风险三要素齐备' `
        (([bool]$permConfirm.confirmOpen) -and ([bool]$permConfirm.confirmHasScope) -and ([int]$permConfirm.riskItems -ge 1) -and ([int]$permConfirm.durationOptions -ge 2)) `
        "scope=$($permConfirm.confirmHasScope) risks=$($permConfirm.riskItems) durations=$($permConfirm.durationOptions) text=$($permConfirm.confirmText)"
    $null = Invoke-OwoCdpEval -Port $cdpPort -Expression ($clickPermAction.Replace('__PERM_ACTION__', 'cancel-confirm')) -TimeoutSec 8
    $permCancelled = Get-CdpJson -Port $cdpPort -Expression $exprPermissionsPage -TimeoutSec 12
    Add-Check '§4.5.2 取消二次确认不提交任何配置（卡收起且请求数不变）' `
        ((-not [bool]$permCancelled.confirmOpen) -and ($permCancelled.text -notmatch '不含不受限')) `
        "confirm_open_after_cancel=$($permCancelled.confirmOpen)"
    $ledgerAfterPerm = Get-OwoLedgerFacts -Base $base -Token $token -Label 'r45-after-permissions'
    $overviewCalls = @($ledgerAfterPerm.web_business_routes | Where-Object { $_ -match '/permissions/overview' }).Count
    $permDelta = [int]$ledgerAfterPerm.web_business - [int]$ledgerBeforePerm.web_business
    Add-Check '§4.5 权限中心按需加载：整页只发 1 次 overview，无周期轮询' `
        (($overviewCalls -eq 1) -and ($permDelta -eq 1)) `
        "overview_calls=$overviewCalls delta=$permDelta routes=$($ledgerAfterPerm.web_business_routes -join ',')"
    $permLeaks = @(Test-NoForbiddenEcho -Text ([string]$perm.text))
    Add-Check '§4.5 权限页文本不回显绝对路径/授权头/密钥形态' ($permLeaks.Count -eq 0) "hits=$($permLeaks -join ',')"
    # 状态条权限段：§4.5 落地后必须直指权限中心（此前临时降级到设置页）。
    $null = Invoke-OwoCdpEval -Port $cdpPort -Expression $clickStatusBarPermission -TimeoutSec 8
    $permFromStatusBar = Get-CdpJson -Port $cdpPort -Expression $exprPermissionsPage -TimeoutSec 12
    Add-Check '§4.3 状态条权限段直达权限中心（不再降级到设置页）' `
        ([bool]($permFromStatusBar -and $permFromStatusBar.present)) "present=$($permFromStatusBar.present)"
    if (-not $SkipScreenshots) {
        $shotPerm = Join-Path $EvidenceDir 'r4-ui-permissions.png'
        try {
            $permWin = Get-OwoValidatedWindow -ProcessId $shellProc.Id -TimeoutSec 8 -RequireVisible
            if (-not $permWin.ok) { throw "截图前窗口验证失败：$($permWin.fail_reasons -join ';')" }
            $null = Save-OwoWindowShot -Hwnd $permWin.hwnd -Path $shotPerm
            $permMetric = Test-OwoScreenshot -Path $shotPerm -ExpectedWidth $permWin.width -ExpectedHeight $permWin.height
            Add-Check '权限中心截图像素级有效（§4.5 真机外观证据）' ([bool]$permMetric.ok) `
                "$shotPerm（$($permWin.width)x$($permWin.height)） $(Get-OwoScreenshotMetricLine $permMetric)"
        } catch {
            Add-Check '权限中心截图像素级有效（§4.5 真机外观证据）' $false $_.Exception.Message
        }
    }

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
