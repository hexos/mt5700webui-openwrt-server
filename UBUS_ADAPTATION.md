# mt5700webui × QModem ubus 适配说明

本仓库的 Rust 服务端（`atserver`）新增了 `UBUS` 连接类型，通过 QModem 的
`at-daemon` ubus 服务与模组通信，从而**与 QModem 共享同一个串口**，不再独占。

## 1. 为什么需要适配

原版有三种连接方式：

| 方式 | 说明 | 与 QModem 共存 |
|------|------|----------------|
| `NETWORK` | 直连模组网络 AT（192.168.8.1:20249） | 取决于模组是否同时开放网络 AT |
| `SERIAL` | 自己 `open()` 串口独占读写 | ❌ 与 QModem 抢串口缓冲 |
| `TOM_MODEM` + `-u` | 调用 `tom_modem ... -u`，由它转给 at-daemon（XGPV3 原厂方案） | ✅（本次已修复参数） |
| `UBUS`（新增） | atserver 直接 `ubus call at-daemon sendat`，不依赖 tom_modem | ✅（本次新增） |

QModem 从 ubus 暴露了 `at-daemon` 对象：串口由它**唯一持有**，所有 AT I/O 经它
排队转发；短信/来电等 URC 通过 `qmodem.at.line` / `qmodem.at.urc` 事件广播。
因此正确做法是让 mt5700webui 也走这条 ubus 通道。

## 2. 适配后的架构

```
Web 前端 (www/5700)  ──WebSocket──>  atserver (Rust)
                                        │
                    ┌───────────────────┴────────────────────┐
                    │  AT 命令（ubus call）                    │  URC（ubus listen）
                    ▼                                        ▼
              at-daemon.sendat                     qmodem.at.line / qmodem.at.urc
                    │                                        │
                    └───────────────────┬────────────────────┘
                                        ▼
                          串口 /dev/ttyUSBx（QModem 持有）
```

- **命令下行**：`UbusATConn::send()` 调用
  `ubus -S call at-daemon sendat '{"at_port":...,"at_cmd":...,"timeout":...,"owner":...}'`，
  一次调用拿到完整响应，复用 `ATClient` 原有的结束符判定 / 回显清洗逻辑。
- **URC 上行**：`ubus::monitor_events()` 运行
  `ubus -S listen qmodem.at.line qmodem.at.urc`，解析事件 JSON，
  只取 `correlation == "idle"` 的行，经 `is_urc_line()` 过滤后写入 `urc_tx`，
  最终由 WebSocket 以 `{"type":"raw_data","data":...}` 推给前端，
  同时驱动通知日志（短信 / 来电）。
- **事件去重**：同一行可能同时出现在两个事件里，按 `(restart_epoch, sequence)` 去重。
- **租约处理**：atserver 作为观察者**不抢占租约**（不调用 `lease_acquire`）；
  当 QModem 初始化期间持有租约时，`sendat` 返回 `settings_pending`，
  atserver 短暂重试（最多约 2 秒）而不是失败。

## 3. 涉及的代码改动

| 文件 | 改动 |
|------|------|
| `at-webserver/srcs/src/ubus.rs`（新增） | ubus CLI 调用封装、事件监听、URC 分发与去重；`at_port` 自动识别 |
| `at-webserver/srcs/src/at/connection.rs` | 新增 `UbusATConn`（实现 `ATConnection`）；重连时刷新 at_port |
| `at-webserver/srcs/src/at/client.rs` | `ATClient::new()` 增加 `UBUS` 分支；补 `#[derive(Clone)]` |
| `at-webserver/srcs/src/config.rs` | `serial_port=auto` 解析，`auto_detect` / `fallback_port` 字段 |
| `at-webserver/srcs/src/main.rs` | 注册 `ubus` 模块；启动自动识别；UBUS 模式改用事件总线取 URC；配置摘要 |
| `at-webserver/Makefile` | 增加 `+ubus` 依赖；版本 1.1.0 |
| `at-webserver/files/etc/config/at-webserver` | 说明 UBUS 模式复用的串口参数与 auto 用法 |
| `luci-app-at-webserver/.../view/at-webserver/config.js` | 连接类型增加 `UBUS`；串口下拉增加「自动检测」 |

## 4. 部署步骤

### 4.1 安装依赖

在 ImmortalWrt/OpenWrt 上需要同时具备：

1. **QModem 的 `at-daemon`**：提供 ubus 对象 `at-daemon`（`ubus-at-daemon` 包）。
2. **`ubus` 命令行工具**（`ubus` 包，`/bin/ubus`）。
3. 本仓库的 `at-webserver` 与 `luci-app-at-webserver`。

### 4.2 让 QModem 走 ubus 并共享串口

关键：QModem 侧必须启用 `use_ubus`，否则 at-daemon 启动后会独占串口缓冲：
```sh
uci set qmodem.<modem_section>.use_ubus='1'
uci commit qmodem
/etc/init.d/qmodem_init restart   # 或重启相关服务
```

确保 at-daemon 正在运行并已 open 对应串口：
```sh
ubus list at-daemon -v
ubus call at-daemon list
```

### 4.3 配置 at-webserver

编辑 `/etc/config/at-webserver`：
```sh
config at-webserver 'config'
    option enabled '1'
    option connection_type 'UBUS'
    option serial_port 'auto'                # 自动识别 at-daemon 持有的 AT 串口
    option serial_port_custom '/dev/ttyUSB1' # 识别失败时的兜底（XGPV3 实测值）
    option serial_baudrate '115200'
    option serial_timeout '10'               # sendat 超时（秒）
    # ... WebSocket / 通知等其它配置保持不变
```

或在 LuCI：`服务 → AT WebServer → 连接类型` 选择 **QModem UBUS (at-daemon)**，
串口设备选 **自动检测（UBUS 模式推荐）**。需要固定端口时，把 `serial_port`
直接写成 `/dev/ttyUSBx` 即关闭自动识别。

```sh
/etc/init.d/at-webserver restart
```

#### 4.3.1 AT 串口自动识别怎么工作

`serial_port='auto'`（或 `detect`）仅在 UBUS 模式生效，解决 `/dev/ttyUSBx`
因模组更换、USB/MHI 枚举顺序变化而漂移的问题：

1. 启动时调用 `ubus call at-daemon list`，取 `ports[]` 中 `is_open=1` 的端口
   —— 这是 QModem 实际打开并持有的 AT 串口，最可信；其次是
   `file_status="exists"` 的端口。多个候选时优先保留兜底值，避免无谓漂移。
2. atserver 通常比 QModem 先起来，所以最多等 10 轮、每轮 3 秒（约 30 秒）。
3. 等不到 at-daemon 时扫描 `/dev` 下的 `ttyUSB*` / `ttyACM*` / `wwan*at*` /
   `mhi_*` / `ttyS*` 节点（按此优先级）。**只枚举节点、不主动发 AT 命令**，
   避免与 QModem 抢租约。
4. 仍无结果则使用 `serial_port_custom` 兜底，服务照常启动。
5. 之后每次断线重连都会重新识别；事件监听任务重连时也会按新端口重新注册
   URC 前缀，因此模组重插后无需重启服务、无需改配置。

日志会打印识别来源，便于定位：
`[UBUS] 自动识别 AT 端口: /dev/ttyUSB1 (来源: at-daemon list, 兜底: /dev/ttyUSB1)`。

### 4.4 验证

```sh
# 1. 服务端日志应出现
#    "[UBUS] 自动识别 AT 端口: /dev/ttyUSB1 (来源: at-daemon list, 兜底: ...)"
#    "[UBUS] 已连接 at-daemon 串口: /dev/ttyUSB1 @ 115200 bps"
#    "[UBUS] 启动事件监听 (qmodem.at.line / qmodem.at.urc)"
logread -f | grep -i ubus

# 2. 先看 at-daemon 实际持有的端口，再手动发一条 AT 验证 ubus 通道
ubus call at-daemon list
ubus call at-daemon sendat '{"at_port":"/dev/ttyUSB1","at_cmd":"AT+CSQ"}'

# 3. 打开 WebUI（http://<路由>/5700/），AT 终端可正常收发即成功
```

## 5. 注意事项

- **串口唯一**：UBUS 模式下 atserver 不再自己 open 串口，务必保证 at-daemon 已 open
  同一 `at_port`；两个 at_port 不一致会导致命令超时。默认 `serial_port='auto'`
  会自动对齐，只有写死端口时才需要手工核对。
- **初始化窗口**：QModem 若持有租约（ttl 默认 300s）初始化模组，此期间 atserver 的
  命令会短暂返回 `settings_pending` 并自动重试；初始化完成后恢复。
- **防火墙 / WebSocket**：UBUS 只改变 AT 命令的获取方式，WebSocket 端口、认证、
  通知配置等保持不变。
- **回退**：如需恢复直连，把 `connection_type` 改回 `NETWORK` 或 `SERIAL` 并重启服务即可。

## 6. 关于 `-u` 参数（TOM_MODEM 模式）

XGPV3 原厂方案走的是 `TOM_MODEM`，而不是本仓库新增的 `UBUS` 连接类型。
两者最终都用 at-daemon 共享串口，但入口不同：

| 模式 | atserver 侧配置 | 实际调用 |
|------|----------------|---------|
| `UBUS`（新增） | `connection_type='UBUS'` | `ubus call at-daemon sendat ...`（atserver 直接发） |
| `TOM_MODEM`（原厂） | `connection_type='SERIAL'`, `serial_method='TOM_MODEM'`, `serial_feature='UBUS'` | `tom_modem <port> -c <cmd> -u`（tom_modem 内部转给 at-daemon） |

原代码把 `serial_feature='UBUS'` 直接拼成 `-UBUS`，但 QModem 的 `tom_modem`
只接受 **`-u` / `--use_ubus`**（见 `application/tom_modem/src/utils.c` 的 usage
与 `main.c` 的 `case USE_UBUS`；短参数定义为 `USE_UBUS_S 'u'`）。`-UBUS` 会被
解析为短参数 `-U` 而报无效。

**本次修复**（`at-webserver/srcs/src/at/connection.rs` 的
`TomModemATConn::feature_flag()`）：`UBUS` → `-u`，其余取值保持兼容。
`-u` 需要 `tom_modem` 编译时带 `ENABLE_UBUS_DAEMON`（XGPV3 的
`CONFIG_PACKAGE_tom_modem_ENABLE_UBUS_DAEMON=y` 已满足）。

## 7. XGPV3（西瓜皮V3）集成

XGPV3 的 LEDE 固件由 `zzzz0317/lede-xgp-auto-build` 构建仓库产出：拉取
`coolsnowwolf/lede` 主线、QModem feed（`FUjr/QModem`）和自有配置拼装编译。
该构建仓库此前未含 mt5700webui（或已移除），本次重新接入并改为走 at-daemon
ubus 通道。针对 XGPV3 的"适配补全"改动集（可直接套用的配置/脚本片段、
操作说明、验证清单）见随附的 `xgp-v3-integration/` 目录。

要点：
1. XGPV3 的 `xgp.config` 需打开 `ubus-at-daemon`、`tom_modem_ENABLE_UBUS_DAEMON`、
   `qmodem`，并恢复 `at-webserver` / `luci-app-at-webserver` 编译项。
2. QModem 从 v2.8.x 升级到 v3.x 后，`luci-app-qmodem_INCLUDE_*` 等选项迁移到了
   `qmodem_INCLUDE_*`，旧名会被 `make defconfig` 静默丢弃（含 PCIe MHI 驱动），
   需一并改名。
3. mt5700webui 走 `connection_type=UBUS`，串口由 QModem at-daemon 独占，
   不再与 QModem 抢串口；同时修复 TOM_MODEM 模式的 `-u` 参数。

