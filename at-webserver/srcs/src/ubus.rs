//! UBUS 适配层：把 QModem 的 `at-daemon` 能力接入 mt5700webui。
//!
//! QModem 的 `at-daemon` 在 ubus 上注册了 `at-daemon` 对象：
//!   - 方法：`open` / `sendat` / `list` / `close` / `lease_*` / `urc_*`
//!   - 事件：`qmodem.at.line`（每一行）、`qmodem.at.urc`（匹配注册前缀的空闲行）
//!
//! 本模块只依赖 OpenWrt/ImmortalWrt 系统自带的 `ubus` 命令行工具，
//! 不引入任何 Rust 原生 libubus 绑定，因此交叉编译零额外依赖。
//!
//! 串口由 at-daemon 独占持有；需在 QModem 侧启用 `use_ubus=1` 才能共享。

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio::time::{sleep, Duration};

/// at-daemon 的 ubus 对象名（QModem 固定）。
pub const AT_DAEMON_OBJECT: &str = "at-daemon";
/// 每一条 AT 收发行的事件名。
pub const EVENT_LINE: &str = "qmodem.at.line";
/// 匹配 URC 前缀的空闲行事件名。
pub const EVENT_URC: &str = "qmodem.at.urc";

/// 清理 at-daemon `raw_line` 字段：
/// at-daemon 用 `blobmsg_add_field(..., line_len + 1)` 发送，末尾带一个 `\0`，
/// 经 JSON 序列化后表现为 `\u0000`，这里统一去掉首尾的 NUL / CR / LF。
fn clean_raw_line(raw: &str) -> String {
    raw.trim_matches(|c| c == '\0' || c == '\r' || c == '\n')
        .to_string()
}

/// 调用 at-daemon 的 ubus 方法，返回解析后的 JSON 对象。
///
/// `timeout_secs` 为 ubus CLI 的整体超时（-t），应略大于方法自身超时。
pub async fn ubus_call(method: &str, params: &Value, timeout_secs: u64) -> Result<Value, String> {
    let payload = params.to_string();
    let timeout = timeout_secs.max(1).to_string();

    let output = Command::new("ubus")
        .arg("-S")
        .arg("-t")
        .arg(&timeout)
        .arg("call")
        .arg(AT_DAEMON_OBJECT)
        .arg(method)
        .arg(&payload)
        .output()
        .await
        .map_err(|e| format!("无法执行 ubus 命令: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "ubus {} 调用失败（退出码 {:?}）",
                method,
                output.status.code()
            )
        } else {
            stderr
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = stdout.trim();
    if text.is_empty() {
        return Err(format!("ubus {} 返回空响应", method));
    }
    serde_json::from_str(text)
        .map_err(|e| format!("解析 ubus {} 响应失败: {}（原始: {}）", method, e, text))
}

// ========== AT 串口自动识别 ==========
//
// XGPV3 上模组的 AT 串口不是固定的：换模组、改 USB 配置、MHI/USB 枚举顺序
// 变化都会让 /dev/ttyUSBx 漂移。QModem 的 at-daemon 才是「谁真正持有 AT 口」
// 的唯一权威来源，所以自动识别以 `ubus call at-daemon list` 为准，不主动往
// 串口发探测命令（避免与 QModem 抢租约）。

/// UCI `serial_port` 写成这些值时启用自动识别（大小写不敏感）。
pub fn is_auto_port(value: &str) -> bool {
    let v = value.trim();
    v.is_empty() || v.eq_ignore_ascii_case("auto") || v.eq_ignore_ascii_case("detect")
}

/// at-daemon 的 `is_open` 可能以数字、布尔或字符串形式出现。
fn port_is_open(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0) > 0,
        Some(Value::String(s)) => s == "1" || s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// 从 `ubus call at-daemon list` 的回复里挑出最可信的 AT 端口。
///
/// 回复形如 `{"ports":[{"port":"/dev/ttyUSB1","is_open":1,...},
/// {"port":"/dev/ttyUSB2","file_status":"exists",...}]}`。
/// `is_open=1` 表示该端口已被 at-daemon（即 QModem）打开，优先级最高；
/// 其次是设备节点确实存在的端口。多个候选时优先保留 `preferred`，
/// 避免端口重枚举时无谓漂移。
pub fn pick_at_port(reply: &Value, preferred: &str) -> Option<String> {
    let ports = reply.get("ports")?.as_array()?;
    let mut opened: Vec<String> = Vec::new();
    let mut existing: Vec<String> = Vec::new();

    for entry in ports {
        let port = match entry.get("port").and_then(|v| v.as_str()) {
            Some(p) if p.starts_with("/dev/") => p.to_string(),
            _ => continue,
        };
        let open = port_is_open(entry.get("is_open"));
        if open {
            opened.push(port.clone());
        }
        if open || entry.get("file_status").and_then(|v| v.as_str()) == Some("exists") {
            existing.push(port);
        }
    }

    for candidates in [&opened, &existing] {
        if candidates.is_empty() {
            continue;
        }
        if !preferred.is_empty() && candidates.iter().any(|p| p.as_str() == preferred) {
            return Some(preferred.to_string());
        }
        return candidates.first().cloned();
    }
    None
}

fn is_numbered(name: &str, prefix: &str) -> bool {
    match name.strip_prefix(prefix) {
        Some(suffix) => !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// 扫描 `/dev` 下常见 AT 串口节点，作为 at-daemon 尚未就绪时的补充。
/// 只枚举节点、不打开端口；ttyS*（板载调试串口）排在最后。
fn scan_serial_devices() -> Vec<String> {
    let entries = match std::fs::read_dir("/dev") {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut found: Vec<(u8, String)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let tier = if is_numbered(&name, "ttyUSB") {
            0
        } else if is_numbered(&name, "ttyACM") {
            1
        } else if name.starts_with("wwan") && name.contains("at") {
            2
        } else if name.starts_with("mhi_") {
            3
        } else if is_numbered(&name, "ttyS") {
            4
        } else {
            continue;
        };
        found.push((tier, format!("/dev/{}", name)));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().map(|(_, p)| p).collect()
}

/// 单次读取 at-daemon 当前持有的端口（不等待、不扫描设备节点）。
pub async fn current_at_port(preferred: &str) -> Option<String> {
    let reply = ubus_call("list", &json!({}), 5).await.ok()?;
    pick_at_port(&reply, preferred)
}

/// 重连时重新识别：at-daemon 没报告端口时退回 `/dev` 节点扫描。
pub async fn rediscover_at_port(preferred: &str, fallback: &str) -> Option<String> {
    if let Some(p) = current_at_port(preferred).await {
        return Some(p);
    }
    let devices = scan_serial_devices();
    devices
        .iter()
        .find(|p| p.as_str() == fallback)
        .cloned()
        .or_else(|| devices.first().cloned())
}

/// 启动时自动识别 AT 端口。
///
/// atserver 往往比 QModem / at-daemon 先起来，因此最多等待 `attempts` 轮
/// （每轮间隔 3 秒）直到 `list` 里出现端口。等不到则扫描 `/dev` 节点，
/// 最后才用 UCI 的 `serial_port_custom` 兜底，保证服务始终能起来。
pub async fn discover_at_port(fallback: &str, attempts: u32) -> (String, &'static str) {
    let total = attempts.max(1);
    for attempt in 0..total {
        if let Some(p) = current_at_port(fallback).await {
            return (p, "at-daemon list");
        }
        if attempt + 1 < total {
            crate::log_info!(
                "[UBUS] 尚未发现 at-daemon 持有的 AT 端口，3 秒后重试 ({}/{})",
                attempt + 1,
                total
            );
            sleep(Duration::from_secs(3)).await;
        }
    }

    let devices = scan_serial_devices();
    if let Some(p) = devices.iter().find(|p| p.as_str() == fallback) {
        return (p.clone(), "/dev 节点（兜底值）");
    }
    if let Some(p) = devices.first() {
        return (p.clone(), "/dev 节点扫描");
    }
    (fallback.to_string(), "配置兜底")
}

/// best-effort 注册我们关心的 URC 前缀。
///
/// at-daemon 以 (owner, urc_id) 为唯一键；这里用独立 owner，
/// 不会与 QModem 自身的 `qmodem.sms` / `qmodem.voip` 注册冲突。
/// 注册后同一行会同时出现在 `qmodem.at.line` 与 `qmodem.at.urc`，由去重逻辑合并。
async fn register_urc_prefixes(at_port: &str, owner: &str) {
    const PREFIXES: &[(&str, &str)] = &[
        ("sms", "+CMT"),          // +CMTI: / +CMT:
        ("sms-status", "+CDS"),   // +CDS:
        ("call-clip", "+CLIP"),   // +CLIP:
        ("call-ring", "RING"),    // RING
        ("call-busy", "BUSY"),    // BUSY
        ("call-nocarrier", "NO CARRIER"),
        ("call-noanswer", "NO ANSWER"),
        ("vendor", "^"),          // 厂商自定义 URC（^MONSC、^HFREQINFO 等）
    ];

    for (id, prefix) in PREFIXES {
        let params = json!({
            "at_port": at_port,
            "owner": owner,
            "urc_id": id,
            "prefix": prefix,
        });
        match ubus_call("urc_register", &params, 5).await {
            Ok(v) if v.get("status").and_then(|s| s.as_str()) == Some("success") => {}
            other => crate::log_debug!("[UBUS] 注册 URC {} 未成功: {:?}", id, other),
        }
    }
    crate::log_info!(
        "[UBUS] URC 前缀注册完成 (at_port={}, owner={})",
        at_port,
        owner
    );
}

/// 把一条事件载荷分发为 URC 通知 + WebSocket 广播。
fn dispatch_urc(
    payload: &Value,
    urc_tx: &broadcast::Sender<String>,
    seen: &mut VecDeque<(u64, u64)>,
) {
    let raw = match payload.get("raw_line").and_then(|v| v.as_str()) {
        Some(s) => clean_raw_line(s),
        None => return,
    };
    if raw.is_empty() {
        return;
    }

    // 去重：同一行可能同时来自 qmodem.at.line 与 qmodem.at.urc。
    let epoch = payload
        .get("restart_epoch")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let seq = payload
        .get("sequence")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if epoch > 0 && seq > 0 {
        let key = (epoch, seq);
        if seen.contains(&key) {
            return;
        }
        seen.push_back(key);
        if seen.len() > 512 {
            seen.pop_front();
        }
    }

    // qmodem.at.line 携带 correlation：只有 idle（非命令响应）行才是 URC。
    // qmodem.at.urc 已由 at-daemon 过滤过，没有 correlation 字段。
    if let Some(corr) = payload.get("correlation").and_then(|v| v.as_str()) {
        if corr != "idle" || !crate::is_urc_line(&raw) {
            return;
        }
    }

    if let Some((kind, message)) = crate::notify::describe_urc(&raw) {
        crate::notify::event(kind, &message);
    }
    let _ = urc_tx.send(raw);
}

/// 从一行 `ubus listen` 输出里取出指定事件的载荷。
///
/// ubus CLI 的 print_event 输出形如：
/// `{"qmodem.at.line": {"port":"...","raw_line":"...","correlation":"idle",...}}`
fn event_payload(line: &str, event_name: &str) -> Option<Value> {
    let root: Value = serde_json::from_str(line).ok()?;
    root.get(event_name).cloned()
}

/// 长驻事件监听：订阅 at-daemon 的 line / urc 事件，翻译为 urc_tx 广播。
/// 断线自动重连（ubusd 或 at-daemon 重启后事件流会中断）。
pub async fn monitor_events(
    urc_tx: broadcast::Sender<String>,
    owner: String,
    at_port: String,
    auto_detect: bool,
) {
    let mut at_port = at_port;
    let mut seen: VecDeque<(u64, u64)> = VecDeque::new();

    loop {
        // 每次（重）连接时重新识别端口：URC 前缀注册是按 at_port 绑定的，
        // 模组重插后必须跟着换，否则 qmodem.at.urc 事件不会送到这里。
        if auto_detect {
            if let Some(p) = rediscover_at_port(&at_port, &at_port).await {
                if p != at_port {
                    crate::log_info!("[UBUS] 事件监听 AT 端口更新: {} -> {}", at_port, p);
                    at_port = p;
                }
            }
        }

        crate::log_info!("[UBUS] 启动事件监听 ({} / {})", EVENT_LINE, EVENT_URC);

        // 每次连接（重连）后重新注册，因为 at-daemon 可能已重启。
        register_urc_prefixes(&at_port, &owner).await;

        let mut child = match Command::new("ubus")
            .arg("-S")
            .arg("listen")
            .arg(EVENT_LINE)
            .arg(EVENT_URC)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                crate::log_error!("[UBUS] 无法启动 ubus listen: {}，3 秒后重试", e);
                sleep(Duration::from_secs(3)).await;
                continue;
            }
        };

        if let Some(stdout) = child.stdout.take() {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let line = match lines.next_line().await {
                    Ok(Some(l)) => l,
                    Ok(None) => break,
                    Err(e) => {
                        crate::log_warn!("[UBUS] 读取事件失败: {}", e);
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }

                if let Some(v) = event_payload(&line, EVENT_LINE) {
                    dispatch_urc(&v, &urc_tx, &mut seen);
                    continue;
                }
                if let Some(v) = event_payload(&line, EVENT_URC) {
                    dispatch_urc(&v, &urc_tx, &mut seen);
                }
            }
        }

        let _ = child.wait().await;
        crate::log_warn!("[UBUS] 事件监听中断，3 秒后重连...");
        sleep(Duration::from_secs(3)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_trailing_nul() {
        assert_eq!(clean_raw_line("RING\0"), "RING");
        assert_eq!(clean_raw_line("+CMTI: \"SM\",1\0"), "+CMTI: \"SM\",1");
    }

    #[test]
    fn parses_line_event_payload() {
        let line = r#"{"qmodem.at.line":{"port":"/dev/ttyUSB0","raw_line":"+CMTI: \"SM\",3","correlation":"idle","sequence":5,"restart_epoch":1}}"#;
        let v = event_payload(line, EVENT_LINE).expect("should parse");
        assert_eq!(v.get("correlation").unwrap().as_str(), Some("idle"));
        assert_eq!(
            v.get("raw_line").unwrap().as_str(),
            Some("+CMTI: \"SM\",3")
        );
    }

    #[test]
    fn ignores_unrelated_event() {
        let line = r#"{"some.other.event":{"x":1}}"#;
        assert!(event_payload(line, EVENT_LINE).is_none());
    }

    #[test]
    fn auto_port_values() {
        assert!(is_auto_port("auto"));
        assert!(is_auto_port("AUTO"));
        assert!(is_auto_port("detect"));
        assert!(is_auto_port("  "));
        assert!(!is_auto_port("/dev/ttyUSB1"));
        assert!(!is_auto_port("custom"));
    }

    #[test]
    fn picks_open_port_from_daemon_list() {
        let reply: Value = serde_json::from_str(
            r#"{"ports":[
                {"port":"/dev/ttyUSB2","is_open":0,"file_status":"missing"},
                {"port":"/dev/ttyUSB1","is_open":1,"fd":7}
            ]}"#,
        )
        .unwrap();
        assert_eq!(pick_at_port(&reply, "").as_deref(), Some("/dev/ttyUSB1"));
    }

    #[test]
    fn prefers_configured_port_when_open() {
        let reply: Value = serde_json::from_str(
            r#"{"ports":[
                {"port":"/dev/ttyUSB1","is_open":1},
                {"port":"/dev/ttyUSB2","is_open":1}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            pick_at_port(&reply, "/dev/ttyUSB2").as_deref(),
            Some("/dev/ttyUSB2")
        );
    }

    #[test]
    fn falls_back_to_existing_node() {
        let reply: Value = serde_json::from_str(
            r#"{"ports":[{"port":"/dev/ttyUSB3","is_open":0,"file_status":"exists"}]}"#,
        )
        .unwrap();
        assert_eq!(pick_at_port(&reply, "").as_deref(), Some("/dev/ttyUSB3"));
    }

    #[test]
    fn skips_missing_ports() {
        let reply: Value = serde_json::from_str(
            r#"{"ports":[{"port":"/dev/ttyUSB9","is_open":0,"file_status":"missing"}]}"#,
        )
        .unwrap();
        assert!(pick_at_port(&reply, "").is_none());
        assert!(pick_at_port(&json!({"ports": []}), "").is_none());
    }
}
