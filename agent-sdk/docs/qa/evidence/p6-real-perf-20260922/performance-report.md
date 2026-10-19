# Agent SDK 性能报告

- 状态：`task_coverage_incomplete`；总 trace：60；每项任务最低样本：20
- 生成时间：2026-09-22T05:38:31.3904133Z
- 机器：ASUSROG / Microsoft Windows 11 企业版 / X64
- 源码：`fc8251d42aa192715fa9f3208585802fa87d88ef`；dirty：True
- Provider：https://open.bigmodel.cn/api/paas/v4（source_default）；模型：glm-5.3-flash
- token：prompt 217010 / completion 11763 / total 228773；成本未在 trace 中记录

## 固定任务

| 任务 | 样本 | P50 (ms) | P95 (ms) | 状态 |
|---|---:|---:|---:|---|
| 启动 Daemon 并创建会话 (`daemon_start_session`) | 0 | — | — | not_measured |
| 纯文本短对话（首 token） (`short_text_conversation`) | 20 | 7080 | 10741 | measured |
| 读取 100 KB 文件 (`read_100kb_file`) | 20 | 11794 | 15899 | measured |
| 搜索 1,000 文件工作区 (`search_1000_files`) | 20 | 11456 | 20201 | measured |
| 写入小文件并生成 diff (`write_file_and_diff`) | 0 | — | — | not_measured |
| 一次需要审批的命令 (`approval_command`) | 0 | — | — | not_measured |
| 坏 MCP 存在时启动并对话 (`invalid_mcp_startup`) | 0 | — | — | not_measured |
| 客户端断开、重连与取消 (`disconnect_reconnect_cancel`) | 0 | — | — | not_measured |

## 汇总分布

| 指标 | P50 (ms) | P95 (ms) | 最大值 (ms) |
|---|---:|---:|---:|
| Turn duration | 11173 | 18376 | 34452 |

| 阶段 | 样本 | elapsed P50 | elapsed P95 | 首 token P50 | 首 token P95 |
|---|---:|---:|---:|---:|---:|
| model | 101 | 6139 | 10077 | 5291 | 9243 |
| persistence | 60 | 0 | 0 | 0 | 0 |
| tool | 44 | 1 | 28 | 0 | 0 |

## 口径与限制

- 只有带 `performance_task` 固定任务标签的 trace 才计入任务行；未标记 trace 只进入总分布，不推断任务归属。
- Core trace writer 仅在 Daemon 以 allowlisted `OWO_PERF_TASK_ID` 启动时写标签；固定任务门仍需逐项受控采集至少 20 次真实任务。
- 本报告只汇总 turn traces；Daemon/Desktop 冷启动、HTTP 首 token 到 UI 显示、审批提交恢复、SSE 队列内存曲线等需独立探针。
- Trace 不含 Provider 单价，因此只列 token 数，不估算成本。

## 原始 trace

- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\00990ae3-951d-4c95-a7cd-f37b4b1055ee-1790052234639.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\10a60e23-7d74-4285-8eb9-acfd6266ef0e-1790055220841.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\11facfde-696d-4dab-95de-664498ac7607-1790055418240.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\126693d4-7ab3-4ece-b4fd-4bd17be17d06-1790051857830.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\13231c52-2279-4d99-901b-bd3fe5e8ec58-1790052020883.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\141ecf70-757e-4f5e-9308-880b34b4aa56-1790051813748.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\15e41440-1d7c-4e7f-8e3e-ab0c7e36f6f5-1790052197202.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\16661269-b0bc-45db-a87e-c2866cd5621a-1790055370052.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\18f8c4fa-5916-4366-8fa9-95266e88fb06-1790051824745.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\1b54344d-2895-4920-9efe-0c79be0d6fa9-1790055314981.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\20e5cc17-e682-4984-aec1-a78c52fa792d-1790052091641.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\22b4247e-799c-42b8-9f5a-849db1c85b78-1790052246083.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\25304e6e-e2d3-42d5-a54b-e11d5284f8d6-1790055280361.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\280fed75-5db4-4a1c-a981-dc2fc90618d2-1790055442195.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\2ac065f1-580b-4f48-b2f8-0fe84fceaa60-1790052257841.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\2bbb3bb3-1998-45ea-9e55-3da927c6bf90-1790051843300.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\2e8a1750-0a5a-4d3d-a0c9-88e3145894f9-1790051899151.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\2f253187-138e-4be8-9a9c-342137a4854f-1790051776513.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\2fedf537-60bb-42ad-a2d1-06f26dfe93fa-1790055406107.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\335a1d79-c795-4076-b725-7b0432c8cf9f-1790052145054.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\386941df-4252-4eab-9e16-012806229e1c-1790052210043.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\3ba2186b-5942-4a3d-b3b3-788939b428cc-1790052067682.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\3bd6e299-0fa8-4b7e-ae1c-3877dfe6ea40-1790055291527.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\3d7cd270-6436-4f4b-93a3-cdf881e97367-1790052188588.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\44d47f9b-803b-412e-9b6c-d3689bab27aa-1790055358191.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\455480d0-c8b3-410e-a096-cd2286162449-1790055177836.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\4995bacc-ce04-4cc6-b688-eb13f7b338b5-1790051866051.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\5124016e-c02c-427a-ac47-6a2d7ae061fe-1790051882425.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\516f1bca-1bd2-4215-882b-d3a61008bd61-1790052133245.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\524a244d-227b-4428-a775-c4adcccccf6d-1790052033533.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\5a931ace-af21-4785-86fe-5e57099b86b7-1790055234489.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\5ab054e2-ae13-4f4d-9dde-5216be954d97-1790051930022.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\6cfd952c-fa91-4fc5-84e0-39ae59985127-1790051888724.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\6fae2f83-e6fd-4e73-b8a1-a7faf875ca62-1790051806652.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\6feaa839-faca-4ee4-864a-2ed34eec68b9-1790051872167.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\72a31d6c-c611-44cf-aad0-142cc7d73807-1790051792026.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\773b4543-f8fb-4733-9f39-1c97a3824448-1790051924324.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\78879f0f-7e60-4c3c-8d59-7b0a90667ee2-1790051784524.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\8359fac5-15d2-4e03-9c9d-e8cf0e09a863-1790055326703.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\94e8ac21-b46b-4115-b739-517c1878c789-1790055398521.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\9f56e4ee-7bfd-4b30-a526-a18d13245ac3-1790051849223.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\a0181db6-0bff-4d72-96de-ccc6c5455f8a-1790055335472.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\ade0f7ff-0282-43b8-a34b-ce53052063e3-1790052121014.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\bdc072b6-943f-45c0-aeff-75f09b29effa-1790052058258.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\c0899770-4fdb-4f23-820d-7f2738908d77-1790051799395.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\c4e07795-80d9-4c71-8729-a5eeb9788001-1790055453914.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\d0c09571-b37a-43a4-8793-d56bf3e5ea8e-1790052175817.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\d6cd427c-673f-4f05-a84e-6ed6b53d0d12-1790051837349.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\d70f2502-3b1f-456e-9c7d-8d04723ba21b-1790051913299.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\d7fed153-1f34-493c-98d4-47213a44c755-1790052045359.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\ddf5387a-5047-4848-a812-d697ba280377-1790055430799.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\df0a2939-4d83-42a2-8d2a-774a9d6a252a-1790051907120.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\e7fe8d91-516c-4dac-a6fe-750356fce679-1790055346405.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\eb3e5614-69c5-47ab-9d13-beb13532399a-1790052079560.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\ece509aa-7058-4c10-9aca-321420dcdf26-1790055269218.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\f34c6255-0982-42f4-8558-a9445aee8a2b-1790052156930.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\f641ca05-5b62-4c32-ba53-3eddcb003710-1790052220962.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\f66107bf-5c2f-46e9-a4dd-fd0801563cd6-1790055303060.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\f6fb3ac2-c2a3-4832-a272-d1daf61c8cd7-1790055378051.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\fbeb963b-c2f9-4491-aa25-225028193698-1790052107831.json`
