#requires -Version 5.1
<#
GLM 默认联调启动器（OwO Agent SDK）。

约定（与 gateway.rs 的 DEFAULT_MODEL_* 一致）：
- 端点 https://open.bigmodel.cn/api/paas/v4 与模型 glm-5.3-flash 已是代码内置默认，
  无需再配 OPENAI_BASE_URL / OPENAI_MODEL；
- 唯一必填是凭据：只走环境变量 OPENAI_API_KEY（仓库红线：密钥禁止写入代码/配置/提交，
  也不要提交 dev-glm.local.ps1——该文件名已被 .gitignore 忽略）。

用法：
  # 会话级注入一次（推荐）
  $env:OPENAI_API_KEY = '<你的 GLM key>'
  ./scripts/dev-glm.ps1

  # 用户级持久化（本机生效，不入库）
  setx OPENAI_API_KEY '<你的 GLM key>'

  # 或者临时传参
  ./scripts/dev-glm.ps1 -ApiKey '<你的 GLM key>'

默认监听 http://127.0.0.1:4097（前端面板与 TS SDK 集成测试的目标口）；
WSL/多开场景用 -Port 换端口。服务起来后：
  cargo test -p owo-agent-server --test desktop_world_api_tests   # 独立测试无需服务器
  pnpm --dir clients/ts test                                      # TS 集成测试打本服
#>
[CmdletBinding()]
param(
    [string]$ApiKey = $env:OPENAI_API_KEY,
    [int]$Port = 4097,
    [string]$Workspace = (Split-Path -Parent $PSScriptRoot)
)
$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($ApiKey)) {
    throw "缺少凭据：请设置环境变量 OPENAI_API_KEY 或用 -ApiKey 传入（凭据不得写入脚本/仓库文件）"
}

$env:OPENAI_API_KEY = $ApiKey
if (-not $env:OPENAI_BASE_URL) { $env:OPENAI_BASE_URL = 'https://open.bigmodel.cn/api/paas/v4' }
if (-not $env:OPENAI_MODEL) { $env:OPENAI_MODEL = 'glm-5.3-flash' }
if (-not $env:OWO_CLOUD_ENABLED) { $env:OWO_CLOUD_ENABLED = 'true' }

Write-Host "[dev-glm] provider=$env:OPENAI_BASE_URL model=$env:OPENAI_MODEL port=$Port"
Write-Host "[dev-glm] workspace=$Workspace"
cargo run -q -p owo-agent-cli -- serve --port $Port --workspace $Workspace
