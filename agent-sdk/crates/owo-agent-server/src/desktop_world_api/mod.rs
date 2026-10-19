//! DesktopWorld / WorldModel HTTP 面（Part 5 R1，主文档 §8.5、§5.11、§5.12、§9.0 T0 遗留）。
//!
//! 把 E0（desktop_env）/T0（transition + dataset_builder）/WM0（world_model）经 HTTP
//! 对外接通，形成可被真实调用方消费的闭环：
//!
//! 环境面（/desktop-envs，S1 可编程环境；写路径 lease fencing）：
//! - `POST /desktop-envs`                    创建环境（+自动授予初始写租约，返回凭证与首帧观测）
//! - `POST /desktop-envs/{id}/reset`         按新任务种子复位（写路径；§8.5）
//! - `POST /desktop-envs/{id}/observe`       只读观测（读路径，不要求租约；§8.5）
//! - `POST /desktop-envs/{id}/step`          执行一步：影子预测 → 执行 → 预测/实际对照 →
//!   transition 落盘（写路径；§8.5，闭环粘合点）
//! - `POST /desktop-envs/{id}/snapshot`      保存快照（读路径；§8.5）
//! - `POST /desktop-envs/{id}/restore`       恢复快照（写路径；§8.5）
//! - `POST /desktop-envs/{id}/judge`         程序化判分（读路径；§8.5）
//! - `POST /desktop-envs/{id}/inject-fault`  故障注入（写路径；E0 DesktopEnv 既有能力，
//!   支撑失败轨迹 → fork 点 → 失败样本数据集）
//! - `POST /desktop-envs/{id}/lease`         租约 acquire/renew/release（fencing 运维；
//!   长循环调用方需要续租，§4.2 单写者纪律）
//!
//! 模型面（/world-model，§8.5）：
//! - `POST /world-model/predict`             单步结构化预测（active provider；
//!   无规则时显式确定性回退，不伪造预测）
//! - `GET  /world-model/providers`           provider 与候选健康（影子样本 + 校准聚合）
//!
//! 数据面（§8.5 / §5.12.3 数据飞轮）：
//! - `GET  /transitions/{id}`                单条 transition（预测 vs 实际对照，训练数据溯源）
//! - `POST /datasets/build`                  触发 DatasetBuilder（顺序清洗+去重+平衡+清单）
//! - `GET  /datasets/{id}/manifest`          数据集清单
//!
//! 模型治理（§5.12.4 晋升门控）：
//! - `POST /model-candidates`                注册新候选（shadow 起步；不得静默成为 active）
//! - `POST /model-candidates/{id}/promote`   显式 ack + 理由 + 影子样本齐备才允许晋升
//!
//! 数据集持久化（第二路）：
//! - `datasets/` 目录在 [`DesktopWorldHub::new`] 时全量扫描重建索引；
//! - 每个 `.json` 都按「文件名 ↔ dataset_id、结构、计数一致性、content_hash」完整校验；
//!   任何损坏文件都让初始化**带具体路径 fail-fast**，绝不静默忽略；
//! - `GET /datasets/{id}/manifest` 以持久化数据为权威（读盘 + 校验），不再只信进程内 HashMap。
//!
//! 候选治理（第二路，修订 R1 遗留的伪影子样本问题）：
//! - 候选必须声明 provider 身份（[`CandidateProviderRef`]）；缺省为 metadata_only；
//! - 只有真实接线了可执行 provider 的 shadow 候选才会通过并行预测积累影子样本
//!   （[`DesktopWorldHub::wire_candidate_executor`]）；无 provider 的候选**零样本**；
//! - 从 transition 语料克隆 active 规则模型「代跑」生成独立评估的行为已移除——
//!   那类样本只是复制 active 的输出（伪影），不可作为晋升依据；
//! - 晋升门槛 = 有真实接线 provider + 样本数达标 + 校准摘要 + 相对上一任 active
//!   关键指标无超阈值退化 + 调用方显式 ack 与理由；当前尚无 WM1 真实调用面，
//!   未接线/伪声明的候选一律明确拒绝晋升（显式报错，不伪造已验证状态）。
//!
//! 运行态：模块内 `OnceLock` 单例 [`DesktopWorldHub`]（进程内环境注册表、transition 日志
//! （`data_root/desktop_world/transitions.jsonl`，崩溃重放幂等）、provider/候选登记、数据集
//! 清单（启动时从磁盘恢复）、经验接线）。**不给 AppState 加字段**。
//!
//! 协议约束（与 fleet_api 一致）：本模块不引用 `crate::`/`super::`；`AppState` 全限定名
//! `owo_agent_server::AppState`；错误统一 `(StatusCode, Json({error}))`。
//! 安全：写路径（step/reset/restore/inject-fault/lease 变更）必须携带与当前租约完全匹配的
//! `LeaseProof`，不匹配即 409 fencing；预测永不参与、不覆盖动作决策（§4.2 预测不是事实）。

// dataset_builder 属 ProductEval 开发工具包（M1 起位于 devtools/product-eval），
// 经 owo-agent-eval-facade 暴露；core 自身不再持有它。

mod handlers;
mod hub;

pub use handlers::*;
pub use hub::*;
