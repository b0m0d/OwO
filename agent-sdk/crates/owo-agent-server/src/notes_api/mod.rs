//! 笔记 HTTP API（Lane A，第四轮 HTTP/UI 集成轮）。
//!
//! 只新建本文件；禁止修改任何既有文件（lib.rs/Cargo.toml/core 等由主控统一收尾）。
//! 独立编译约束：本模块不使用 `crate::`/`super::`；引用 server 类型一律写全限定名
//! `owo_agent_server::AppState`，保证测试能以 `#[path = "../src/notes_api.rs"] mod notes_api;`
//! 方式独立编译。
//!
//! 存储：按 `AppState.data_root` 键控的模块内单例注册表（不允许给 AppState 加字段）。
//! `<data_root>/notes/<id>/doc.json`（save_doc/load_doc）+ `index.json` 清单 +
//! `<id>/fts.db`（每文档 FTS5 索引，写操作后重索引该文档；搜索遍历合并）。
//! 写操作一律留审计（复用 owo_agent_core::AuditLog，经 `state.agent.audit_log()`）。

mod handlers;
mod store;
mod support;

pub use handlers::*;
