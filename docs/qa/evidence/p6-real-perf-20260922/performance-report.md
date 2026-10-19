# Agent SDK 性能报告

- 状态：`complete`；总 trace：160；每项任务最低样本：20
- 生成时间：2026-09-22T10:08:50.1857198Z
- 机器：ASUSROG / Microsoft Windows 11 企业版 / X64
- 源码：`fc8251d42aa192715fa9f3208585802fa87d88ef`；dirty：True
- Provider：https://open.bigmodel.cn/api/paas/v4（source_default）；模型：glm-5.3-flash
- token：prompt 383381 / completion 40462 / total 423843；成本未在 trace 中记录

## 固定任务

| 任务 | 样本 | P50 (ms) | P95 (ms) | 状态 |
|---|---:|---:|---:|---|
| 启动 Daemon 并创建会话 (`daemon_start_session`) | 20 | 4452 | 4854 | measured |
| 纯文本短对话（首 token） (`short_text_conversation`) | 20 | 7080 | 10741 | measured |
| 读取 100 KB 文件 (`read_100kb_file`) | 20 | 11794 | 15899 | measured |
| 搜索 1,000 文件工作区 (`search_1000_files`) | 20 | 11456 | 20201 | measured |
| 写入小文件并生成 diff (`write_file_and_diff`) | 20 | 11857 | 16253 | measured |
| 一次需要审批的命令 (`approval_command`) | 20 | 34216 | 131105 | measured |
| 坏 MCP 存在时启动并对话 (`invalid_mcp_startup`) | 20 | 4085 | 12718 | measured |
| 客户端断开、重连与取消 (`disconnect_reconnect_cancel`) | 20 | 76 | 90 | measured |

## 汇总分布

| 指标 | P50 (ms) | P95 (ms) | 最大值 (ms) |
|---|---:|---:|---:|
| Turn duration | 9275 | 39623 | 164754 |

| 阶段 | 样本 | elapsed P50 | elapsed P95 | 首 token P50 | 首 token P95 |
|---|---:|---:|---:|---:|---:|
| approval | 140 | 0 | 25 | 0 | 0 |
| model | 297 | 5310 | 18300 | 4844 | 15857 |
| persistence | 140 | 0 | 0 | 0 | 0 |
| tool | 170 | 23 | 44 | 0 | 0 |

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
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-01-975b712a-a639-466c-9809-ea3f2812862a-1790068069045.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-02-6523a59c-f9e6-4b59-8cac-8f00c2240144-1790068122651.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-03-c3144a7c-b462-498d-ab4b-21e06aaef5a1-1790068165051.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-04-d4e7f5d7-3a72-43cd-8457-4d020cd5f07b-1790068214972.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-05-aacfe98f-6242-4d88-8da7-7019f84eebf8-1790068259468.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-06-f03bcbb4-089c-47f2-b005-e7a3078fa306-1790068293463.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-07-673632b6-cfcd-49ae-b075-19a06149ad87-1790068396386.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-08-f219a330-eda7-4b4e-999b-26d807066587-1790068433797.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-09-bcc77b05-be27-435b-b4ad-2651016c2a67-1790068465576.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-10-f8eb405b-4730-4f52-b1e7-009dcf44d68a-1790068606982.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-11-5f5aa58a-e2ba-4e0a-ae34-c14ad4afd8bf-1790068637252.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-12-d2519d5d-84a1-4d35-b387-b6c62740954b-1790068673602.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-13-91b241b8-259c-463e-834f-d9c6cf9b7c52-1790068727831.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-14-629f0c15-ce56-439d-b7dc-af9f235b9a1b-1790068764943.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-15-0439e8f8-60c6-4cb8-9308-07e2f0f53b0f-1790068807414.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-16-668da49c-3db6-4a4d-a155-5c4c4d7b8d52-1790068865010.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-17-5a5df32f-60a9-4a4b-9707-055e3138f5c2-1790069040051.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-18-49b767c6-8a2a-48d6-9944-c62975bffb8c-1790069133581.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-19-f2502729-ea12-4dd3-89e4-8426ffb5854b-1790069172764.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\approval-command-run-20-82dd4497-f39d-4b73-a0ea-9bef3cecef2b-1790069218295.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\bdc072b6-943f-45c0-aeff-75f09b29effa-1790052058258.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\c0899770-4fdb-4f23-820d-7f2738908d77-1790051799395.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\c4e07795-80d9-4c71-8729-a5eeb9788001-1790055453914.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\d0c09571-b37a-43a4-8793-d56bf3e5ea8e-1790052175817.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\d6cd427c-673f-4f05-a84e-6ed6b53d0d12-1790051837349.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\d70f2502-3b1f-456e-9c7d-8d04723ba21b-1790051913299.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\d7fed153-1f34-493c-98d4-47213a44c755-1790052045359.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-01-ed591784-09ce-4337-b39f-cd143b6d45b3-1790055831498.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-02-33a71938-a598-49c9-a96d-e8410aed0b55-1790055846147.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-03-6c46d3f2-ce85-422b-b23a-af032a5dc4b5-1790055860560.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-04-de66e3ef-014e-43fa-a007-121888a39709-1790055875314.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-05-027c13e7-b709-4221-995f-d69998b6a245-1790055890312.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-06-952c05e7-87d5-41c8-a328-31afbb126228-1790055905217.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-07-1a53a02a-7064-4790-b3b4-bcb6d6f7550b-1790055919673.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-08-1b920bc5-e1b2-417d-b88b-c5eedc34235f-1790055934243.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-09-c1e2ed8c-6426-4f0d-bae8-b64729cdc428-1790055949032.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-10-72380b56-5582-4bfd-87f1-8ca1791b6460-1790055963689.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-11-1f195812-5f1f-4753-8921-0e854597e522-1790055977861.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-12-06690ee9-c640-4f87-abbb-eb7b9a204b75-1790055992664.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-13-9070682e-b017-4760-ac01-21ee568ed13a-1790056003933.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-14-255b1618-1d07-4532-9c22-7d504470d3e1-1790056018716.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-15-337c55f8-ba00-413c-b648-c397530e2180-1790056033620.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-16-a902605b-aaab-4fce-b76b-30e304d2e0fc-1790056048974.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-17-7ac838cc-9166-4db1-b4c8-9a6610c9eb5a-1790056063273.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-18-edb2a3eb-15c7-4bcb-9e79-0877e4a63ccd-1790056078055.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-19-27cdfacb-9679-4eb9-8eb3-3d41d0fb7548-1790056093217.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\daemon-start-run-20-73b5975f-58ee-4a0d-af4d-e07c364db470-1790056107956.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\ddf5387a-5047-4848-a812-d697ba280377-1790055430799.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\df0a2939-4d83-42a2-8d2a-774a9d6a252a-1790051907120.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-01-59366873-e3ef-4894-8855-4a4b336e5fa7-1790071492797.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-02-c544ebb9-ed81-4957-b44b-96291b002430-1790071503375.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-03-e1e9a6ee-4a5c-4db9-946d-b3b9e8d25c91-1790071513999.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-04-f1963ded-62c8-48f0-a8e8-ab74dfec6303-1790071524549.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-05-452113d7-fe6e-44ae-93a8-1c0a2b43b411-1790071535167.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-06-f5865b13-69fb-438b-816f-0401b9a05c88-1790071545734.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-07-222749bf-20f7-4c00-a112-46bd6e011b8a-1790071556307.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-08-b3291b23-229a-47ad-a1ab-abd7e9f61988-1790071566937.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-09-b4c8a175-867f-46a1-8e83-ce99c659669d-1790071577519.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-10-5eb23cc7-a462-4b26-9bac-3bc11bd303d5-1790071588102.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-11-8ec60d45-605a-4c53-9c9e-3f21063cc1b3-1790071598650.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-12-c8d87980-1f55-4772-8f72-55a29f9c813f-1790071609243.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-13-5bd5b2c0-5f30-4c4d-a77a-6b235cec5e1b-1790071619895.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-14-918383c6-af6f-4f20-a63f-8a82a23c5ff3-1790071630463.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-15-2f3d9ae1-a3ee-42c9-a056-7a9afafafd29-1790071641051.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-16-133c723c-5d51-4ff1-9f1b-783cfcb598ad-1790071651603.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-17-3ea5c21b-bf98-462b-8284-327e6019fc85-1790071662146.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-18-4adb3433-47d2-48a8-907a-336103192a6f-1790071672669.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-19-1f3a9b77-fdea-4d3e-b881-97e1432e2087-1790071683256.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\disconnect-cancel-run-20-873a4b45-6f3f-4a06-9eab-d62749cd8575-1790071693852.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\e7fe8d91-516c-4dac-a6fe-750356fce679-1790055346405.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\eb3e5614-69c5-47ab-9d13-beb13532399a-1790052079560.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\ece509aa-7058-4c10-9aca-321420dcdf26-1790055269218.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\f34c6255-0982-42f4-8558-a9445aee8a2b-1790052156930.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\f641ca05-5b62-4c32-ba53-3eddcb003710-1790052220962.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\f66107bf-5c2f-46e9-a4dd-fd0801563cd6-1790055303060.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\f6fb3ac2-c2a3-4832-a272-d1daf61c8cd7-1790055378051.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\fbeb963b-c2f9-4491-aa25-225028193698-1790052107831.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-01-5b41195f-b6bb-46ec-9415-a46d0226ea99-1790069257772.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-02-08cf96f8-b014-4b01-9000-449d96480fe3-1790069278814.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-03-7b5c0e03-171b-41d4-bb83-942f951d1654-1790069291211.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-04-9a899b06-88c3-4774-918f-58e8ecc55da5-1790069306628.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-05-96ee56fb-dfc5-446f-9aa1-c569167275f9-1790069321520.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-06-00175ed7-6f4d-49cb-bee8-e5e7828ced06-1790069335813.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-07-204e76dd-4bb2-465b-b897-e559548d58c7-1790069350056.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-08-2111dfb1-0826-4abc-8ee4-f382b0c79aec-1790069365004.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-09-a59e4159-1f15-48c8-bb19-ba52a5176e3f-1790069379194.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-10-c7adc34c-b31a-418b-8c3c-232f54d2f3d6-1790069392253.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-11-f12bdba9-c33a-4d13-8ed8-d06357bbdc7e-1790069420677.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-12-458dc3ab-7a15-4b1b-803c-c45125007abf-1790069435263.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-13-e48062a2-5bc4-4eb8-821b-a1887883f8fd-1790069451205.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-14-6aa56c4f-642f-41cd-a9d8-81b6a8c78b4c-1790069463832.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-15-8f81937d-ad4b-4b19-b92a-0540c7e90ba1-1790069476271.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-16-27102834-4342-4835-98ac-2de96a187df0-1790069491232.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-17-eb6c5368-57a9-405f-992d-09e763c01a2f-1790069514266.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-18-ef85f93d-8aff-45cc-8d2c-1c991e555357-1790069528621.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-19-47d039c4-d87f-4b8e-ac6b-9f00c9efae79-1790069544211.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\invalid-mcp-run-20-a85d5f24-ec3b-4ceb-8303-3bd2b1adef06-1790069558540.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-01-8e886102-ce9a-48f3-9308-b08fdfb08261-1790056448187.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-02-999400c0-fe3d-49d0-b420-d6dc1c73520e-1790056469983.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-03-87b155db-3cbf-4903-80ce-f4b0e853fa46-1790056492024.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-04-f3774963-b65f-47a7-9a84-6490566d7464-1790056514663.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-05-451e2f7f-0477-497c-a5e6-f2aac4754da6-1790056541217.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-06-4d452bca-f65f-4100-b69a-096e8df5fcda-1790056561805.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-07-88a8adb8-e80e-42f1-9508-9da8c6d74b1e-1790056584204.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-08-73b7638b-f3fb-4986-a87c-30421fdea88f-1790056610653.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-09-0be107ac-0fa1-46ef-826f-88c855165688-1790056632876.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-10-c1329f73-bf0e-4bd7-ad38-1de0a7491053-1790056659202.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-11-bc95a323-e052-489e-84f6-d144ee2f795a-1790056683193.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-12-61849665-63cc-4bf1-b087-513c8d29bbf2-1790056704380.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-13-bb63fc78-fce7-4fd8-983a-9007a0fecb52-1790056726599.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-14-e0e10f37-3769-4a10-9427-7a271fb41877-1790056746148.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-15-e03f3e43-9c68-422d-b3d0-5351576494e8-1790056765657.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-16-a86e2d10-4f0c-4053-8a76-4e37669a1b33-1790056794448.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-17-a978b7b7-1b27-42b8-bc47-fa9c1f919e77-1790056816353.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-18-75e221b5-7900-44da-9ad9-a15ac0dd2b28-1790056838508.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-19-09c8b154-a6ab-4ed0-9252-553c26d15fd0-1790056854859.json`
- `T:\创新创业\OwO-master\agent-sdk\docs\qa\evidence\p6-real-perf-20260922\traces\write-file-run-20-134916a3-9dfd-461e-8c57-5850a3ccd60e-1790056875757.json`
