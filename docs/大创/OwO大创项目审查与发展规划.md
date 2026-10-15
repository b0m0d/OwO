# OwO 智能输入法 × Agent：大创项目技术审查与发展规划

> 版本：v1.0　｜　日期：2026-10-16　｜　适用对象：大创项目组全体成员、指导老师
>
> 本文是对 OwO 项目两个板块（**OwO 输入法板块**与 **Agent 板块**）的代码级审查，内容包括：现存问题、两板块合并方案、后续发展方向，以及如何在大创评审中体现创新。

---

## 0. 审查范围与方法

### 0.1 审查对象

| 板块 | 仓库 / 分支 | 审查时的提交 | 说明 |
|---|---|---|---|
| 输入法 | `b0m0d/OwO`（`master`） | `e1c0537`（2026-10-11，v0.2.3） | 与 `4919545484946757/OwO-release` 的 HEAD 完全一致，是同一份代码 |
| Agent 引擎 | `3311930677/LingXi-Suite`（`engine` 分支，目录 `agent-sdk/`） | `be6298f`（2026-09-30） | OwO Agent SDK，Rust 实现，含 `serve-ime` 输入法对接 |
| Agent 桌面端 | `3311930677/LingXi-Suite`（`desktop` 分支） | `9a76fae`（2026-09-30） | 灵犀桌宠 overlay（Tauri）、桥接 crate、另一套 Rust 拼音引擎 |
| 历史仓库（参考） | `3311930677/LingXi-DesktopAgent`、`3311930677/Intelligent-Input-Method-v2` | — | 前者是 desktop 分支的前身，后者是早期输入法分叉（v0.9.1，含语音 Broker） |

### 0.2 方法与局限

- 方法：完整克隆后做静态阅读，覆盖目录结构、核心源码、协议、测试、文档、ADR 和提交历史；关键结论都附了文件路径，便于逐条复核。
- 局限：审查环境是 Linux，**没有在 Windows 上编译、运行或实测**（项目依赖 MSVC/TSF/WinUI 3），所以性能数据引用的是仓库自带的基准报告。文中「问题」指代码事实或设计风险，不代表已复现的 Bug。

### 0.3 规模速览

| 模块 | 语言 | 规模（约） |
|---|---|---|
| OwO 输入法 `src/` | C++20 | 2.2 万行（其中 `src/tsf/text_service.cpp` 5,707 行） |
| OwO 设置中心 / 安装器 `apps/settings_center` | C# / WinUI 3 | 4,000 行 |
| OwO 测试 `tests/` | C++ | 58 个 CTest 目标 |
| Agent 引擎 `agent-sdk/crates` | Rust | 10 万行（`owo-agent-core` 6.2 万、`owo-agent-server` 2.7 万、`owo-agent-ime` 4,100） |
| Agent 引擎测试 | Rust | 约 1,100 个 `#[test]` / `#[tokio::test]` |
| Agent HTTP 接口 | — | 约 210 条路由 |
| 桌面端 `desktop` 分支 | Rust + JS | 1.45 万行 Rust |

---

## 1. 项目全景：两个板块现在是什么关系

### 1.1 当前实际链路

```text
┌──────────────── OwO 输入法（b0m0d/OwO，C++） ─────────────────┐
│ 应用程序 → OwO.TSF.dll（TextService，5.7k 行）                 │
│          → Core Service（命名管道，引擎/词库/学习/插件执行器） │
│          → ModelHost（libime N-gram 重排，可选 ONNX 运行时）   │
│          → PluginHost → org.owo.agent-ipc 连接器（full_trust） │
└───────────────────────────┬────────────────────────────────────┘
                            │ \\.\pipe\OwO.Agent.External.v1
                            │ 4 字节长度 + JSON，协议 v3，10s 一问一答
┌───────────────────────────▼────────────────────────────────────┐
│ owo-agent serve-ime（LingXi-Suite/engine，Rust）               │
│   管道面 → ImeBridge →（HTTP 回环）→ Agent Loop → LLM / 工具   │
│   HTTP 面 ← 桌宠 overlay（desktop 分支）/ Web 工作台            │
└────────────────────────────────────────────────────────────────┘
```

### 1.2 一句话诊断

> **输入法是「入口」，Agent 是「大脑」，但入口目前只往大脑递了一个字符串，大脑做的又是通用桌面 Agent。** 两者在工程上已经能连通，在**产品和创新层面还没有真正融合**。大创选题「智能输入法」的核心价值——输入法独有的上下文和交互能力——两边都没有吃透。

### 1.3 团队与历史上的分叉

从提交历史看，项目已经出现过多次分叉：

1. **输入法有两条线**：`Intelligent-Input-Method-v2`（v0.9.1，含 `modules/voice-input`、`modules/ai-text`）和 `OwO-release` / `b0m0d/OwO`（v0.2.3，新的插件与输入方案体系）。两者共用同一份《Codex 项目总控提示词》，但代码已经分道扬镳。
2. **Agent 有两条线**：`LingXi-DesktopAgent`（桌宠 + 小工具 + 本地 GGUF 推理）先演变为 `LingXi-Suite/desktop`，再收敛为「Agent 显示板块」；`agent-sdk` 则独立长成了 Codex 式的通用 Agent 引擎。
3. **拼音引擎至少有三套**：OwO 的 C++ 引擎、desktop 分支的 Rust `crates/assistant-ime`，以及 `docs/weasel-integration.md` 里规划的 Rime/小狼毫 + `plugins/lingxi-rime-filter`。

分叉本身不是错，但大创需要**一个能讲清楚的主线**。

---

## 2. 输入法板块（OwO）审查

### 2.1 已有的亮点（答辩时应当突出）

- **架构规范**：TSF 进程内只放轻量逻辑，Core Service、ModelHost、PluginHost 都是独立进程，命名管道 IPC 带版本、请求 ID、超时与降级，基本符合总控提示词 3.2 节的原则。
- **工程纪律好**：`docs/adr/` 下有 37 份 ADR，许可证、哈希和来源都固定；词库来自雾凇拼音并有 manifest（`data/rime-ice-*.manifest`），libime 模型也固定了 SHA-256（ADR-0033）。
- **性能有据可查**：`docs/benchmarks/` 中的精确拼音查询 p50 约 0.24 ms、p95 约 1.7 ms，辅助（简拼/纠错）路径 p95 约 6.4 ms，满足低延迟目标。
- **插件安全体系完整**：AppContainer 沙箱、离线签名、逐版本授权、高风险插件二次告知（ADR-0011～0022）。
- **Agent IPC v3 协议设计有创新性**：「拼音区间槽位」（`PinyinRange` / `TaskSlot` / `unconsumed_ranges`）、「候选只锁定一个槽位、不重发整句」「高风险操作不能由候选框授权」，这些都是输入法独有的交互设计（`include/owo/agent/agent_protocol.h`、`docs/plugins/agent-ipc-integration.md`）。

### 2.2 问题清单

优先级：**P0** 影响大创主线或安全，必须处理；**P1** 显著影响质量或可维护性；**P2** 建议改进。

#### P0-1　「智能」部分偏弱，撑不起「智能输入法」的选题

- 总控方案规划了三个模型：候选排序 `UER Chinese RoBERTa L4-H256`、智能联想 `MiniCPM4-0.5B`、高级重排 `Qwen3-Reranker-0.6B`。实际情况是：
  - RoBERTa：因为**权重许可不明、且是 MLM 检查点、没有排序头**，处于「技术候选，许可阻塞」状态，未下载也未集成（`docs/architecture/p3-kickoff.md`、ADR-0005）。
  - ONNX Runtime 链路只用一个 **589 字节的合成测试模型**跑通（`docs/architecture/p3a-acceptance.md`），默认构建 `OWO_ENABLE_ONNXRUNTIME=OFF`。
  - MiniCPM4 / Qwen3-Reranker 只保留了接口。
- 当前真正在线的「智能」是 **libime 静态 N-gram 重排 + 本地词频学习 + 会话上下文**（ADR-0031/0032/0033）。这在工程上很扎实，但相比微软拼音、搜狗等商业输入法并没有本质区别，**评审很难把它认定为创新点**。
- 建议：见第 5 章「方向 A：上下文感知的神经候选」，至少落地一个**有真实许可、真实效果对比**的小模型。

#### P0-2　Agent 模式「只出不进」：Agent 的结果无法上屏

- 协议规定 `message` 只用于展示，`commands[]` 是唯一可选的结构化候选（`docs/plugins/agent-ipc-integration.md` 第 175 行）；协议里**没有「把文本提交到光标处」的动作**。
- TSF 侧 Agent 分支没有任何 `commit_text` 调用来提交 Agent 结果（`src/tsf/text_service.cpp` 中 Agent 相关代码只会把 `v` 本身作为普通字母提交，见第 1223 行）。
- Agent 端虽然生成了 `insert-reply`（`text.insert`）候选（`agent-sdk/crates/owo-agent-ime/src/commands.rs:68`），但选择后只是把回复再放进 `message` 显示一遍（`state.rs` 中 `CommandAction::InsertReply` 分支）。
- 后果：「帮我把这段话改正式一点」「翻译成英文」「回复这条消息」这类**输入法 + AI 最有价值的场景都做不了**。用户在候选框里看到答案，却不能把它放进文档。
- 建议：协议 v4 增加受控的输出动作，见 4.4 节。

#### P0-3　协议最有创新的「槽位任务」在两端都没跑通

- IME 端完整实现了槽位锁定、区间消耗和 revision 校验（`text_service.cpp` 中的 `apply_agent_task` / `select_agent_command`）。
- Agent 端 `compose_prompt` 只是把 `user_input` 和上下文拼成一段提示词交给 LLM（`owo-agent-ime/src/bridge.rs:398`），**从不根据拼音生成 `TaskDraft`/槽位**：`set_task()` 只在测试中被调用（`tests/state_machine.rs`），`CommitTask` 直接返回 `not_implemented`（`bridge.rs:160`）。
- 集成方案里的验收用例「`vsanzhongtixingwokaihui` → 槽位草稿 + 时间变体候选 → commit_task 创建提醒」（`desktop` 分支 `docs/ime-agent-integration-plan.md` E1.8）**尚未达成**。
- 这是整个项目**最有可能写成论文和专利的部分**，目前停在协议层。

#### P0-4　Agent 上下文的隐私边界需要收紧

1. **敏感输入识别不一致**：Agent 请求的 `sensitive_input` 只认 Win32 `Edit` 控件且带 `ES_PASSWORD` 样式（`text_service.cpp:155`）。浏览器、Electron、WinUI、Qt 中的密码框通常不是 Win32 `Edit`，会被判为非敏感。而 `src/tsf/input_method_context.cpp` 已经实现了基于 TSF `InputScope`（`IS_PASSWORD`/`IS_NUMERIC_PIN`/`IS_PRIVATE`）的判断，**Agent 路径没有复用它**。
2. **跨应用上下文聚合**：Core Service 在内存中保存最近最多 32 段、128 KB 的「OwO 上屏文本」，每次 submit 都会附上**来自其他应用**的片段（relevance 500/800，见 `src/ipc/named_pipe.cpp` 第 52～84 行和第 1310 行附近）。也就是说，用户在聊天软件里打的字，可能在另一个应用里调用 Agent 时被发给 LLM。文档对此有说明（`agent-ipc-privacy.md` 第 18 行），但**默认值偏激进**，而且 Agent 端 `egress.cloud_enabled` 默认是 `true`（`owo-agent-core/src/settings.rs:190`），这些数据会直接出境到云端模型。
3. **脱敏规则偏英文**：`filter_sensitive_text` 只匹配 `password`、`token`、`api_key` 等英文关键字和 Luhn 校验的银行卡号（`src/agent/agent_protocol.cpp:1065` 起），**不覆盖「密码：」「验证码」、手机号、身份证号、邮箱**等中文场景常见的敏感信息。
4. 连接器插件声明了 `system.full_trust`（`plugins/agent-ipc/manifest.json`），以当前用户的完整令牌运行、不进 AppContainer。这对于连接外部管道可以理解，但需要在文档和授权页中显著说明。

#### P1-1　TSF 层过于臃肿，违背自身「TSF 尽量小」的原则

- `src/tsf/text_service.cpp` 有 **5,707 行、116 个成员函数**，同时承担按键处理、预编辑、候选窗、标点、快捷键、学习链、插件输入方案和**整套 Agent 会话状态机**。
- `src/ipc/named_pipe.cpp` 有 2,166 行，把传输层与 Agent 会话、上下文聚合、轮询转发等业务逻辑混在一起。
- 风险：TSF DLL 跑在每个宿主进程里，越复杂，崩溃面越大；多人协作时冲突集中在一个文件上；也不利于给评审讲清架构。
- 建议：把 Agent 状态机抽成 `owo::agent::ImeAgentSession`（纯逻辑、可单测），TSF 只负责转发按键和渲染；把 Core 侧 Agent 逻辑从 `named_pipe.cpp` 移到 `src/core/agent_router.cpp`。

#### P1-2　规划的首批官方插件大部分未落地，而 Agent 侧已经有了

| 总控规划（5.4 节） | 输入法侧现状 | Agent 侧已有能力 |
|---|---|---|
| 截图工具 | 无 | GDI 截图、PrintWindow 窗口截取 |
| OCR 识别与翻译 | 无 | 本地 ONNX PP-OCRv4、Windows Media.Ocr、PP-OCRv6 |
| 语音输入 | 无（v2 分叉中有） | sherpa-onnx + SenseVoice-Small 本地 STT |
| 文本润色与翻译 | 无 | LLM 网关 + `plugins/owo-translate` |
| 字符编码转换 | 只有 Unicode 符号列表（`plugins/unicode-palette`） | 无 |
| 快捷键管理 | 设置中心内置 | 无 |
| AI Agent 核心插件 | 只有 IPC 连接器 | 完整引擎 |

这说明**两边在重复造轮子，也各自缺对方已有的东西**，是合并的天然切入点（见 4.5 节）。

#### P1-3　工程与仓库卫生

- **没有 CI**：仓库中不存在 `.github/workflows`，58 个 CTest 只能在开发者本机运行。
- **根目录杂乱**：有 `..gitignore.un~`、`.gitignore~`（编辑器备份文件）、`start-owo-dev.cmd - 快捷方式.lnk`、9 个 `open-owo-installer-*.cmd`，以及 1,450 行的《Codex 项目总控提示词》。
- `项目计划.md` 实际只是「字符编码转换插件」的增补，名不副实；`docs/architecture/config-schema-v1…v14.md` 版本断档（缺 v6～v12），新人难以确认现行 schema。
- 发行包未做代码签名（README 已说明）；对一个要注册 TSF COM 组件的输入法来说，这会触发 SmartScreen，也会影响试用推广。

#### P2　体验类问题

- `v` 前缀可发现性差，没有任何 UI 提示；部分用户习惯用 `v` 进入英文或符号模式，存在认知冲突。建议增加可配置的触发方式（如 `Ctrl+Shift+A`、双击 Shift，或候选栏末尾的「问 AI」按钮）。
- Agent 交互采用「10 秒一问一答 + 轮询」，用户要等模型回合（秒级到分钟级），而候选窗只显示「思考中」，缺少进度和中间结果的可视化。

---

## 3. Agent 板块（LingXi-Suite）审查

### 3.1 已有的亮点

- **能力完整**：Agent Loop、工具注册、三档权限审批、会话 fork/revert、MCP、技能包（SKILL.md + manifest + tests）、审计落 SQLite、OpenAPI 契约测试、TypeScript SDK。
- **本地优先的感知能力**：UIA 无障碍树、本地 ONNX OCR、SenseVoice 本地 STT、Ollama 视觉模型、截图只在内存的环形缓冲中保存。
- **可复现的实验台**：`sim/` 下的模拟 QQ 与模拟浏览器，可以在不碰真实桌面的情况下跑端到端回归（`scripts/sim-regression.py`），这对写论文、做评测非常有价值。
- **已经和输入法打通**：`owo-agent serve-ime` 实现了 v3 管道协议、幂等、`state_revision` 单调、取消转 `/abort`，单进程同时提供管道面和 HTTP 面（`crates/owo-agent-ime`）。

### 3.2 问题清单

#### P0-5　范围严重膨胀，偏离「智能输入法」选题

- 引擎有约 **210 条 HTTP 路由**，`owo-agent-core` 有 70 多个模块，包括 `fleet`（多节点集群）、`cloud_exec`（云端执行）、`team`、`goal`、`workflow`、`plugin_market`、`notes`、`observability/SLO/Prometheus`、`usage/topup`（预算充值）等。Web 工作台有 12 个扩展面板。
- 这些能力与输入法关系很弱，带来三个问题：
  1. **评审视角**：容易被看作「复刻 Codex/Claude Code 的通用 Agent」，创新点被稀释，也很难说明和选题的关系。
  2. **维护视角**：10 万行 Rust 由少数人维护，大量功能没有真实用户；任何重构都牵一发动全身。
  3. **安全视角**：攻击面越大，越难保证「输入法级」的隐私承诺。
- 建议：按 4.3 节的取舍表**冻结与输入法无关的模块**，不删代码，用 Cargo feature 关闭，答辩只讲主线。

#### P0-6　本地 API 鉴权可被本机任意进程绕过

- 服务端对业务接口启用了 bearer token，但 `/auth/token` 是**公开接口，会直接返回 token**（`owo-agent-server/src/auth_token.rs:213-215`，路由见 `lib.rs:275`）。CORS 白名单只能阻止跨域网页读取响应，**挡不住本机任何进程用 HTTP 请求取 token**，之后就能调用 `/session/{id}/turn` 驱动 Agent 读写文件、操作桌面（computer-use）。
- SSE 事件流路径（`/…/events`）**豁免鉴权**（`lib.rs` 第 20 行注释），而事件流里可能包含会话内容。
- `AutoApprover { allow: true }` 在 `--no-approval`、评测、子代理等路径中使用（`owo-agent-cli/src/main.rs:1149/2745`、`owo-agent-core/src/agent.rs`）。如果被误用到 `serve-ime`，就会让输入法触发的任务无需审批直接执行。
- 建议：token 只通过 ACL 保护的文件或 Tauri IPC 下发给桌宠，取消公开的 `/auth/token`；SSE 改用一次性 query token；`serve-ime` 强制禁止 AutoApprover；`egress.cloud_enabled` 默认改为 `false`，或在首次使用时弹出明确授权。

#### P1-4　协议双份手写，存在漂移风险

- C++ 端 `src/agent/agent_protocol.cpp`（1,116 行）和 Rust 端 `crates/owo-agent-ime/src/protocol.rs`（600 行）**各自手写编解码与校验**。虽然有 `docs/plugins/agent-ipc-v3.schema.json`，但两端都不是由它生成的。
- 已有的风险信号：两端常量（payload 上限、槽位上限等）靠人工同步；v2/v3 兼容逻辑只在 C++ 端有。
- 建议：建立独立的 `protocol/` 目录，以 JSON Schema 为**唯一事实来源**，两端在 CI 中用同一批 golden JSON 做往返测试（4.4 节）。

#### P1-5　重复建设与仓库卫生

- **两套桌面前端**：engine 分支有 `desktop/web` + `desktop/tauri`，desktop 分支有 `apps/overlay`（Tauri），皮肤资产在两个分支中各存一份（每张 spritesheet 约 2 MB）。
- **第三套拼音引擎**：desktop 分支的 `crates/assistant-ime`（纯 Rust 拼音引擎）、`apps/ime-server`、`apps/ime-repl`，以及基于 Rime/小狼毫的 `plugins/lingxi-rime-filter`。这与 OwO 的 C++ 引擎重复，**也与总控「不基于 Rime 二次开发」的非目标冲突**。
- **二进制入库**：engine 分支提交了 `dist/OwO-Agent-IME/owo-agent.exe`（65 MB）和 `dist/OwO-Agent-IME-0.1.0-debug.zip`（20 MB），导致仓库长期膨胀。应改用 GitHub Releases，并用 `git filter-repo` 清理历史。
- **AI 协作残留**：`.coord6/`、`.coord7/`、`.coord8/`（多智能体 STATUS/DEPENDENCIES 文件）、`SECURITY_UPDATE_SUMMARY.md`、`storage_crypto_refactor_summary.md`、`demo-output.md`、`stream-demo.md`、空目录 `agent-sdk/agent-sdk/`；`.owo-agent/skills` 与 `skills/` 重复；`AGENTS.md` 仍是未填写的模板；README 引用的 `../builGoal/技术文档-AI智能体输入法.md` 不在仓库中。
- **「分支即项目」**：`main` 分支只有一份 README，`engine` 与 `desktop` 两个分支**没有共同祖先**（`git merge-base` 为空）。这让 PR、CI、版本号和 issue 管理都很别扭，跨两边的改动无法放在同一个提交里。
- `docs/ci.md` 描述了 pr/merge/nightly/weekly 四条流水线，但 engine 分支的 `agent-sdk/` 下**没有对应的 workflow 文件**。只有 desktop 分支有 `.github/workflows`。

#### P1-6　素材版权风险

- 桌宠皮肤 `petdex-*` 包含哆啦A梦、蜡笔小新、尼尔 2B、02（Zero Two）、灰原哀、星野爱、Aerith 等**商业动漫或游戏角色**。`skin.json` 中标注了「petdex 社区 · MIT」，但 **MIT 只能授权代码或作品作者本人的权利，不能覆盖角色版权**。
- 在大创结题展示、公开发布、参赛和申请软著时，这都可能成为问题。建议默认只保留自绘的 `lingxi-cat`、`lingxi-hamster`、`codex-cloud`，第三方角色皮肤从仓库中移除或改为用户自行导入。

#### P2　其他

- `serve-ime` 与 `serve` 通过 PidFile 互斥，用户需要知道该启动哪一个。建议合并为一个常驻服务，按需开启管道面。
- 引擎是「管道 → HTTP 回环 → Agent」的结构，设计文档给出了理由（复用契约测试），但多了一跳序列化和一个监听端口。长期可以把 `ImeBridge` 直接挂在 `AppState` 上。
- 文档和提交记录显示，大量代码由 AI 编码助手按提示词批量生成。这本身没有问题，但**团队必须保证每个核心模块都有成员能讲清原理**，否则答辩和后续维护都有风险。

---

## 4. 合并方案

### 4.1 合并原则

1. **基础输入永不依赖 Agent**：Agent、模型或网络不可用时，打字体验不受任何影响（沿用总控 2.2 节）。
2. **一个产品、一个主线**：对外统一叫「OwO 智能输入法」，Agent 是它的「智能内核」，桌宠和工作台是它的「可信界面」。
3. **一个协议、一个事实来源**：协议 Schema 独立成目录，两端代码由它约束。
4. **输入法优先的取舍**：与输入法主线无关的 Agent 能力一律冻结（feature-gate），而不是继续扩张。
5. **隐私默认最小化**：默认本地模型、默认不跨应用、默认不出境，用户逐项开启。

### 4.2 目标架构

```text
                       ┌───────────────────────────────┐
  应用程序 ── TSF ──▶  │ OwO.TSF.dll（瘦身后 ≤ 2.5k 行） │
                       │  按键/编辑会话/候选渲染        │
                       └──────────────┬────────────────┘
                                      │ owo-ipc（已有）
                       ┌──────────────▼────────────────┐
                       │ OwO Core Service（C++）        │
                       │  拼音引擎 / 词库 / 用户学习    │
                       │  CandidatePipeline            │
                       │  AgentRouter（新，从 TSF 和   │
                       │   named_pipe.cpp 中抽出）      │
                       └───┬──────────────┬────────────┘
                 ModelHost │              │ Agent IPC v4（protocol/ 生成）
       （本地小模型重排/联想）│              │
                       ┌───▼───┐   ┌──────▼─────────────────────────┐
                       │ ONNX/ │   │ OwO Agent Engine（Rust，精简版） │
                       │libime │   │  IntentParser（拼音→意图/槽位） │
                       └───────┘   │  Agent Loop + 审批 + 审计       │
                                   │  能力：OCR / STT / 翻译 / 文件  │
                                   │  / 桌面操作（feature-gate）     │
                                   └──────┬─────────────────────────┘
                                          │ 本地 HTTP/SSE（鉴权修复后）
                                   ┌──────▼─────────────────────────┐
                                   │ 灵犀桌宠 / 工作台（Tauri，单套）│
                                   │  = 高风险操作的「可信确认界面」 │
                                   └────────────────────────────────┘
```

### 4.3 功能取舍表

| 模块 | 处理 | 理由 |
|---|---|---|
| OwO C++ 拼音引擎、TSF、插件体系、设置中心 | **保留（主线）** | 选题核心，工程质量最好 |
| `owo-agent-core`：agent loop、tools、permissions、session、audit、mcp、skill | **保留** | Agent 内核 |
| OCR（onnx_ocr/paddle_ocr/ocr）、STT、vision、perception（L0/L1） | **保留，并包装为输入法插件** | 补齐输入法规划中的 OCR/语音插件 |
| learn / observe / memory / skill_health | **保留（研究方向）** | 「输入习惯 → 操作习惯」个性化学习 |
| computer_use / executor / action_program | **保留但默认关闭** | 能力强、风险高，演示时开启 |
| `owo-agent-ime`（serve-ime） | **保留并重点增强** | 合并枢纽 |
| `sim/` 模拟实验台 | **保留** | 评测与论文实验基础设施 |
| fleet / cloud_exec / node_agent / remote_step / lease / worker_pool | **冻结（feature 关闭）** | 与输入法无关，攻击面大 |
| team / goal / workflow / plugin_market / notes / usage 充值 / SLO | **冻结** | 通用平台功能，稀释主线 |
| engine 分支 `desktop/web` + `desktop/tauri` | **合并进单一桌面端** | 去掉重复前端 |
| desktop 分支 `assistant-ime`、`ime-server`、`ime-repl`、`lingxi-rime-filter`、weasel 方案 | **归档移出主仓** | 第三套拼音引擎，与主线冲突 |
| `Intelligent-Input-Method-v2` | **归档**，语音 Broker 思路迁移到插件 | 旧分叉 |
| 第三方角色皮肤 | **移除** | 版权风险 |

### 4.4 协议层改造：Agent IPC v4

1. **单一事实来源**：新建 `protocol/agent-ipc/`，内容包括 `v4.schema.json`、`golden/*.json`（请求和响应样例）、`CHANGELOG.md`。
   - Rust 端用 `schemars` / `typify` 生成或校验类型，C++ 端用脚本生成常量头文件；两端 CI 都跑同一批 golden 往返测试。
2. **新增受控输出动作**（解决 P0-2）：
   - `Command.output = { kind: "commit_text" | "replace_selection" | "copy", text, preview }`；
   - 只有用户在候选框中**显式选中**该候选，TSF 才在当前编辑会话中提交文本；
   - `replace_selection` 要求请求时就带了选区，并且 TSF 校验当前选区未变化（generation 校验）；
   - 输出文本同样经过长度限制和控制字符过滤，不允许插入换行以外的控制字符。
3. **槽位任务真正落地**（解决 P0-3）：Agent 端新增 `IntentParser`，输入 `raw_pinyin` + `segmented_pinyin` + `selected_text`，输出带 `source_ranges` 的 `TaskDraft`。实现可分两级：
   - 规则 / 小模型（本地，<100 ms）先给出意图和槽位草稿；
   - LLM（可选）在用户确认前做补全和校正。
   - 先实现 2～3 个高频意图，如「提醒/日程」「查找文件」「翻译/润色」，把 `commit_task` 做实。
4. **隐私字段收紧**（解决 P0-4）：
   - `sensitive_input` 统一由 `input_method_context` 的 InputScope 判定，再叠加 `ES_PASSWORD` 兜底；
   - 跨应用上下文默认关闭（`context.cross_app = false`），开启后在候选框显示「将附带 N 段其他应用文本」；
   - 脱敏规则补充中文关键字（密码、口令、验证码、卡号）、11 位手机号、18 位身份证号（带校验位）和邮箱；
   - 请求中增加 `privacy.egress_allowed`，由 IME 告诉 Agent 当前上下文是否允许出境，Agent 必须遵守。
5. **流式进度**：保留 10 s 管道一问一答（兼容），新增 `progress_text`（如「正在读取文件…」「已找到 3 个结果」），让候选窗能显示中间状态。

### 4.5 插件层对接：Agent 能力反哺输入法插件

利用 OwO 已有的 `.owopkg` 进程插件体系，把 Agent 的能力包装成输入法官方插件，**一份实现、两个入口**：

| 输入法插件 | 实现方式 | 触发方式 |
|---|---|---|
| OCR 识别与翻译 | 插件进程调用 Agent 引擎的 `/perception/ocr/region` 或直接链接 `onnx_ocr` | 快捷键框选 → 结果进入候选框 → 选择后上屏 |
| 语音输入 | 调用 `/stt/transcribe`（SenseVoice 本地） | 按住快捷键说话，松开后结果进候选 |
| 文本润色与翻译 | 走 v4 `replace_selection` | 选中文本 + `v` 或快捷键 |
| 字符编码转换 | 纯本地 C++ 插件（无需 Agent） | 按《项目计划》的设计执行 |

这样，**输入法规划中的 P4 插件可以很快补齐**，Agent 侧的感知能力也有了「输入法入口」这一独特场景。

### 4.6 仓库组织

**推荐：合并为 monorepo `OwO`**（保留历史）：

```text
OwO/
├── ime/                # 现 b0m0d/OwO 全部内容（C++ / WinUI）
├── agent/              # 现 LingXi-Suite/engine 的 agent-sdk（精简后）
├── desktop/            # 合并后的唯一 Tauri 桌宠/工作台
├── protocol/           # Agent IPC Schema + golden 样例 + 代码生成脚本
├── plugins/            # 官方插件（含 OCR/语音/翻译包装）
├── docs/               # 统一文档（ADR 继续编号）
├── eval/               # 评测数据集与脚本（见第 6 章）
└── .github/workflows/  # windows-ime.yml / agent.yml / desktop.yml / protocol.yml
```

迁移步骤（建议一个周末内完成）：

1. 冻结各仓库的写入，打标签 `pre-merge-2026-10`。
2. `git subtree add --prefix=ime <OwO-release> master`，`agent`、`desktop` 两个分支同理，保留各自的提交历史。
3. 删除 `dist/` 二进制和 `.coord*` 等残留；对新仓库执行一次 `git filter-repo --strip-blobs-bigger-than 10M`。
4. 添加四条 CI：Windows runner 跑 CMake + CTest，Rust 跑 fmt/clippy/test，Tauri 构建，protocol golden 测试。
5. 旧仓库 README 改为「已迁移至 …」，并设为 Archived。

> 如果团队暂时不想动仓库结构，**最低限度**也要：把 `protocol/` 单独抽成仓库并作为两边的 git submodule；engine 和 desktop 合并到同一分支的不同目录。

---

## 5. 未来发展方向

以下方向都要求「**只有输入法能做，或者输入法做得最好**」，这是区别于通用 Agent 的关键。

### 方向 A：上下文感知的神经候选（补强「智能输入法」本体）

- **问题**：当前排序靠 N-gram + 词频，不理解语义和应用场景。
- **方案**：在 ModelHost 中运行一个**许可证清晰**的小模型做候选重排，例如 Qwen3-0.6B / Qwen2.5-0.5B 系列（Apache-2.0）的量化版，或自行蒸馏的 4 层中文 BERT 排序头。输入为「前文 + 应用类型 + 候选列表」，只对前 N 个候选重排，设 15～30 ms 的时间预算，超时则回退到 libime。
- **创新点**：融合 **应用上下文**（UIA 读取的窗口角色，如聊天框、代码编辑器、邮件正文）的候选重排。例如在 IDE 中优先技术词汇、在聊天窗口中优先口语表达、在邮件正文中优先书面语，具体收益需用评测数据验证。
- **可量化指标**：首选命中率（Top-1）、Top-5 命中率、平均键击数（KSPC）、重排延迟 p95。

### 方向 B：拼音直达意图（Pinyin-to-Intent）

- **问题**：现有 Agent 输入法普遍是「先打字成汉字 → 再发给 AI」，而 OwO 可以在**汉字还没上屏时**就理解意图。
- **方案**：对 `vsanzhongtixingwokaihui` 这类拼音串，直接做「意图分类 + 槽位标注（带拼音区间）」。同音歧义（如 `sanzhong` 可能是「三中」或「三钟」）通过槽位候选让用户逐个锁定，正是 v3 协议已经设计好的能力。
- **技术路线**：拼音序列 → 小型序列标注模型（BiLSTM-CRF / 小 Transformer），或规则 + LLM 校正；训练数据可由「汉字指令 → 拼音」自动合成。
- **创新点**：**基于拼音区间的增量槽位锁定交互方法**，可申请发明专利，也可写成人机交互方向的论文。

### 方向 C：候选即命令（Candidate-as-Command）交互范式

- 把候选框从「汉字列表」扩展成「**结构化命令面板**」：同一个候选窗里，文字候选、命令候选和槽位候选统一排序、统一选择键。
- 风险分级是这个范式的核心安全设计：低风险命令可直接选择执行，高风险命令只能「打开可信界面确认」（协议红线已经写好，需要两端都实现）。
- 评测：与「打开聊天窗口 → 输入 → 复制 → 粘贴」的传统流程对比**任务完成时间和操作步数**。

### 方向 D：输入法级隐私安全框架

- 输入法能看到用户几乎所有输入，隐私是 AI 输入法最大的社会顾虑，也是**很好的创新叙事**。
- 内容：敏感场景识别（InputScope + UIA + 应用白名单）→ 分级上下文（L0 仅当前输入，L1 当前应用，L2 跨应用）→ 本地脱敏（中文 PII）→ 出境开关 → 全链路审计（Agent 已有 SQLite 审计）。
- 可量化：敏感字段召回率和误报率（自建测试集）、默认配置下的数据出境字节数。

### 方向 E：从「输入习惯」到「操作习惯」的个性化学习

- 输入法已有本地词频学习，Agent 已有示范录制 → 动作图 → 技能包（`learn.rs`），以及静默观察 → 技能挖掘。
- 合并后可以做到：输入法发现用户反复在某应用中输入同类内容（如每天发日报），由 Agent 建议沉淀为技能，下次 `v ribao` 一键生成并上屏。
- 注意：必须在本地完成，并通过方向 D 的隐私框架约束。

### 方向 F：多模态输入（语音 / 截图 OCR 融入输入法）

- 语音：SenseVoice 本地识别结果进入候选框，可与拼音混合纠错（例如语音给出候选，拼音首字母筛选）。
- OCR：截图识别 → 进入候选 → 选择上屏或翻译后上屏。

> **优先级建议**：大创周期有限，建议 **A + B + C 作为主线**（对应「智能输入法」与「Agent 融合」两大创新），**D 作为贯穿全程的安全保障**，E、F 作为加分展示项。

---

## 6. 如何体现大创的创新作用

### 6.1 创新点提炼（答辩与申报书可直接使用）

| # | 创新点 | 一句话表述 | 现有基础 | 待补工作 |
|---|---|---|---|---|
| 1 | 拼音区间槽位交互 | 在汉字上屏之前，把拼音串直接解析为带原始拼音区间的任务槽位，支持逐槽锁定和局部重算 | 协议 v3 与 TSF 端已实现 | Agent 端 IntentParser + 评测 |
| 2 | 候选即命令 + 风险分级 | 把输入法候选框扩展为带风险分级的结构化命令面板，高风险操作强制转入可信界面 | 协议与两端部分实现 | 输出动作（v4）、可信界面闭环 |
| 3 | 场景感知神经候选 | 融合应用场景和前文语义的本地小模型候选重排，有严格的时延预算和降级 | ModelHost / ONNX 链路已通 | 选型、训练或蒸馏、AB 评测 |
| 4 | 输入法级隐私框架 | 敏感场景识别 + 分级上下文 + 中文 PII 本地脱敏 + 出境控制 + 审计 | 部分实现 | 统一判定、测试集、量化评估 |
| 5 | 进程隔离的插件与 Agent 安全架构 | TSF 极简、模型/插件/Agent 全部进程外、AppContainer 沙箱、逐版本授权 | 已较完整 | 整理成架构论文或软著材料 |

### 6.2 评测方案（创新必须有数据支撑）

建立 `eval/` 目录，让每一个创新点都有**对照实验**：

| 评测项 | 数据 | 指标 | 对照组 |
|---|---|---|---|
| 候选质量 | 自建 2,000～5,000 条分场景语料（聊天、文档、代码注释），以及公开语料的测试集切分 | Top-1、Top-5、KSPC、MRR | libime 基线、微软拼音（人工记录） |
| 候选延迟 | 仓库现有 benchmarks 加模型重排 | p50、p95、p99、首候选时间 | 关闭模型 |
| 意图/槽位 | 自建 300～500 条拼音指令（覆盖 3～5 个意图），人工标注 | 意图准确率、槽位 F1、需锁定次数 | 先转汉字再交给 LLM |
| 端到端任务 | `sim/` 模拟实验台加真实应用，选 10 个典型任务 | 成功率、完成时间、操作步数 | 手动操作、聊天式 Agent |
| 隐私 | 构造含密码、手机号、身份证号等内容的测试文本 300 条 | 敏感字段召回率、误报率、出境字节数 | 现有过滤规则 |
| 用户研究 | 10～20 名同学，使用一周 | SUS 可用性量表、NASA-TLX、主观访谈 | 日常输入法 |

### 6.3 成果形式建议

- **软件著作权**：建议申请两项，分别是「OwO 智能输入法软件」和「OwO 输入法智能体引擎软件」。注意先清理第三方版权素材。
- **论文**：方向 B + C 适合投人机交互类会议的短文或中文期刊（如《中文信息学报》《计算机工程与应用》）；方向 A 可写成「场景感知的输入法候选重排」实验报告。
- **专利**：「一种基于拼音区间的增量任务槽位确认方法」「一种输入法候选框风险分级命令执行方法」。
- **竞赛**：中国大学生计算机设计大赛（软件应用与开发类）、「挑战杯」、中国国际大学生创新大赛。
- **开源影响力**：monorepo 公开后完善 README、演示 GIF 和 Release，用 star 数、下载量、issue 作为社会影响力佐证。

### 6.4 答辩叙事建议

1. **痛点**：用 AI 要「切窗口 → 打字 → 复制 → 粘贴」；输入法是用户最高频的入口，却只会出汉字。
2. **方案**：OwO 让输入法既能出字，也能理解意图、调度工具，并且始终守住隐私和安全底线。
3. **创新**：讲清 6.1 中的 1、2、3 号创新点，每个都配一段 10 秒演示视频和一张数据图。
4. **工程实力**：多进程隔离架构、37 份 ADR、上千个测试、性能基准。
5. **安全与伦理**：隐私框架、风险分级、本地优先，主动回应评审的担忧。
6. **展望**：方向 E、F，以及跨平台计划。

---

## 7. 行动路线（建议按大创周期安排）

### 7.1 里程碑

| 阶段 | 时间（建议） | 目标 | 交付物 |
|---|---|---|---|
| M0 整顿 | 第 1～2 周 | 安全修复 + 仓库合并 + CI | monorepo、四条 CI、P0-6 修复、版权素材清理 |
| M1 闭环 | 第 3～6 周 | 协议 v4 + 上屏输出 + IntentParser 首版（3 个意图） | 可演示的「`v` 指令 → 槽位 → 确认 → 执行或上屏」 |
| M2 智能 | 第 5～10 周 | 场景感知候选重排模型 + 评测集 | 模型 manifest、AB 实验数据 |
| M3 插件 | 第 8～12 周 | OCR、语音、润色翻译插件（复用 Agent 能力） | 3 个 `.owopkg` 官方插件 |
| M4 评测与成果 | 第 10～16 周 | 全量评测、用户研究、论文、软著、专利初稿 | 评测报告、论文初稿、软著材料 |

### 7.2 近期待办（按优先级排序，负责人待分配）

| # | 优先级 | 事项 | 涉及位置 |
|---|---|---|---|
| 1 | P0 | 取消公开的 `/auth/token`，token 改由 ACL 文件或 Tauri IPC 下发；SSE 加鉴权 | `agent/…/owo-agent-server/src/auth_token.rs`、`lib.rs` |
| 2 | P0 | `egress.cloud_enabled` 默认改为 `false`，首次使用时明确授权 | `owo-agent-core/src/settings.rs` |
| 3 | P0 | `serve-ime` 路径禁止 AutoApprover，并加断言测试 | `owo-agent-cli/src/main.rs` |
| 4 | P0 | Agent 的 `sensitive_input` 改用 InputScope 判定；跨应用上下文默认关闭 | `ime/src/tsf/text_service.cpp`、`input_method_context.cpp`、`src/ipc/named_pipe.cpp` |
| 5 | P0 | 补充中文 PII 脱敏规则（手机号、身份证、密码/验证码关键字）并加测试 | `ime/src/agent/agent_protocol.cpp` |
| 6 | P0 | 移除第三方角色皮肤和仓库中的 exe/zip 二进制 | desktop 与 engine 分支 |
| 7 | P0 | 设计并实现协议 v4 的 `commit_text` / `replace_selection` | `protocol/`、TSF、`owo-agent-ime` |
| 8 | P0 | Agent 端 IntentParser 首版（提醒、查找文件、翻译润色），实现 `CommitTask` | `owo-agent-ime/src/bridge.rs` |
| 9 | P1 | 建立 `protocol/` 单一 Schema 与 golden 测试，两端 CI 同时校验 | 新目录 |
| 10 | P1 | 从 `text_service.cpp` 抽出 Agent 状态机，从 `named_pipe.cpp` 抽出 AgentRouter | `ime/src/tsf`、`ime/src/core` |
| 11 | P1 | 为 OwO 添加 Windows CI（CMake + CTest） | `.github/workflows/windows-ime.yml` |
| 12 | P1 | 用 Cargo feature 冻结 fleet、cloud_exec、team、goal、workflow、market 等模块 | `agent/Cargo.toml` |
| 13 | P1 | 合并两套桌面前端，归档第三套拼音引擎和 Rime 方案 | desktop 与 engine 分支 |
| 14 | P2 | 清理根目录残留文件，重写 `项目计划.md` 为真实路线图，整理 config-schema 文档 | 仓库根目录、`docs/` |
| 15 | P2 | Agent 模式增加可配置触发键和 UI 提示 | TSF、设置中心 |

---

## 附录 A：关键证据索引

| 结论 | 位置 |
|---|---|
| IME 中模型状态（RoBERTa 许可阻塞、合成 ONNX） | `docs/architecture/p3-kickoff.md`、`docs/architecture/p3a-acceptance.md`、`docs/adr/0005-*.md` |
| libime N-gram 后端 | `docs/adr/0033-libime-language-model-backend.md`、`src/model/libime_backend.cpp` |
| Agent 请求构造、槽位锁定 | `src/tsf/text_service.cpp`：`queue_agent_action`（约 3162 行）、`apply_agent_task`、`select_agent_command` |
| 敏感输入仅判定 ES_PASSWORD | `src/tsf/text_service.cpp` 约 122～163 行 |
| InputScope 敏感判定（未被 Agent 复用） | `src/tsf/input_method_context.cpp` 约 49～66 行 |
| 跨应用上下文聚合 | `src/ipc/named_pipe.cpp` 约 52～84 行、约 1310 行 |
| 脱敏规则 | `src/agent/agent_protocol.cpp` 约 1065～1114 行 |
| 连接器 full_trust 权限 | `plugins/agent-ipc/manifest.json`、`src/plugin/plugin_host.cpp` 约 651～690 行 |
| 协议「message 只展示」 | `docs/plugins/agent-ipc-integration.md` 第 175、207 行 |
| Agent 端 prompt 直拼、不生成槽位 | `agent-sdk/crates/owo-agent-ime/src/bridge.rs` 约 398 行（`compose_prompt`）、约 160 行（`CommitTask` → `not_implemented`） |
| insert-reply 仅回显 | `agent-sdk/crates/owo-agent-ime/src/commands.rs` 约 68 行、`state.rs` 约 284 行 |
| `/auth/token` 公开返回 token | `agent-sdk/crates/owo-agent-server/src/auth_token.rs` 约 213 行、`lib.rs` 约 275 行 |
| 出境默认开启 | `agent-sdk/crates/owo-agent-core/src/settings.rs` 约 190 行 |
| 二进制入库 | `agent-sdk/dist/OwO-Agent-IME/owo-agent.exe`（65 MB）、`dist/OwO-Agent-IME-0.1.0-debug.zip`（20 MB） |
| 第三套拼音引擎与 Rime 方案 | desktop 分支 `crates/assistant-ime/`、`plugins/lingxi-rime-filter/`、`docs/weasel-integration.md` |
| 原集成计划验收用例 | desktop 分支 `docs/ime-agent-integration-plan.md` E1.8 |

## 附录 B：术语

- **TSF**：Windows Text Services Framework，系统级输入法框架。
- **槽位（Slot）**：任务中的参数，如提醒任务的「时间」「事项」。
- **拼音区间（PinyinRange）**：槽位对应原始拼音串中的 `[start, end)` 位置，用于局部修改和锁定。
- **可信界面**：输入法以外、由 Agent 引擎控制的确认窗口（桌宠审批卡片或工作台），用于确认高风险操作。
- **出境（Egress）**：数据离开本机、发往云端服务。
