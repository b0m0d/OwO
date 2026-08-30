//! Artifact 下载交付 HTTP 路由（七期 · 第三路：Artifact 校验、证据链与下载交付）。
//!
//! 路由面（经 [`crate::artifact_review_api::router`] 合并挂载）：
//! - `GET /artifacts/{id}/content`：产物正文交付。默认 JSON 信封（冻结契约：
//!   `artifact_id/format/sha256/size_bytes/content`）；`?raw=true` 时返回原始字节
//!   下载流（`Content-Type: <media_type>; charset=utf-8` +
//!   `Content-Disposition: attachment; filename="<file_name>"`）；
//! - `GET /artifacts/{id}/metadata`：交付元数据（冻结契约：`artifact_id/team_id/
//!   kind/format/version/sha256/size_bytes/validation/evidence_refs`，可选 `handoff`
//!   ——Worker 交接原文，仅提供时出现）；
//! - `GET /projects/{id}/delivery-manifest`：最终交付清单（冻结契约：
//!   `project_id/generated_at/manifest[{artifact_id,kind,format,version,sha256,
//!   size_bytes,approved,content_url}]`；每 kind 取最高版本，评审结论（kind=review）
//!   为过程产物不入清单；`content_url` 为 content 端点相对路径）。
//!
//! 契约要点：
//! - 哈希/字节数解析：七期 additive 字段优先（`sha256`/`size_bytes`），legacy 记录
//!   回退 `content_ref`（`cas://sha256:{hash}`）与 CAS 实际字节数；
//! - metadata `team_id`：additive 字段优先，legacy 记录回退 artifact_id 首段
//!   （artifact_id 形如 `{team_id}:{role}:v{n}`）；
//! - 校验结果 `validation`（`{format, valid, reason?}`）与证据引用 `evidence_refs[]`
//!   原样透出（登记前门控产物，七期第三路 core 侧产出）；
//! - 未知产物 / 未知项目 / CAS 内容缺失 → 404。
//!
//! 存储：复用 WorkSwarm 的 `space.db` 与 `cas/`（与 TeamCoordinator 同一物理目录）。

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use owo_agent_core::project_space_store::{ProjectSpaceStoreBackend, ProjectSpaceStoreError};
use owo_agent_core::CasStore;
use owo_agent_protocol::{Artifact, ReviewState};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use super::{store_error, store_error_msg};
use crate::AppState;

// ---------------------------------------------------------------------------
// 公共辅助
// ---------------------------------------------------------------------------

/// CAS 内容库目录（与 TeamCoordinator 同一物理目录：`data_root/workswarm/cas`）。
fn cas_store(state: &AppState) -> Result<CasStore, (StatusCode, Json<Value>)> {
    CasStore::new(state.data_root.join("workswarm").join("cas"))
        .map_err(|e| store_error_msg(format!("CAS 内容库不可用：{e}")))
}

/// 产物 CAS 哈希（七期 additive 字段优先；legacy 记录回退 content_ref 引用解析）。
fn artifact_hash(artifact: &Artifact) -> String {
    if !artifact.sha256.is_empty() {
        artifact.sha256.clone()
    } else {
        artifact
            .content_ref
            .strip_prefix("cas://sha256:")
            .map(str::to_string)
            .unwrap_or_default()
    }
}

/// 产物字节数（七期 additive 字段优先；legacy 记录由 CAS 实际内容回退计算）。
fn artifact_size(artifact: &Artifact, cas: &CasStore, hash: &str) -> u64 {
    if artifact.size_bytes > 0 {
        return artifact.size_bytes;
    }
    cas.get(hash).map(|b| b.len() as u64).unwrap_or(0)
}

/// 产物 lookup + 404 映射（三个端点共用）。
async fn load_artifact(
    store: &owo_agent_core::project_space_store::SqliteProjectSpaceStore,
    artifact_id: &str,
) -> Result<Artifact, (StatusCode, Json<Value>)> {
    store.get_artifact(artifact_id).await.map_err(|e| match e {
        ProjectSpaceStoreError::NotFound(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("产物不存在：{artifact_id}") })),
        ),
        other => store_error(other),
    })
}

// ---------------------------------------------------------------------------
// GET /artifacts/{id}/content
// ---------------------------------------------------------------------------

/// content 端点查询参数。
/// 四路集成注：经 artifact_review_api::router 以 `get(artifact_delivery::artifact_content)`
/// 挂载，Handler 签名中的 Query 类型必须跨模块可见，故 pub(crate)（否则 `type is private` 编译错）。
#[derive(Debug, Deserialize)]
pub(crate) struct ContentQuery {
    /// `true` 时返回原始字节下载流（正确文件名 + media type）。
    #[serde(default)]
    raw: bool,
}

/// GET /artifacts/{id}/content：产物正文交付（JSON 信封；`?raw=true` 原始下载流）。
pub(crate) async fn artifact_content(
    State(state): State<Arc<AppState>>,
    AxumPath(artifact_id): AxumPath<String>,
    Query(query): Query<ContentQuery>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let store = state.artifact_review.store()?;
    let artifact = load_artifact(store.as_ref(), &artifact_id).await?;

    let hash = artifact_hash(&artifact);
    if hash.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!(
                "产物内容引用缺失（artifact={artifact_id}，content_ref={}）",
                artifact.content_ref
            ) })),
        ));
    }
    let cas = cas_store(&state)?;
    let bytes = cas.get(&hash).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("产物内容缺失（CAS 对象 {hash} 不存在）") })),
        )
    })?;
    let size_bytes = artifact_size(&artifact, &cas, &hash);
    let media_type = if artifact.media_type.is_empty() {
        "text/plain"
    } else {
        artifact.media_type.as_str()
    };
    let file_name = if artifact.file_name.is_empty() {
        owo_agent_core::artifact_pipeline::file_name_of(&artifact.kind, &artifact.format)
    } else {
        artifact.file_name.clone()
    };

    if query.raw {
        // 原始下载流：正确 media type + 下载文件名（验收项：按文件名与媒体类型下载）。
        let content_type = HeaderValue::from_str(&format!("{media_type}; charset=utf-8"))
            .unwrap_or(HeaderValue::from_static("text/plain; charset=utf-8"));
        let safe_name = file_name.replace('"', "_");
        let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{safe_name}\""))
            .unwrap_or(HeaderValue::from_static("attachment"));
        let mut response = (StatusCode::OK, bytes).into_response();
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, disposition);
        return Ok(response);
    }

    Ok((
        StatusCode::OK,
        Json(json!({
            // 冻结契约（七期四路冻结，字段名固定）
            "artifact_id": artifact.artifact_id,
            "format": artifact.format,
            "sha256": hash,
            "size_bytes": size_bytes,
            "content": String::from_utf8_lossy(&bytes),
            // additive：交付元数据与证据链（UI 宽容读取）
            "kind": artifact.kind,
            "version": artifact.version,
            "media_type": artifact.media_type,
            "file_name": file_name,
            "evidence_refs": artifact.evidence_refs,
            "open_issues": artifact.open_issues,
            "validation": artifact.validation,
            "handoff": artifact.handoff,
        })),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// GET /artifacts/{id}/metadata
// ---------------------------------------------------------------------------

/// GET /artifacts/{id}/metadata：交付元数据（格式/哈希/字节数/校验/证据链/交接）。
pub(crate) async fn artifact_metadata(
    State(state): State<Arc<AppState>>,
    AxumPath(artifact_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = state.artifact_review.store()?;
    let artifact = load_artifact(store.as_ref(), &artifact_id).await?;
    let cas = cas_store(&state)?;
    let hash = artifact_hash(&artifact);
    let size_bytes = artifact_size(&artifact, &cas, &hash);

    // team_id：七期 additive 字段优先；legacy 记录回退 artifact_id 首段解析。
    let team_id = if artifact.team_id.is_empty() {
        artifact
            .artifact_id
            .split(':')
            .next()
            .unwrap_or_default()
            .to_string()
    } else {
        artifact.team_id.clone()
    };

    // 冻结字段 + additive 元数据；handoff 仅在 Worker 提供交接说明时出现（可选键）。
    let mut body = json!({
        "artifact_id": artifact.artifact_id,
        "team_id": team_id,
        "kind": artifact.kind,
        "format": artifact.format,
        "version": artifact.version,
        "sha256": hash,
        "size_bytes": size_bytes,
        "validation": artifact.validation,
        "evidence_refs": artifact.evidence_refs,
        // additive
        "media_type": artifact.media_type,
        "file_name": artifact.file_name,
        "content_ref": artifact.content_ref,
        "open_issues": artifact.open_issues,
        "review_state": artifact.review_state,
        "supersedes_artifact_id": artifact.supersedes_artifact_id,
        "created_at": artifact.created_at,
    });
    if let Some(handoff) = artifact.handoff {
        body["handoff"] = json!(handoff);
    }
    Ok(Json(body))
}

// ---------------------------------------------------------------------------
// GET /projects/{id}/delivery-manifest
// ---------------------------------------------------------------------------

/// GET /projects/{id}/delivery-manifest：最终交付清单（每 kind 最新版本）。
pub(crate) async fn project_delivery_manifest(
    State(state): State<Arc<AppState>>,
    AxumPath(project_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = state.artifact_review.store()?;
    store
        .get_project_space(&project_id)
        .await
        .map_err(|e| match e {
            ProjectSpaceStoreError::NotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("项目空间不存在：{project_id}") })),
            ),
            other => store_error(other),
        })?;
    let artifacts = store
        .list_artifacts_by_project(&project_id)
        .await
        .map_err(store_error)?;

    // 每 kind 取最高版本（版本链 head；被取代的旧版本不出现在清单）；
    // critic 评审结论（kind=review）是过程产物，不属于交付物。
    let mut latest: HashMap<String, &Artifact> = HashMap::new();
    for artifact in &artifacts {
        if artifact.kind == "review" {
            continue;
        }
        match latest.get(&artifact.kind) {
            Some(current) if current.version >= artifact.version => {}
            _ => {
                latest.insert(artifact.kind.clone(), artifact);
            }
        }
    }
    let mut heads: Vec<&Artifact> = latest.values().copied().collect();
    heads.sort_by(|a, b| a.artifact_id.cmp(&b.artifact_id));

    let cas = cas_store(&state)?;
    let manifest: Vec<Value> = heads
        .iter()
        .map(|artifact| {
            let hash = artifact_hash(artifact);
            let size_bytes = artifact_size(artifact, &cas, &hash);
            json!({
                // 冻结契约（七期四路冻结，字段名固定）
                "artifact_id": artifact.artifact_id,
                "kind": artifact.kind,
                "format": artifact.format,
                "version": artifact.version,
                "sha256": hash,
                "size_bytes": size_bytes,
                "approved": artifact.review_state == ReviewState::Approved,
                "content_url": format!("/artifacts/{}/content", artifact.artifact_id),
                // additive：校验结果与证据引用（验收项：清单含版本/哈希/验证/证据）
                "validation": artifact.validation,
                "evidence_refs": artifact.evidence_refs,
            })
        })
        .collect();

    Ok(Json(json!({
        "project_id": project_id,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "manifest": manifest,
    })))
}
