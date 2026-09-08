//! Small, synchronous supervisor for the bundled local core.
//! The UI still owns reconnect presentation; this module owns discovery and
//! makes launch failures observable instead of silently discarding them.
//!
//! §4.2 扩展：`/health` 实例身份字段、`core_ready` 握手行解析、通用本机 HTTP
//! 请求（无 reqwest 依赖；裸 TcpStream，仅供壳对 loopback 核心使用）。
use serde::Deserialize;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct HealthProbe {
    pub healthy: bool,
    pub api_version: String,
    /// §4.2：核心实例身份（注入 OWO_DESKTOP_INSTANCE_ID 后由 /health 公开）。
    #[serde(default)]
    pub instance_id: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub stage: Option<String>,
}

fn read_health(port: u16) -> Result<HealthProbe, String> {
    let (status, body) = http_request(port, "GET", "/health", &[], None)?;
    if status != 200 {
        return Err(format!("/health 返回 HTTP {status}"));
    }
    serde_json::from_str(&body).map_err(|error| format!("核心服务健康响应无效：{error}"))
}

/// §4.2：壳对 loopback 核心的最小 HTTP 客户端（GET/POST JSON；Connection: close）。
pub(crate) fn http_request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Option<&str>,
) -> Result<(u16, String), String> {
    let address = match ("127.0.0.1", port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addresses| addresses.next())
    {
        Some(address) => address,
        None => return Err("无法解析核心服务地址".to_string()),
    };
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(1000))
        .map_err(|error| format!("核心服务连接失败：{error}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(4000)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(2000)));
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    match body {
        Some(body) => {
            request.push_str("Content-Type: application/json\r\n");
            request.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
            request.push_str(body);
        }
        None => request.push_str("\r\n"),
    }
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("核心服务请求写入失败：{error}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("核心服务响应读取失败：{error}"))?;
    let (head, payload) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| "核心服务响应缺少正文".to_string())?;
    let status_line = head.lines().next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("核心服务状态行无效：{status_line}"))?;
    Ok((status, payload.to_string()))
}

fn validate_health_payload(health: HealthProbe, expected_api_version: &str) -> Result<(), String> {
    if !health.healthy {
        return Err("核心服务未报告 healthy=true".to_string());
    }
    if health.api_version != expected_api_version {
        return Err(format!(
            "核心服务 API 版本不兼容：期望 {expected_api_version}，实际 {}",
            health.api_version
        ));
    }
    Ok(())
}

/// §4.2：以「实例身份 + API 版本」双重校验轮询 /health——只认自己启动的核心。
pub(crate) fn wait_for_instance(
    port: u16,
    expected_instance: &str,
    expected_api_version: &str,
    timeout: Duration,
) -> Result<HealthProbe, String> {
    let started = Instant::now();
    let mut delay = Duration::from_millis(100);
    let mut last_error = None;
    while started.elapsed() < timeout {
        match read_health(port) {
            Ok(health) => {
                if health
                    .stage
                    .as_deref()
                    .is_some_and(|stage| stage != "ready")
                {
                    // /health 已应答但核心仍在启动阶段：按瞬态处理，继续轮询。
                    last_error = Some(format!("核心服务尚未就绪（stage={:?}）", health.stage));
                } else if let Err(error) =
                    validate_health_payload(health.clone(), expected_api_version)
                {
                    last_error = Some(error);
                } else if health.instance_id.as_deref() != Some(expected_instance) {
                    last_error = Some(format!(
                        "核心实例身份不匹配：期望 {expected_instance}，实际 {:?}（该端口属于另一个核心实例）",
                        health.instance_id
                    ));
                } else {
                    return Ok(health);
                }
            }
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(delay);
        delay = std::cmp::min(delay * 2, Duration::from_millis(800));
    }
    Err(last_error.unwrap_or_else(|| "未取得健康检查响应".to_string()))
}

/// §4.2：解析 stdout 中的 `core_ready` 握手行（容忍其余噪声行）。
/// 约定：恰好一行 JSON，含 `"event":"core_ready"` 与 >0 的 port。
/// 纯函数；旧核心不输出该行 → 返回 None，由调用方走兼容回退。
pub(crate) fn parse_ready_line(line: &str) -> Option<Value> {
    let trimmed = line.trim();
    if !trimmed.contains("\"event\":\"core_ready\"") {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    let port = value.get("port")?.as_u64()?;
    if port == 0 || port > u16::MAX as u64 {
        return None;
    }
    value.get("pid")?.as_u64()?;
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::{parse_ready_line, validate_health_payload, HealthProbe};

    #[test]
    fn health_handshake_accepts_matching_api_version() {
        assert!(validate_health_payload(
            HealthProbe {
                healthy: true,
                api_version: "0.7".to_string(),
                instance_id: None,
                pid: None,
                stage: None,
            },
            "0.7"
        )
        .is_ok());
    }

    #[test]
    fn health_handshake_rejects_old_or_incompatible_core() {
        let error = validate_health_payload(
            HealthProbe {
                healthy: true,
                api_version: "0.6".to_string(),
                instance_id: None,
                pid: None,
                stage: None,
            },
            "0.7",
        )
        .expect_err("旧核心不得被桌面壳复用");
        assert!(error.contains("不兼容"));
    }

    #[test]
    fn health_handshake_rejects_unhealthy_core() {
        let error = validate_health_payload(
            HealthProbe {
                healthy: false,
                api_version: "0.7".to_string(),
                instance_id: None,
                pid: None,
                stage: None,
            },
            "0.7",
        )
        .expect_err("未就绪核心不得被桌面壳复用");
        assert!(error.contains("healthy"));
    }

    #[test]
    fn parses_core_ready_line_from_clean_json() {
        let value = parse_ready_line(
            r#"{"event":"core_ready","pid":1234,"port":6071,"api_version":"0.7","build_id":"abc","instance_id":"a1b2"}"#,
        )
        .expect("标准 ready 行应可解析");
        assert_eq!(value["port"].as_u64(), Some(6071));
        assert_eq!(value["pid"].as_u64(), Some(1234));
    }

    #[test]
    fn tolerates_noise_lines_and_rejects_invalid() {
        assert!(parse_ready_line("tracing: log line").is_none());
        assert!(parse_ready_line(r#"{"event":"other"}"#).is_none());
        assert!(
            parse_ready_line(r#"{"event":"core_ready","pid":1,"port":0}"#).is_none(),
            "port=0 无效"
        );
        assert!(parse_ready_line(r#"{"event":"core_ready","pid":1,"port":99999}"#).is_none());
        assert!(
            parse_ready_line(r#"{"event":"core_ready","port":8080}"#).is_none(),
            "缺 pid"
        );
        assert!(parse_ready_line("not json with \"event\":\"core_ready\"").is_none());
    }
}
