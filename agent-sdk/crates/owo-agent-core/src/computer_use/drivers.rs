use async_trait::async_trait;
use owo_agent_kernel::required_string;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex as AsyncMutex;

use crate::tools::{resolve_session_path, ToolContext, ToolSpec};
use crate::Tool;

use super::sim::*;

/// 模拟面执行器源：把动作图执行（/learn/execute-package）落到 headless 虚拟窗口，
/// 使“学习沉淀的技能包”可以在模拟环境里复用验证，不触碰真实桌面。
pub struct SimUiActionSource {
    base: String,
    keep: std::sync::Mutex<Vec<serde_json::Value>>,
}

impl SimUiActionSource {
    pub fn new() -> Result<Self, String> {
        let base = sim_base_url().ok_or("模拟环境未配置 OWO_SIM_QQ_URL")?;
        Ok(Self {
            base,
            keep: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn get(&self, path: &str) -> Result<serde_json::Value, String> {
        sim_http_sync(&self.base, "GET", path, None)
    }

    fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
        sim_http_sync(&self.base, "POST", path, Some(&body))
    }

    fn lines(&self) -> Result<Vec<serde_json::Value>, String> {
        let ocr = self.get("ocr")?;
        Ok(ocr
            .get("lines")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    fn line(&self, handle: u64) -> Result<serde_json::Value, String> {
        let keep = self.keep.lock().map_err(|_| "锚点池锁中毒".to_string())?;
        keep.get(handle as usize)
            .cloned()
            .ok_or_else(|| "模拟锚点句柄失效".to_string())
    }

    fn click_line_center(&self, line: &serde_json::Value) -> Result<(), String> {
        let x = line
            .get("x")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0) as i32
            + line
                .get("width")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0) as i32
                / 2;
        let y = line
            .get("y")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0) as i32
            + line
                .get("height")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0) as i32
                / 2;
        self.post("click", json!({ "x": x, "y": y }))?;
        Ok(())
    }
}

impl crate::executor::UiActionSource for SimUiActionSource {
    fn find(&self, anchor: &crate::learn::SemanticAnchor) -> Result<u64, String> {
        let lines = self.lines()?;
        let found = lines
            .iter()
            .find(|line| sim_anchor_matches(line, anchor))
            .cloned()
            .ok_or_else(|| format!("模拟窗口未找到锚点：{}", anchor.name))?;
        let mut keep = self.keep.lock().map_err(|_| "锚点池锁中毒".to_string())?;
        keep.push(found);
        Ok((keep.len() - 1) as u64)
    }

    fn invoke(&self, handle: u64) -> Result<(), String> {
        let line = self.line(handle)?;
        self.click_line_center(&line)
    }

    fn type_text(&self, handle: u64, text: &str) -> Result<(), String> {
        let line = self.line(handle)?;
        if line.get("role_hint").and_then(serde_json::Value::as_str) == Some("input") {
            self.click_line_center(&line)?;
        }
        self.post("type", json!({ "text": text }))?;
        Ok(())
    }

    fn shortcut(&self, combo: &str) -> Result<(), String> {
        self.post("key", json!({ "key": combo }))?;
        Ok(())
    }

    fn launch(&self, _target: &str) -> Result<(), String> {
        Ok(())
    }

    fn click_at(&self, x: i32, y: i32) -> Result<(), String> {
        self.post("click", json!({ "x": x, "y": y }))?;
        Ok(())
    }

    fn verify(&self, predicate: &str) -> Result<bool, String> {
        if let Some(expected) = predicate.strip_prefix("value:") {
            let state = self.get("state")?;
            return Ok(state
                .get("input")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .contains(expected));
        }
        let ocr = self.get("ocr")?;
        let haystack = serde_json::to_string(&ocr).unwrap_or_default();
        if let Some(expected) = predicate.strip_prefix("ui:") {
            return Ok(haystack.contains(expected));
        }
        Ok(haystack.contains(predicate))
    }
}

/// 模拟版面行与语义锚点的匹配规则（供 SimUiActionSource 与单测复用）。
pub(super) fn sim_anchor_matches(
    line: &serde_json::Value,
    anchor: &crate::learn::SemanticAnchor,
) -> bool {
    let text = line
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let role_hint = line.get("role_hint").and_then(serde_json::Value::as_str);
    let role_ok = match anchor.role.as_deref() {
        Some("button") => role_hint == Some("button"),
        Some("edit") | Some("input") => role_hint == Some("input"),
        Some("text") => matches!(
            role_hint,
            Some("message" | "header" | "contact" | "status" | "preview")
        ),
        Some(_) => true,
        None => true,
    };
    role_ok && !anchor.name.is_empty() && text.contains(&anchor.name)
}

/// 纯 TcpStream 的同步 HTTP 客户端（仅本机模拟服务）：
/// 避免 reqwest::blocking 在 async 处理器中析构内部 runtime 导致 panic。
pub(super) fn sim_http_sync(
    base: &str,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let url = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "模拟服务地址仅支持 http://".to_string())?;
    let (host_port, path_and_query) = match rest.split_once('/') {
        Some((host_port, path)) => (host_port, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().unwrap_or(80)),
        None => (host_port, 80),
    };
    let payload = body
        .map(|value| serde_json::to_string(value).unwrap_or_default())
        .unwrap_or_default();
    let request = format!(
        "{method} {path_and_query} HTTP/1.1\r\nHost: {host_port}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let mut stream = TcpStream::connect((host, port))
        .map_err(|e| format!("连接模拟服务失败（{host}:{port}）：{e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|e| format!("设置读超时失败：{e}"))?;
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("发送模拟请求失败：{e}"))?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("读取模拟响应失败：{e}"))?;
    let text = String::from_utf8_lossy(&response).to_string();
    let body_start = text
        .find("\r\n\r\n")
        .map(|index| index + 4)
        .ok_or_else(|| format!("模拟服务响应无正文：{text}"))?;
    let json_body = &text[body_start.min(text.len())..];
    serde_json::from_str(json_body).map_err(|e| format!("模拟服务响应解析失败：{e}：{json_body}"))
}

// ---------- 浏览器工具（Playwright + 本机 Edge） ----------

pub(super) struct BrowserDriver {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl BrowserDriver {
    async fn call(&mut self, command: &str, args: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({ "id": id, "cmd": command, "args": args });
        let mut line =
            serde_json::to_string(&request).map_err(|e| format!("序列化浏览器命令失败：{e}"))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("写入浏览器驱动失败：{e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("刷新浏览器驱动失败：{e}"))?;
        loop {
            let mut response = String::new();
            let read = tokio::time::timeout(
                Duration::from_secs(180),
                self.stdout.read_line(&mut response),
            )
            .await
            .map_err(|_| format!("浏览器命令超时：{command}"))?
            .map_err(|e| format!("读取浏览器驱动失败：{e}"))?;
            if read == 0 {
                return Err(format!("浏览器驱动进程已退出（命令：{command}）"));
            }
            let trimmed = response.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(trimmed)
                .map_err(|e| format!("浏览器驱动返回非法 JSON：{e}：{trimmed}"))?;
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if value.get("ok").and_then(Value::as_bool) == Some(true) {
                return Ok(value.get("data").cloned().unwrap_or(Value::Null));
            }
            return Err(value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("浏览器命令失败")
                .to_string());
        }
    }
}

#[derive(Clone)]
pub struct BrowserTools {
    driver: Arc<AsyncMutex<Option<BrowserDriver>>>,
}

impl BrowserTools {
    pub fn new() -> Self {
        Self {
            driver: Arc::new(AsyncMutex::new(None)),
        }
    }

    async fn call(&self, command: &str, args: Value) -> Result<Value, String> {
        let mut guard = self.driver.lock().await;
        ensure_browser_driver(&mut guard).await?;
        let driver = guard.as_mut().ok_or("浏览器驱动未启动")?;
        let result = driver.call(command, args).await;
        if result.is_err() {
            // 驱动异常时清理，下次调用重新拉起。
            if let Some(child) = guard.as_mut() {
                let _ = child.child.kill().await;
            }
            *guard = None;
        }
        result
    }
}

impl Default for BrowserTools {
    fn default() -> Self {
        Self::new()
    }
}

pub(super) async fn ensure_browser_driver(lock: &mut Option<BrowserDriver>) -> Result<(), String> {
    if lock.is_some() {
        return Ok(());
    }
    let (node, node_path) = node_runtime();
    let script = include_str!("../../../../scripts/browser-driver.js");
    let temp_dir = std::env::temp_dir().join("owo-agent-browser");
    std::fs::create_dir_all(&temp_dir).map_err(|e| format!("创建浏览器驱动目录失败：{e}"))?;
    let script_path = temp_dir.join("browser-driver.js");
    std::fs::write(&script_path, script).map_err(|e| format!("写出浏览器驱动失败：{e}"))?;
    let profile = std::env::var("OWO_BROWSER_PROFILE").unwrap_or_else(|_| {
        let local = std::env::var("LOCALAPPDATA")
            .unwrap_or_else(|_| temp_dir.to_string_lossy().to_string());
        format!("{}\\OwO\\Agent\\browser-profile", local)
    });
    let mut command = Command::new(&node);
    command
        .arg(&script_path)
        .env("OWO_BROWSER_PROFILE", profile)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(node_path) = node_path {
        command.env("NODE_PATH", node_path);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("启动浏览器驱动失败（node={node}）：{e}"))?;
    let stdin = child.stdin.take().ok_or("浏览器驱动 stdin 不可用")?;
    let stdout = child.stdout.take().ok_or("浏览器驱动 stdout 不可用")?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                eprintln!("[browser-driver] {}", line.trim_end());
                line.clear();
            }
        });
    }
    *lock = Some(BrowserDriver {
        child,
        stdin,
        stdout: BufReader::new(stdout),
        next_id: 1,
    });
    Ok(())
}

pub(super) fn node_runtime() -> (String, Option<String>) {
    if let Ok(node) = std::env::var("OWO_BROWSER_NODE") {
        let node_path = std::env::var("OWO_BROWSER_NODE_PATH").ok();
        return (node, node_path);
    }
    if let Ok(node) = std::env::var("OWO_SKILL_NODE") {
        let runtime = std::env::var("OWO_SKILL_RUNTIME").unwrap_or_default();
        let node_path = if runtime.is_empty() {
            None
        } else {
            Some(format!("{}\\node\\node_modules", runtime))
        };
        return (node, node_path);
    }
    const FALLBACK_NODE: &str = r"C:\Users\23843\.cache\codex-runtimes\codex-primary-runtime\dependencies\node\bin\node.exe";
    const FALLBACK_NODE_PATH: &str = r"C:\Users\23843\.cache\codex-runtimes\codex-primary-runtime\dependencies\node\node_modules";
    if Path::new(FALLBACK_NODE).exists() {
        return (
            FALLBACK_NODE.to_string(),
            Some(FALLBACK_NODE_PATH.to_string()),
        );
    }
    ("node".to_string(), None)
}

macro_rules! browser_tool {
    ($tool:ident, $name:literal, $description:literal, $schema:expr) => {
        pub struct $tool {
            pub tools: BrowserTools,
        }

        #[async_trait]
        impl Tool for $tool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: $name.into(),
                    description: $description.into(),
                    input_schema: $schema,
                    effect: None,
                }
            }

            async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
                self.tools
                    .call($name.trim_start_matches("browser_"), args)
                    .await
            }
        }
    };
}

browser_tool!(
    BrowserNavigateTool,
    "browser_navigate",
    "在浏览器中打开 URL（持久化 Edge 窗口，保持登录态）",
    json!({ "type": "object", "properties": { "url": { "type": "string" } }, "required": ["url"] })
);

browser_tool!(
    BrowserSearchTool,
    "browser_search",
    "用 Bing/Baidu 搜索关键词并返回结果列表（标题/链接/摘要）",
    json!({ "type": "object", "properties": { "query": { "type": "string" }, "engine": { "type": "string", "enum": ["bing", "baidu"] } }, "required": ["query"] })
);

browser_tool!(
    BrowserSnapshotTool,
    "browser_snapshot",
    "读取当前页面的可见文本、链接、图片和输入框清单，用于理解页面状态",
    json!({ "type": "object", "properties": { "max_items": { "type": "integer" } } })
);

browser_tool!(
    BrowserClickTool,
    "browser_click",
    "点击页面元素：传 selector（CSS 选择器）或 text（页面可见文本）",
    json!({ "type": "object", "properties": { "selector": { "type": "string" }, "text": { "type": "string" }, "exact": { "type": "boolean" } } })
);

browser_tool!(
    BrowserTypeTool,
    "browser_type",
    "在页面输入框填文本：传 selector 时自动聚焦填充，否则向当前焦点输入",
    json!({ "type": "object", "properties": { "selector": { "type": "string" }, "text": { "type": "string" } }, "required": ["text"] })
);

browser_tool!(
    BrowserPressTool,
    "browser_press",
    "向页面发送按键，例如 Enter / Escape / Tab / Control+A",
    json!({ "type": "object", "properties": { "key": { "type": "string" } }, "required": ["key"] })
);

// 需要写工作区的浏览器工具：路径校验 + 绝对路径传给驱动。
pub struct BrowserScreenshotWriteTool {
    pub tools: BrowserTools,
}

#[async_trait]
impl Tool for BrowserScreenshotWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser_screenshot".into(),
            description: "把当前页面截图保存到工作区路径，返回文件大小".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" }, "full_page": { "type": "boolean" } },
                "required": ["path"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let path = required_string(&args, "path")?;
        let abs = resolve_session_path(ctx, &path)?;
        let mut call_args = args.clone();
        call_args["path"] = json!(abs.to_string_lossy());
        self.tools.call("screenshot", call_args).await
    }
}

pub struct BrowserDownloadImageWriteTool {
    pub tools: BrowserTools,
}

#[async_trait]
impl Tool for BrowserDownloadImageWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser_download_image".into(),
            description: "下载图片到工作区：传 url 直接下载，或传 src（CSS 选择器）取页面中该图片的地址再下载".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "url": { "type": "string" }, "src": { "type": "string" }, "path": { "type": "string" } },
                "required": ["path"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let path = required_string(&args, "path")?;
        let abs = resolve_session_path(ctx, &path)?;
        let mut call_args = args.clone();
        call_args["path"] = json!(abs.to_string_lossy());
        self.tools.call("download_image", call_args).await
    }
}

pub struct BrowserCloseTool {
    pub tools: BrowserTools,
}

#[async_trait]
impl Tool for BrowserCloseTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser_close".into(),
            description: "关闭浏览器会话（清空页面状态）".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
            effect: None,
        }
    }

    async fn run(&self, _ctx: &mut ToolContext<'_>, _args: Value) -> Result<Value, String> {
        let mut guard = self.tools.driver.lock().await;
        if let Some(driver) = guard.as_mut() {
            let _ = driver.call("close", json!({})).await;
            let _ = driver.child.kill().await;
        }
        *guard = None;
        Ok(json!({ "closed": true }))
    }
}
