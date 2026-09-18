# AGENTS.md

本仓库包含两个项目：

- `OwO 输入法`（根目录 C++ 工程）：历史基线，当前不主动修改。
- `agent-sdk/`：Codex 式 Agent 智能体 SDK，**当前活跃开发目标**。

## 开发规则

- 所有新开发放在 `agent-sdk/` 内；不修改 OwO C++ 输入法代码。
- 技术基线：`builGoal/技术文档-AI智能体输入法.md`（v0.6，只实施 Agent 智能体方案，输入法路线不实施）。
- 模型凭据只经环境变量（`OPENAI_API_KEY` 等），禁止写入代码、配置或提交。
- 权限默认 deny；任何工具调用必须经过权限策略，审批与主 Agent 分离。
- M1 验收项：会话、审计、diff/revert、工具权限必须保持工作，改动需带契约测试。
- Rust 代码保持 `cargo fmt` 与 `clippy` 干净；契约测试随功能提交。
- **文件编码**：所有源文件必须为 UTF-8；Windows 下写入含中文的 .rs/.md 文件时禁止经 GBK 控制台中转（会导致 mojibake 损坏）。提交前用 `cargo fmt --check` + `git diff` 抽查。
- **并行协作**：多个 Agent 并行时按 `AGENTS-COORD.md` 认领文件；同一文件同一时间只允许一个 Agent 修改；涉及 `owo-agent-server/src/lib.rs` 等核心文件的改动需先跑 `cargo check` 验证。
- **HTTP 契约**：服务端新增/修改路由必须同步 `tests/route_contract_tests.rs`（路由面契约测试），防止接口回归丢失。
- **原生依赖前置（构建挂死陷阱）**：新开进程**不带** ONNX Runtime 环境变量，直接跑 `cargo` 会让 `ort-sys`/`sherpa-onnx-sys` 退化为联网下载产物，在受限沙箱内**静默挂死**（实测 15 分钟零 CPU、无 `rustc`，无任何报错）。任何手写 `cargo` 命令前必须先注入解析入口：

  ```powershell
  cd agent-sdk
  . scripts\resolve-ort.ps1; Resolve-OwoOrtEnv -Quiet   # 仅进程级注入，不写用户/机器级变量
  cargo test -p owo-agent-server --locked               # 之后才可执行
  ```

  `ci-gate.ps1` / `dev.ps1` / sidecar 与安装包脚本已统一走该入口；只注入 `SHERPA_ONNX_LIB_DIR`/`ORT_LIB_PATH`/`ORT_LIB_LOCATION` 三变量的做法不再允许另复制一份探测逻辑（方案 §7.2）。

## 模型凭据与环境变量（OPENAI_*）

模型凭据**只经环境变量注入**，本仓库已将 `OPENAI_API_KEY` 配置在 **Windows 用户级环境变量**（注册表 `HKCU\Environment`），对全体新开进程生效；密钥本体禁止写入任何代码、配置文件或提交。

### 已配置的变量（当前机器）

| 变量 | 状态 | 说明 |
|---|---|---|
| `OPENAI_API_KEY` | ✅ 已配置（用户级） | GLM/BigModel 密钥，格式 `id.secret`，共 49 字符 |
| `OPENAI_BASE_URL` | 未设置 | 缺省即内置 BigModel 端点 `https://open.bigmodel.cn/api/paas/v4`，无需设置 |
| `OPENAI_MODEL` | 未设置 | 缺省即内置 `glm-5.3-flash`，无需设置 |

### 如何设置 / 更换密钥

```powershell
# 设置或更换（写入用户级环境变量，永久生效；对已运行的进程需重开终端）
[Environment]::SetEnvironmentVariable('OPENAI_API_KEY', '<你的密钥>', 'User')

# 验证是否已配置（只看存在性与长度，不要回显密钥值）
$k = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
"SET=$([bool]$k) LEN=$($k.Length)"
```

### 子进程如何取用（脚本/Agent 任务中的标准写法）

新开的 PowerShell 子进程不会自动继承"用户级"变量的注册表新值（父进程环境是启动时快照），标准注入写法：

```powershell
$k = [Environment]::GetEnvironmentVariable('OPENAI_API_KEY', 'User')
$env:OPENAI_API_KEY = $k   # 注入本进程环境，子命令即可继承
cargo run -q -p owo-agent-cli -- product-eval run --exec agent --agents single --reps 1 --only code-bug-fix --fresh
```

### 可选辅助变量

- `OPENAI_BASE_URL` / `OPENAI_MODEL`：覆盖缺省端点/模型（如指向 Ollama 等本地端点；指向本地端点时允许空密钥）。
- `OWO_CLOUD_ENABLED=false`：拒绝一切云端模型调用（数据出境开关）。
- `OWO_HTTP_PROXY` / `HTTPS_PROXY`：出网代理。
- `OWO_EVAL_PRICE_IN_PER_MTOK` / `OWO_EVAL_PRICE_OUT_PER_MTOK`：评测报告的成本估算单价（$/百万 token；未设置时报告 cost 字段为 null，tokens 仍真实落盘）。
- `OWO_MCP_SCHEMA_BUDGET_BYTES`：MCP 工具 schema 压缩阈值。

### 红线重申

密钥只存在于：环境变量 / Windows 注册表（用户级）。**禁止**出现在任何 `.rs`/`.json`/`.md`/`.ps1` 等仓库文件、日志输出或对话回显中；排障时一律用 `SET=True LEN=…` 这类掩码方式确认。
