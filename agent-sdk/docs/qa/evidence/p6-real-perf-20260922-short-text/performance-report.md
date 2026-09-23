# Agent SDK 性能报告

- 状态：`task_coverage_incomplete`；总 trace：20；每项任务最低样本：20
- 生成时间：2026-09-22T04:39:00.6128842Z
- 机器：ASUSROG / Microsoft Windows 11 企业版 / X64
- 源码：`fc8251d42aa192715fa9f3208585802fa87d88ef`；dirty：True
- Provider：https://open.bigmodel.cn/api/paas/v4（source_default）；模型：glm-5.3-flash
- token：prompt 14380 / completion 3401 / total 17781；成本未在 trace 中记录

## 固定任务

| 任务 | 样本 | P50 (ms) | P95 (ms) | 状态 |
|---|---:|---:|---:|---|
| 启动 Daemon 并创建会话 (`daemon_start_session`) | 0 | — | — | not_measured |
| 纯文本短对话（首 token） (`short_text_conversation`) | 20 | 7080 | 10741 | measured |
| 读取 100 KB 文件 (`read_100kb_file`) | 0 | — | — | not_measured |
| 搜索 1,000 文件工作区 (`search_1000_files`) | 0 | — | — | not_measured |
| 写入小文件并生成 diff (`write_file_and_diff`) | 0 | — | — | not_measured |
| 一次需要审批的命令 (`approval_command`) | 0 | — | — | not_measured |
| 坏 MCP 存在时启动并对话 (`invalid_mcp_startup`) | 0 | — | — | not_measured |
| 客户端断开、重连与取消 (`disconnect_reconnect_cancel`) | 0 | — | — | not_measured |

## 汇总分布

| 指标 | P50 (ms) | P95 (ms) | 最大值 (ms) |
|---|---:|---:|---:|
| Turn duration | 7080 | 10741 | 12318 |

| 阶段 | 样本 | elapsed P50 | elapsed P95 | 首 token P50 | 首 token P95 |
|---|---:|---:|---:|---:|---:|
| model | 20 | 7078 | 10739 | 5439 | 9719 |
| persistence | 20 | 0 | 0 | 0 | 0 |

## 口径与限制

- 只有带 `performance_task` 固定任务标签的 trace 才计入任务行；未标记 trace 只进入总分布，不推断任务归属。
- Core trace writer 仅在 Daemon 以 allowlisted `OWO_PERF_TASK_ID` 启动时写标签；固定任务门仍需逐项受控采集至少 20 次真实任务。
- 本报告只汇总 turn traces；Daemon/Desktop 冷启动、HTTP 首 token 到 UI 显示、审批提交恢复、SSE 队列内存曲线等需独立探针。
- Trace 不含 Provider 单价，因此只列 token 数，不估算成本。

## 原始 trace

- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\126693d4-7ab3-4ece-b4fd-4bd17be17d06-1790051857830.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\141ecf70-757e-4f5e-9308-880b34b4aa56-1790051813748.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\18f8c4fa-5916-4366-8fa9-95266e88fb06-1790051824745.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\2bbb3bb3-1998-45ea-9e55-3da927c6bf90-1790051843300.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\2e8a1750-0a5a-4d3d-a0c9-88e3145894f9-1790051899151.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\2f253187-138e-4be8-9a9c-342137a4854f-1790051776513.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\4995bacc-ce04-4cc6-b688-eb13f7b338b5-1790051866051.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\5124016e-c02c-427a-ac47-6a2d7ae061fe-1790051882425.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\5ab054e2-ae13-4f4d-9dde-5216be954d97-1790051930022.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\6cfd952c-fa91-4fc5-84e0-39ae59985127-1790051888724.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\6fae2f83-e6fd-4e73-b8a1-a7faf875ca62-1790051806652.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\6feaa839-faca-4ee4-864a-2ed34eec68b9-1790051872167.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\72a31d6c-c611-44cf-aad0-142cc7d73807-1790051792026.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\773b4543-f8fb-4733-9f39-1c97a3824448-1790051924324.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\78879f0f-7e60-4c3c-8d59-7b0a90667ee2-1790051784524.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\9f56e4ee-7bfd-4b30-a526-a18d13245ac3-1790051849223.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\c0899770-4fdb-4f23-820d-7f2738908d77-1790051799395.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\d6cd427c-673f-4f05-a84e-6ed6b53d0d12-1790051837349.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\d70f2502-3b1f-456e-9c7d-8d04723ba21b-1790051913299.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922-short-text\data\traces\df0a2939-4d83-42a2-8d2a-774a9d6a252a-1790051907120.json`
