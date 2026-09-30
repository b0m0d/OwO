// R13:fleet_api 第二阶段（真实远端节点协议闭环），待主控同步 OpenAPI
//! 控制面 HTTP 契约（P2 双节点网格）：节点注册/心跳续租、任务提交/查询/取消/SSE、
//! 审批响应，以及**真实远端节点协议**（领取/进度/证据/结果/取消确认/fencing）。
//!
//! 路由（前缀 /fleet，已在 `lib.rs::build_router` 挂载 `fleet_api::router(state)`）：
//! - `POST /fleet/nodes/register`           节点注册（CapabilityCard + 心跳续租，返回 lease_token/epoch）
//! - `GET  /fleet/nodes`                    节点列表（NodeStatus 快照）
//! - `POST /fleet/nodes/{id}/heartbeat`     节点心跳续租（旧 token 被拒；R13 新增）
//! - `GET  /fleet/nodes/{id}/tasks`         节点可领取/已领取任务（按自身 node_id 匹配；R13 新增）
//! - `POST /fleet/tasks/submit`             任务提交（`Idempotency-Key` 头幂等）
//! - `GET  /fleet/tasks/{id}`               任务状态 + 事件
//! - `POST /fleet/tasks/{id}/claim`         节点领取匹配任务（fencing 校验；R13 新增）
//! - `POST /fleet/tasks/{id}/progress`      节点回传进度 + 结构化证据（R13 新增）
//! - `POST /fleet/tasks/{id}/result`        节点回传成功/失败结果（R13 新增）
//! - `POST /fleet/tasks/{id}/cancel-ack`    节点确认取消（R13 新增）
//! - `POST /fleet/tasks/{id}/cancel`        取消任务
//! - `GET  /fleet/tasks/{id}/events`        SSE（历史重放 + 实时；`?format=json` 拉全量）
//! - `POST /fleet/approvals/{id}/respond`   审批响应（影响预览 + 结构化证据齐备才批准）
//!
//! 运行态：模块内 `OnceLock` 单例 [`FleetHub`]（进程内 [`InMemoryTransport`] 承载任务状态、
//! [`LeaseManager`] 节点租约/fencing、[`AgentBus`]+[`BusStore`] 节点/任务/违规审计持久化、
//! [`CasStore`] 产物、[`ExperienceStore`] 节点状态变迁、`claims` 领取所有权登记）。
//! **R13 起控制面不再在本进程中"伪执行完成"任务**：任务提交后只进入 Running（等待领取），
//! 状态推进仅由显式节点领取（claim）+ 结果回传（result）驱动。
//!
//! 协议约束：本模块不引用 `crate::`/`super::`；`AppState` 全限定名 `owo_agent_server::AppState`；
//! 错误统一 `(StatusCode, Json({error}))`；不给 AppState 加字段（状态在模块内）。
//! 安全：所有节点写操作经 [`LeaseManager::verify_write`] fencing（token + epoch）校验；
//! 越权/过期 epoch/不匹配节点被拒绝并留审计（bus_store + experience）。协议不携带模型凭据。

mod handlers;
mod hub;

pub use handlers::*;
pub use hub::*;
