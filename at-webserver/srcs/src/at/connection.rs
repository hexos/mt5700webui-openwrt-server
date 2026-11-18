use async_trait::async_trait;
use std::error::Error;
use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

use crate::config::Config;

const IDLE_READ_TIMEOUT: Duration = Duration::from_millis(100);

fn should_reset_connection(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::BrokenPipe
            | ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::NotConnected
            | ErrorKind::UnexpectedEof
            | ErrorKind::ConnectionRefused
            | ErrorKind::TimedOut
    )
}

async fn write_with_timeout<W>(writer: &mut W, data: &[u8]) -> std::io::Result<usize>
where
    W: AsyncWriteExt + Unpin,
{
    timeout(Duration::from_secs(2), writer.write(data))
        .await
        .unwrap_or_else(|_| Err(std::io::Error::new(ErrorKind::TimedOut, "write timed out")))
}

async fn read_with_timeout<R>(reader: &mut R, timeout_dur: Duration) -> std::io::Result<Vec<u8>>
where
    R: AsyncReadExt + Unpin,
{
    let mut buf = vec![0u8; 1024];
    match timeout(timeout_dur, reader.read(&mut buf)).await {
        Ok(Ok(n)) => {
            if n == 0 {
                return Err(std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "connection closed",
                ));
            }
            buf.truncate(n);
            Ok(buf)
        }
        Ok(Err(err)) => Err(err),
        Err(_) => Err(std::io::Error::new(ErrorKind::TimedOut, "read timed out")),
    }
}

// 串口 / 网络流式连接共用的 send/receive 逻辑。
// SerialStream 与 TcpStream 都实现了 AsyncReadExt + AsyncWriteExt，泛型化后
// 消除 SerialATConn / NetworkATConn 里逐行复制的错误处理分支。
async fn stream_send<S>(stream: &mut Option<S>, data: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>>
where
    S: AsyncWriteExt + Unpin,
{
    if let Some(s) = stream.as_mut() {
        match write_with_timeout(s, data).await {
            Ok(n) => Ok(n),
            Err(err) => {
                *stream = None;
                if should_reset_connection(&err) {
                    return Err("Disconnected".into());
                }
                Err(Box::new(err))
            }
        }
    } else {
        Err("Disconnected".into())
    }
}

async fn stream_receive<S>(stream: &mut Option<S>) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>>
where
    S: AsyncReadExt + Unpin,
{
    if let Some(s) = stream.as_mut() {
        match read_with_timeout(s, IDLE_READ_TIMEOUT).await {
            Ok(data) => Ok(data),
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Ok(Vec::new())
            }
            Err(err) => {
                *stream = None;
                if should_reset_connection(&err) {
                    return Err("Disconnected".into());
                }
                Err(Box::new(err))
            }
        }
    } else {
        Err("Disconnected".into())
    }
}

// ========== AT 连接抽象 ==========

#[async_trait]
pub trait ATConnection: Send {
    async fn connect(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>;
    async fn send(&mut self, data: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>>;
    async fn receive(&mut self) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>>;
    fn is_connected(&self) -> bool;
    /// 标记连接已断开，下一次心跳/重连逻辑会重新建立连接。
    fn disconnect(&mut self);
}

// ========== 串口连接实现 ==========

pub struct SerialATConn {
    pub config: Arc<Config>,
    stream: Option<SerialStream>,
}

impl SerialATConn {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            stream: None,
        }
    }
}

#[async_trait]
impl ATConnection for SerialATConn {
    async fn connect(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let serial = &self.config.at_config.serial;
        let port = tokio_serial::new(&serial.port, serial.baudrate)
            .timeout(Duration::from_secs(serial.timeout))
            .open_native_async()?;
        self.stream = Some(port);
        Ok(())
    }

    async fn send(&mut self, data: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>> {
        stream_send(&mut self.stream, data).await
    }

    async fn receive(&mut self) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        stream_receive(&mut self.stream).await
    }

    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    fn disconnect(&mut self) {
        self.stream = None;
    }
}

// ========== 网络 TCP 连接实现 ==========

pub struct NetworkATConn {
    pub config: Arc<Config>,
    stream: Option<TcpStream>,
}

impl NetworkATConn {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            stream: None,
        }
    }
}

#[async_trait]
impl ATConnection for NetworkATConn {
    async fn connect(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let network = &self.config.at_config.network;
        let addr = format!("{}:{}", network.host, network.port);
        let stream = timeout(
            Duration::from_secs(network.timeout),
            TcpStream::connect(addr),
        )
        .await??;
        self.stream = Some(stream);
        Ok(())
    }

    async fn send(&mut self, data: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>> {
        stream_send(&mut self.stream, data).await
    }

    async fn receive(&mut self) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        stream_receive(&mut self.stream).await
    }

    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    fn disconnect(&mut self) {
        self.stream = None;
    }
}

// ========== TomModem 外部命令实现 ==========

pub struct TomModemATConn {
    pub config: Arc<Config>,
    is_connected: bool,
    response: Option<String>,
}

impl TomModemATConn {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            is_connected: false,
            response: None,
        }
    }

    /// 将 serial.feature 映射为 tom_modem 的命令行开关。
    ///
    /// QModem 的 tom_modem 用 `-u` / `--use_ubus` 启用 UBUS AT daemon
    /// （见 application/tom_modem/src/utils.c 的 usage 与 main.c 的
    /// `case USE_UBUS`），旧代码把 feature 值 "UBUS" 直接拼成 `-UBUS`，
    /// 会被 match_option 当作短参数 `-U` 而解析失败。这里做显式映射，
    /// 其余 feature 值沿用 `-<feature>` 以兼容自定义开关。
    fn feature_flag(feature: &str) -> Option<String> {
        let f = feature.trim();
        if f.is_empty() || f.eq_ignore_ascii_case("NONE") {
            return None;
        }
        if f.eq_ignore_ascii_case("UBUS") || f.eq_ignore_ascii_case("u") {
            return Some("-u".to_string());
        }
        // 已是完整开关形式（如 "-u" / "--use_ubus"）时原样使用
        if f.starts_with('-') {
            return Some(f.to_string());
        }
        Some(format!("-{}", f))
    }
}

#[async_trait]
impl ATConnection for TomModemATConn {
    async fn connect(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.is_connected = true;
        Ok(())
    }

    async fn send(&mut self, data: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>> {
        if !self.is_connected {
            return Err("Disconnected".into());
        }

        let command = String::from_utf8_lossy(data).trim().to_string();
        let serial = &self.config.at_config.serial;
        let mut args = vec![serial.port.clone(), "-c".to_string(), command.clone()];

        if let Some(flag) = Self::feature_flag(&serial.feature) {
            args.push(flag);
        }

        let output = timeout(
            Duration::from_secs(serial.timeout),
            tokio::process::Command::new("tom_modem").args(&args).output(),
        )
        .await??;

        if output.status.success() {
            let response = String::from_utf8_lossy(&output.stdout).to_string();
            self.response = Some(response);
            Ok(data.len())
        } else {
            let error = String::from_utf8_lossy(&output.stderr);
            Err(format!("tom_modem执行失败: {}", error).into())
        }
    }

    async fn receive(&mut self) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        Ok(self
            .response
            .take()
            .map(String::into_bytes)
            .unwrap_or_default())
    }

    fn is_connected(&self) -> bool {
        self.is_connected
    }

    fn disconnect(&mut self) {
        self.is_connected = false;
    }
}

// ========== QModem at-daemon ubus 直连实现 ==========

/// 通过 `ubus call at-daemon sendat` 与 QModem 共享的串口通信。
///
/// 与流式连接（串口/网络）不同，at-daemon 的 sendat 是“一次调用返回完整响应”，
/// 所以这里把响应缓存到 `response`，由 `receive()` 一次性取走，从而复用
/// `ATClient::send_command` 里已有的结束符判定 / 清洗逻辑。
///
/// 连接建立时调用 `open`（幂等），发送时附带 `owner` 以便通过租约鉴权；
/// 作为 WebUI 观察者不主动 `lease_acquire`，遇到 `settings_pending`
/// （QModem 初始化期间独占串口）时短暂等待重试。
pub struct UbusATConn {
    pub config: Arc<Config>,
    /// 当前实际使用的 at_port；自动识别模式下随 at-daemon 状态更新。
    at_port: String,
    /// 自动识别失败时的兜底端口。
    fallback_port: String,
    auto_detect: bool,
    is_connected: bool,
    response: Option<String>,
}

impl UbusATConn {
    pub fn new(config: Arc<Config>) -> Self {
        let (at_port, fallback_port, auto_detect) = {
            let serial = &config.at_config.serial;
            let current = serial.port.clone();
            let fallback = if serial.fallback_port.is_empty() {
                current.clone()
            } else {
                serial.fallback_port.clone()
            };
            (current, fallback, serial.auto_detect)
        };
        Self {
            config,
            at_port,
            fallback_port,
            auto_detect,
            is_connected: false,
            response: None,
        }
    }

    /// 单次 sendat 的秒数超时：取串口配置超时，至少 2 秒。
    fn send_timeout(&self) -> u64 {
        self.config.at_config.serial.timeout.max(2)
    }

    /// 租约 owner 标识，用于 at-daemon 的 at_lease_authorize 鉴权。
    fn owner(&self) -> &'static str {
        crate::ubus::OWNER
    }
}

#[async_trait]
impl ATConnection for UbusATConn {
    async fn connect(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        // 重连时重新识别：模组重插 / USB 重枚举后 /dev/ttyUSBx 可能变化。
        if self.auto_detect {
            let preferred = self.at_port.clone();
            let fallback = self.fallback_port.clone();
            if let Some(p) = crate::ubus::rediscover_at_port(&preferred, &fallback).await {
                if p != self.at_port {
                    crate::log_info!("[UBUS] AT 端口自动切换: {} -> {}", self.at_port, p);
                    self.at_port = p;
                }
            }
        }

        let baudrate = self.config.at_config.serial.baudrate;
        let params = serde_json::json!({
            "at_port": self.at_port,
            "baudrate": baudrate,
            "databits": 8,
            "parity": 0,
            "stopbits": 1,
        });
        let reply = crate::ubus::ubus_call("open", &params, self.send_timeout() + 3).await?;
        let status = reply.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if status == "success" {
            self.is_connected = true;
            crate::log_info!(
                "[UBUS] 已连接 at-daemon 串口: {} @ {} bps",
                self.at_port,
                baudrate
            );
            Ok(())
        } else {
            let msg = reply
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("未知错误");
            self.is_connected = false;
            Err(format!("at-daemon open 失败: {}", msg).into())
        }
    }

    async fn send(&mut self, data: &[u8]) -> Result<usize, Box<dyn Error + Send + Sync>> {
        if !self.is_connected {
            return Err("Disconnected".into());
        }

        // 去掉 ATClient::normalize_command 补上的 \r\n；at-daemon 自己会补。
        let command = String::from_utf8_lossy(data)
            .trim_matches(|c| c == '\r' || c == '\n' || c == ' ')
            .to_string();
        if command.is_empty() {
            return Ok(data.len());
        }

        let params = serde_json::json!({
            "at_port": self.at_port,
            "at_cmd": command,
            "timeout": self.send_timeout(),
            "owner": self.owner(),
        });

        // QModem settings 初始化期间持有租约，sendat 返回 settings_pending。
        // WebUI 不抢租约，短暂等待重试；最多约 2 秒。
        let max_retries = 8u32;
        for attempt in 0..max_retries {
            let reply =
                crate::ubus::ubus_call("sendat", &params, self.send_timeout() + 3).await?;
            let status = reply.get("status").and_then(|v| v.as_str()).unwrap_or("");
            match status {
                "success" => {
                    let response = reply
                        .get("response")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    crate::log_debug!("[UBUS] sendat <= {:?}", response);
                    self.response = Some(response);
                    return Ok(data.len());
                }
                "settings_pending" => {
                    if attempt + 1 == max_retries {
                        return Err("AT 端口正被 QModem 初始化占用 (settings_pending)".into());
                    }
                    sleep(Duration::from_millis(250)).await;
                }
                "timeout" => return Err("TIMEOUT".into()),
                "error" => {
                    let msg = reply
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("sendat 失败");
                    return Err(msg.into());
                }
                other => return Err(format!("sendat 未知状态: {}", other).into()),
            }
        }
        Err("sendat 重试耗尽".into())
    }

    async fn receive(&mut self) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        Ok(self
            .response
            .take()
            .map(String::into_bytes)
            .unwrap_or_default())
    }

    fn is_connected(&self) -> bool {
        self.is_connected
    }

    fn disconnect(&mut self) {
        self.is_connected = false;
        self.response = None;
    }
}