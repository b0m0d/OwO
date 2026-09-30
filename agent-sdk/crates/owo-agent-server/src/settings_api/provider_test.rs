/// 端点脱敏：仅保留 scheme + host[:port]（禁止回显路径/查询/用户信息段）。
pub(super) fn mask_endpoint(base_url: &str) -> String {
    let (scheme, rest) = base_url.split_once("://").unwrap_or(("unknown", base_url));
    let authority = rest.split('/').next().unwrap_or("");
    // userinfo（user@host）一律剥离。
    let host = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    if host.is_empty() {
        scheme.to_string()
    } else {
        format!("{scheme}://{host}")
    }
}

/// TCP 层可达性探测（3s 超时）。返回连接耗时毫秒。
pub(super) async fn probe_endpoint(base_url: &str) -> Result<u128, String> {
    let rest = base_url
        .split_once("://")
        .map(|(_, tail)| tail)
        .unwrap_or(base_url);
    let authority = rest.split('/').next().unwrap_or(rest);
    // userinfo 必须先剥掉再解析：否则 `https://token@host/v1` 会把整串
    // `token@host` 当主机名送进 lookup_host —— 凭据因此进入 DNS 查询与解析错误
    // 上下文，正是这个端点最不该发生的泄露面（掩码端点却拿未掩码的值去连网）。
    let authority = authority
        .rsplit_once('@')
        .map(|(_, host_only)| host_only)
        .unwrap_or(authority);
    let default_port: u16 = if base_url.starts_with("http://") {
        80
    } else {
        443
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            // `[::1]:8080` 这类括号 IPv6 字面量只有冒号出现在 ']' 之后才是端口；
            // `[::1]`（省略端口）不能误判成"端口无效"。
            let explicit = !h.starts_with('[') || h.ends_with(']');
            if !explicit {
                (authority.to_string(), default_port)
            } else {
                // 写了 `:xxx` 却不是数字：**必须直说配置错了**。此前这里静默按
                // 443 去连，把"端口打错"报成"端点不可达"，归因方向整个错掉（§2.5）。
                match p.parse::<u16>() {
                    Ok(v) => (h.to_string(), v),
                    Err(_) => return Err(format!("端口无效：{p}")),
                }
            }
        }
        None => (authority.to_string(), default_port),
    };
    if host.is_empty() {
        return Err("端点缺少主机名".to_string());
    }
    let started = std::time::Instant::now();
    let connect = async move {
        let addrs = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|error| format!("DNS 解析失败：{error}"))?;
        let addrs: Vec<_> = addrs.collect();
        let addr = addrs.first().ok_or("DNS 无解析结果".to_string())?;
        tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|error| format!("连接 {}:{} 失败：{error}", addr.ip(), addr.port()))
    };
    match tokio::time::timeout(std::time::Duration::from_secs(3), connect).await {
        Err(_) => Err("连接超时（3s）".to_string()),
        Ok(Err(error)) => Err(error),
        Ok(Ok(_stream)) => Ok(started.elapsed().as_millis()),
    }
}

/// §3.4「provider 未配置 / 测试连接」契约的 L2 底线：这条端点会被引导页按钮调用，
/// 它的返回值**必然落盘到诊断台账并显示给用户**，所以"只回掩码端点、绝不回显凭据"
/// 不是风格问题，而是秘密泄露面。真机矩阵只能证明界面有码可看，证明不了掩码正确性，
/// 故在此按单元层钉住。
#[cfg(test)]
mod provider_test_contract {
    use super::{mask_endpoint, probe_endpoint};

    #[test]
    fn mask_keeps_host_but_strips_credentials_and_path() {
        // 端点里带 userinfo、带 path、query 里还塞了个 key：三者都不得出现在结果里。
        let masked = mask_endpoint("https://user:pw@api.bigmodel.cn/api/paas/v4?key=sk-SECRET123");
        assert_eq!(masked, "https://api.bigmodel.cn");
        assert!(!masked.contains("SECRET123"), "查询串里的 key 不得回显");
        assert!(!masked.contains("user"), "userinfo 不得回显");
        // 掩码后的"authority 段"必须就是主机名：路径与查询都不得残留
        // （`://` 里的两个斜杠是协议分隔符，不算路径）。
        assert_eq!(
            masked.split_once("://").map(|(_, tail)| tail),
            Some("api.bigmodel.cn"),
            "scheme 之后不得再有路径：{masked}"
        );
    }

    #[test]
    fn mask_keeps_local_port_for_diagnostics() {
        // Ollama 这类本地端点：端口不是秘密，且"127.0.0.1:11434 连不通"正是用户
        // 需要看见的信息，剥掉端口只会让排查回到猜。
        assert_eq!(
            mask_endpoint("http://127.0.0.1:11434/v1"),
            "http://127.0.0.1:11434"
        );
    }

    #[test]
    fn mask_degrades_on_non_url_input_without_panicking() {
        // 配置里出现裸串（用户手输漏了 scheme）时，掩码必须仍可渲染。
        let masked = mask_endpoint("just-a-host");
        assert!(!masked.is_empty());
        assert!(
            masked.starts_with("unknown://"),
            "退化形态应显式标 unknown，实测试探到的正是这一支：{masked}"
        );
    }

    #[tokio::test]
    async fn probe_returns_transport_verdict_without_credentials() {
        // 端口非法：解析层就拒绝，不该发起任何连接。
        let bad = probe_endpoint("https://user:pw@example.invalid:notaport/v1").await;
        assert!(bad.is_err());
        let bad_msg = bad.err().unwrap();
        assert!(
            bad_msg.contains("端口无效"),
            "应给出可操作的归因：{bad_msg}"
        );
        assert!(!bad_msg.contains("pw"), "错误信息不得带出 userinfo");

        // 保留端口但含 userinfo/path 的端点：错误里只可能出现 host:port。
        let refused = probe_endpoint("https://token@127.0.0.1:1/v1").await;
        let msg = match refused {
            Ok(ms) => format!("ok {ms}"),
            Err(e) => e,
        };
        assert!(!msg.contains("token"), "回显里不得出现凭据：{msg}");
        assert!(!msg.contains("/v1"), "回显里不得出现路径：{msg}");
    }
}
