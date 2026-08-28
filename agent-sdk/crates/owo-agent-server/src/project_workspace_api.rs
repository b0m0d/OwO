//! Project Workspace：TeamRun 真实工作区绑定（六期 · 第二路）。
//!
//! 路由面（经 [`crate::workswarm_api::router`] 合并挂载）：
//! - `PUT  /projects/{id}/workspace`：绑定/更新项目工作区（root 只读默认开）；
//! - `GET  /projects/{id}/workspace`：读取绑定；
//! - `GET  /projects/{id}/workspace/tree`：绑定目录树（深度受限，不跟随符号链接）；
//! - `GET  /projects/{id}/workspace/git-status`：绑定目录的 `git status --porcelain`。
//!
//! 安全模型：
//! - 绑定持久化为运行数据 sidecar（`<run_dir>/<team_id>-workspace.json`），
//!   cancel/retry/resume 后循环按迭代重建注册表时重新读取——绑定生命周期独立于运行状态；
//! - root/白名单路径一律 canonicalize（符号链接/Junction 解析后）再做包含校验，
//!   显式拒绝 `..` 组件；不存在的白名单目标按「父目录 canonicalize + 文件名」解析；
//! - 只读绑定强制 `Policy::read_only` + 只读工具集（叠加在步骤级 read_only 之上）；
//! - 写入需同时满足：非只读绑定 + `write_allowed_paths` 白名单（空 = root 内可写）+ 审批。

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::Json;
use owo_agent_core::permissions::{Approver, Decision, Level, PermissionRequest};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use super::error_response;
use crate::AppState;

/// 进程内绑定缓存（team_id → binding；PUT 后刷新，循环迭代读缓存免磁盘 IO）。
fn bindings_cache() -> &'static Mutex<HashMap<String, WorkspaceBinding>> {
    static CACHE: OnceLock<Mutex<HashMap<String, WorkspaceBinding>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// POST /teams `workspace` 字段与 PUT 请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceSpec {
    /// 项目根目录（必须已存在）。
    pub root: String,
    /// 只读（缺省 true；写入必须显式放开）。
    #[serde(default = "default_true")]
    pub read_only: bool,
    /// 允许写入的相对路径白名单（空 = 非 只读时 root 内可写）。
    #[serde(default)]
    pub write_allowed_paths: Vec<String>,
    /// 目录树深度（缺省 3，上限 8）。
    #[serde(default)]
    pub tree_depth: Option<u32>,
}

fn default_true() -> bool {
    true
}

/// 工作区绑定（sidecar 持久化 + 运行期注入 worker）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceBinding {
    pub team_id: String,
    pub project_id: String,
    /// 用户输入的 root 原文。
    pub root: String,
    /// canonicalize 后的 root（符号链接/Junction 已解析）。
    pub root_canonical: String,
    pub read_only: bool,
    /// 相对白名单原文。
    pub write_allowed_paths: Vec<String>,
    /// canonicalize 后的白名单绝对路径。
    pub write_allowed_canonical: Vec<String>,
    pub tree_depth: u32,
    pub created_at: String,
}

impl WorkspaceBinding {
    /// 执行作用域（worker 注入用）。
    pub fn scope(&self) -> WorkspaceScope {
        WorkspaceScope {
            root: PathBuf::from(&self.root_canonical),
            read_only: self.read_only,
            allowed: self
                .write_allowed_canonical
                .iter()
                .map(PathBuf::from)
                .collect(),
        }
    }
}

/// Worker 执行作用域：绑定目录 + 只读 + 写白名单。
#[derive(Debug, Clone)]
pub struct WorkspaceScope {
    pub root: PathBuf,
    pub read_only: bool,
    pub allowed: Vec<PathBuf>,
}

/// 去掉 Windows extended-length 前缀（`\\?\C:\...` → `C:\...`）：
/// canonicalize 语义不变，但避免 git/子进程对 verbatim 路径的兼容问题。
fn simplify(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped.to_string()),
        None => path.to_path_buf(),
    }
}

/// 校验并规范化工作区声明（路径安全核心：canonicalize 后包含校验）。
pub fn validate_workspace_spec(spec: &WorkspaceSpec) -> Result<WorkspaceBinding, String> {
    let root = PathBuf::from(spec.root.trim());
    if spec.root.trim().is_empty() {
        return Err("workspace.root 不能为空".to_string());
    }
    if root.as_os_str().to_string_lossy().contains("..") {
        return Err("workspace.root 不得包含 `..`".to_string());
    }
    let root_canonical = simplify(
        &std::fs::canonicalize(&root)
            .map_err(|e| format!("workspace.root 不可用（{}）：{e}", spec.root))?,
    );
    if !root_canonical.is_dir() {
        return Err(format!(
            "workspace.root 必须是已存在的目录：{}",
            root_canonical.display()
        ));
    }
    let mut write_allowed_canonical = Vec::new();
    for raw in &spec.write_allowed_paths {
        let rel = raw.trim();
        if rel.is_empty() {
            return Err("write_allowed_paths 含空路径".to_string());
        }
        if rel.contains("..") {
            return Err(format!("write_allowed_paths 不得包含 `..`：{rel}"));
        }
        let candidate = root_canonical.join(rel);
        // 目标可能尚不存在：存在的部分 canonicalize（解析符号链接/Junction），
        // 不存在的尾段原样拼接。
        let resolved = simplify(&canonicalize_best_effort(&candidate));
        if !resolved.starts_with(&root_canonical) {
            return Err(format!(
                "write_allowed_paths 越出工作区：{rel} → {}",
                resolved.display()
            ));
        }
        write_allowed_canonical.push(resolved.to_string_lossy().to_string());
    }
    let tree_depth = spec.tree_depth.unwrap_or(3).min(8);
    Ok(WorkspaceBinding {
        team_id: String::new(),
        project_id: String::new(),
        root: spec.root.trim().to_string(),
        root_canonical: root_canonical.to_string_lossy().to_string(),
        read_only: spec.read_only,
        write_allowed_paths: spec
            .write_allowed_paths
            .iter()
            .map(|p| p.trim().to_string())
            .collect(),
        write_allowed_canonical,
        tree_depth,
        created_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// canonicalize 尽力而为：目标不存在时对最远存在祖先 canonicalize 后拼接尾段。
fn canonicalize_best_effort(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => {
            let mut exists = path.to_path_buf();
            let mut suffix: Vec<std::ffi::OsString> = Vec::new();
            loop {
                match exists.parent() {
                    Some(parent) => {
                        suffix.push(
                            exists
                                .file_name()
                                .map(|n| n.to_os_string())
                                .unwrap_or_default(),
                        );
                        exists = parent.to_path_buf();
                        if exists.try_exists().unwrap_or(false) {
                            break;
                        }
                    }
                    None => return path.to_path_buf(),
                }
            }
            let mut resolved = std::fs::canonicalize(&exists).unwrap_or(exists);
            for part in suffix.iter().rev() {
                resolved = resolved.join(part);
            }
            resolved
        }
    }
}

// ---------------------------------------------------------------------------
// sidecar 持久化（<run_dir>/<team_id>-workspace.json）
// ---------------------------------------------------------------------------

fn sidecar_path(run_dir: &Path, team_id: &str) -> PathBuf {
    run_dir.join(format!("{team_id}-workspace.json"))
}

/// 读取团队工作区绑定（缓存 → sidecar → None）。
pub fn load_binding(run_dir: &Path, team_id: &str) -> Option<WorkspaceBinding> {
    if let Ok(map) = bindings_cache().lock() {
        if let Some(binding) = map.get(team_id) {
            return Some(binding.clone());
        }
    }
    let text = std::fs::read_to_string(sidecar_path(run_dir, team_id)).ok()?;
    let binding: WorkspaceBinding = serde_json::from_str(&text).ok()?;
    if let Ok(mut map) = bindings_cache().lock() {
        map.insert(team_id.to_string(), binding.clone());
    }
    Some(binding)
}

/// 保存团队工作区绑定（sidecar + 缓存）。
pub fn save_binding(run_dir: &Path, binding: &WorkspaceBinding) -> Result<(), String> {
    let path = sidecar_path(run_dir, &binding.team_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建运行目录失败：{e}"))?;
    }
    let text = serde_json::to_string_pretty(binding).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("写工作区绑定失败：{e}"))?;
    if let Ok(mut map) = bindings_cache().lock() {
        map.insert(binding.team_id.clone(), binding.clone());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 执行期审批器：写白名单 + 只读强制（叠加在 Policy 的 workspace 作用域之上）
// ---------------------------------------------------------------------------

/// 工作区范围审批器：只读绑定一律拒绝写/执行；非只读按白名单放行文件写。
///
/// Read 级操作恒放行（文件/命令路径已被 Policy 限制在绑定 root 内）。
pub struct WorkspaceScopeApprover {
    pub allow_writes: bool,
    pub root: PathBuf,
    /// canonical 白名单；空 = root 内均可写。
    pub allowed: Vec<PathBuf>,
}

impl WorkspaceScopeApprover {
    fn write_path_allowed(&self, raw: &str) -> bool {
        let candidate = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            self.root.join(raw)
        };
        // canonicalize 产物带 `\\?\` 前缀，先 simplify 再与 root/allowed（已去前缀）比对。
        let resolved = simplify(&canonicalize_best_effort(&candidate));
        if !resolved.starts_with(&self.root) {
            return false;
        }
        if self.allowed.is_empty() {
            return true;
        }
        self.allowed.iter().any(|base| resolved.starts_with(base))
    }
}

#[async_trait::async_trait]
impl Approver for WorkspaceScopeApprover {
    async fn decide(&self, request: &PermissionRequest) -> Decision {
        match request.level {
            Level::Read => Decision::Allow,
            Level::Write => {
                if !self.allow_writes {
                    return Decision::Deny;
                }
                let path = request
                    .args
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if self.write_path_allowed(path) {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
            // 命令执行在 allow_writes 时放行（危险片段由 Policy 黑名单拦截）；
            // 只读绑定一律拒绝。
            Level::Execute | Level::Inject => {
                if self.allow_writes {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// 项目 → 团队（绑定以 TeamRun 为主体；project 无团队 → 409）。
async fn team_of_project(
    state: &Arc<AppState>,
    project_id: &str,
) -> Result<
    (
        Arc<owo_agent_core::workswarm::TeamCoordinator>,
        String,
        String,
    ),
    (StatusCode, Json<Value>),
> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let space = coordinator
        .get_project_space(project_id)
        .await
        .map_err(|e| error_response(&e))?;
    let team_id = space.team_id.clone().ok_or_else(|| {
        (
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("项目 {project_id} 未关联团队运行") })),
        )
    })?;
    Ok((coordinator, team_id, space.project_id))
}

/// PUT /projects/{id}/workspace：绑定/更新工作区。
pub(crate) async fn put_workspace(
    State(state): State<Arc<AppState>>,
    AxumPath(project_id): AxumPath<String>,
    Json(spec): Json<WorkspaceSpec>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // 先校验（不依赖团队存在，避免建队后才发现路径非法）。
    let mut binding = validate_workspace_spec(&spec)
        .map_err(|msg| (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))))?;
    let (coordinator, team_id, resolved_project) = team_of_project(&state, &project_id).await?;
    binding.team_id = team_id;
    binding.project_id = resolved_project;
    save_binding(coordinator.run_dir(), &binding).map_err(|msg| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": msg })),
        )
    })?;
    Ok(Json(json!({ "workspace": binding })))
}

/// GET /projects/{id}/workspace：读取绑定。
pub(crate) async fn get_workspace(
    State(state): State<Arc<AppState>>,
    AxumPath(project_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (coordinator, team_id, _) = team_of_project(&state, &project_id).await?;
    let binding = load_binding(coordinator.run_dir(), &team_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("项目 {project_id} 未绑定工作区") })),
        )
    })?;
    Ok(Json(json!({ "workspace": binding })))
}

/// GET /projects/{id}/workspace/tree：受限深度目录树（不跟随符号链接/Junction）。
pub(crate) async fn get_workspace_tree(
    State(state): State<Arc<AppState>>,
    AxumPath(project_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (coordinator, team_id, _) = team_of_project(&state, &project_id).await?;
    let binding = load_binding(coordinator.run_dir(), &team_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("项目 {project_id} 未绑定工作区") })),
        )
    })?;
    let root = PathBuf::from(&binding.root_canonical);
    let mut entries: Vec<Value> = Vec::new();
    let mut truncated = false;
    collect_tree(
        &root,
        &root,
        binding.tree_depth,
        0,
        &mut entries,
        &mut truncated,
        2000,
    );
    Ok(Json(json!({
        "root": binding.root_canonical,
        "depth": binding.tree_depth,
        "truncated": truncated,
        "entries": entries,
    })))
}

/// 迭代式受限深度遍历（symlink/junction 一律跳过，不进入）。
fn collect_tree(
    root: &Path,
    dir: &Path,
    max_depth: u32,
    depth: u32,
    entries: &mut Vec<Value>,
    truncated: &mut bool,
    cap: usize,
) {
    if depth >= max_depth || entries.len() >= cap {
        if entries.len() >= cap {
            *truncated = true;
        }
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut kids: Vec<_> = read.filter_map(Result::ok).collect();
    kids.sort_by_key(|e| e.file_name());
    for entry in kids {
        if entries.len() >= cap {
            *truncated = true;
            return;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let entry_path = entry.path();
        let Ok(rel) = entry_path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if meta.is_dir() {
            entries.push(json!({ "path": rel, "type": "dir" }));
            collect_tree(
                root,
                &entry_path,
                max_depth,
                depth + 1,
                entries,
                truncated,
                cap,
            );
        } else {
            entries.push(json!({
                "path": rel,
                "type": "file",
                "size": meta.len(),
            }));
        }
    }
}

/// GET /projects/{id}/workspace/git-status：绑定目录的 `git status --porcelain`。
pub(crate) async fn get_workspace_git_status(
    State(state): State<Arc<AppState>>,
    AxumPath(project_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (coordinator, team_id, _) = team_of_project(&state, &project_id).await?;
    let binding = load_binding(coordinator.run_dir(), &team_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("项目 {project_id} 未绑定工作区") })),
        )
    })?;
    let output = tokio::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&binding.root_canonical)
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
            Ok(Json(json!({
                "git": true,
                "root": binding.root_canonical,
                "porcelain": text,
                "entries": lines,
            })))
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            Ok(Json(json!({
                "git": false,
                "root": binding.root_canonical,
                "error": stderr.trim(),
            })))
        }
        Err(e) => Ok(Json(json!({
            "git": false,
            "root": binding.root_canonical,
            "error": format!("git 不可用：{e}"),
        }))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 审批器：只读强制拒绝写/执行；白名单匹配放行、越界拒绝；空白名单 root 内放行。
    #[tokio::test]
    async fn approver_enforces_readonly_and_whitelist() {
        let temp = tempfile::tempdir().unwrap();
        // 与生产一致：审批器的 root/allowed 均来自 canonical 化绑定（root_canonical）。
        let root = simplify(&std::fs::canonicalize(temp.path()).unwrap());
        std::fs::create_dir_all(root.join("docs")).unwrap();

        fn req_of(level: Level, path: &str) -> PermissionRequest {
            PermissionRequest {
                request_id: "r".to_string(),
                tool: "write_file".to_string(),
                args: json!({ "path": path }),
                level,
                reason: "test".to_string(),
            }
        }

        // 只读：写/执行一律 Deny，读恒 Allow。
        let ro = WorkspaceScopeApprover {
            allow_writes: false,
            root: root.clone(),
            allowed: vec![],
        };
        assert_eq!(
            ro.decide(&req_of(Level::Write, "a.txt")).await,
            Decision::Deny
        );
        assert_eq!(
            ro.decide(&req_of(Level::Execute, "build.cmd")).await,
            Decision::Deny
        );
        assert_eq!(
            ro.decide(&req_of(Level::Read, "a.txt")).await,
            Decision::Allow
        );

        // 非只读 + 白名单 docs/：白名单内放行；root 内白名单外拒绝；root 外绝对路径拒绝。
        let scoped = WorkspaceScopeApprover {
            allow_writes: true,
            root: root.clone(),
            allowed: vec![root.join("docs")],
        };
        assert_eq!(
            scoped.decide(&req_of(Level::Write, "docs/new.txt")).await,
            Decision::Allow,
            "白名单内相对路径应放行"
        );
        assert_eq!(
            scoped.decide(&req_of(Level::Write, "root-level.txt")).await,
            Decision::Deny,
            "root 内白名单外应拒绝"
        );
        let outside = tempfile::tempdir().unwrap();
        let outside_root = simplify(&std::fs::canonicalize(outside.path()).unwrap());
        let outside_path = outside_root.join("escape.txt");
        assert_eq!(
            scoped
                .decide(&req_of(Level::Write, &outside_path.to_string_lossy()))
                .await,
            Decision::Deny,
            "工作区外绝对路径应拒绝"
        );

        // 非只读 + 空白名单：root 内放行。
        let open = WorkspaceScopeApprover {
            allow_writes: true,
            root: root.clone(),
            allowed: vec![],
        };
        assert_eq!(
            open.decide(&req_of(Level::Write, "root-level.txt")).await,
            Decision::Allow
        );
    }

    /// Spec 缺省语义：read_only 缺省 true、白名单缺省空、tree_depth 缺省 None。
    #[test]
    fn spec_defaults_are_read_only() {
        let spec: WorkspaceSpec =
            serde_json::from_str(&json!({ "root": "C:\\no-check" }).to_string()).unwrap();
        assert!(spec.read_only);
        assert!(spec.write_allowed_paths.is_empty());
        assert_eq!(spec.tree_depth, None);
    }

    /// validate_workspace_spec：`..` root / 白名单越界 / 不存在 root 均拒绝；
    /// 合法声明产出 canonical 化绑定（不含 verbatim 前缀），tree_depth 上限 8。
    #[test]
    fn validate_spec_rejects_escapes_and_canonicalizes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();

        let ok = validate_workspace_spec(&WorkspaceSpec {
            root: root.to_string_lossy().to_string(),
            read_only: false,
            write_allowed_paths: vec!["docs".to_string(), "src/lib.rs".to_string()],
            tree_depth: Some(64),
        })
        .unwrap();
        assert!(!ok.read_only);
        assert_eq!(ok.tree_depth, 8, "tree_depth 上限 8");
        assert!(
            !ok.root_canonical.starts_with(r"\\?\"),
            "canonical 路径不应带 verbatim 前缀：{}",
            ok.root_canonical
        );
        assert_eq!(ok.write_allowed_canonical.len(), 2);
        assert!(ok.write_allowed_canonical[0].ends_with("docs"));

        let dotdot = validate_workspace_spec(&WorkspaceSpec {
            root: root
                .join("..")
                .join("outside")
                .to_string_lossy()
                .to_string(),
            read_only: true,
            write_allowed_paths: vec![],
            tree_depth: None,
        });
        assert!(dotdot.is_err(), "root 含 `..` 应拒绝");

        let escape = validate_workspace_spec(&WorkspaceSpec {
            root: root.to_string_lossy().to_string(),
            read_only: true,
            write_allowed_paths: vec!["../outside".to_string()],
            tree_depth: None,
        });
        assert!(escape.is_err(), "白名单越界应拒绝");

        let missing = validate_workspace_spec(&WorkspaceSpec {
            root: temp.path().join("no-such").to_string_lossy().to_string(),
            read_only: true,
            write_allowed_paths: vec![],
            tree_depth: None,
        });
        assert!(missing.is_err(), "不存在的 root 应拒绝");
    }

    /// 绑定 sidecar 往返（save → load 一致；未知团队 None）。
    #[test]
    fn binding_sidecar_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let mut binding = validate_workspace_spec(&WorkspaceSpec {
            root: root.to_string_lossy().to_string(),
            read_only: true,
            write_allowed_paths: vec![],
            tree_depth: None,
        })
        .unwrap();
        binding.team_id = "team-x".to_string();
        binding.project_id = "proj-x".to_string();
        save_binding(temp.path(), &binding).unwrap();
        let loaded = load_binding(temp.path(), "team-x").unwrap();
        assert_eq!(loaded.root_canonical, binding.root_canonical);
        assert_eq!(loaded.project_id, "proj-x");
        assert!(load_binding(temp.path(), "team-none").is_none());
    }
}
