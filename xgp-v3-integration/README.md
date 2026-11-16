# mt5700webui × XGPV3（西瓜皮V3）集成

本目录把 mt5700webui（`at-webserver` + `luci-app-at-webserver`）重新接入 XGPV3 的
LEDE 自动构建（`zzzz0317/lede-xgp-auto-build`），并以 **QModem 的 `at-daemon` ubus
通道**与模组共享串口，避免和 QModem 抢串口。

> 背景：XGPV3 构建曾集成 mt5700webui，后因串口冲突（atserver 直接 `open()` 串口）
> 与 QModem 争用同一 `/dev/ttyUSBx`，导致响应残缺/超时而被移除。本次通过
> `connection_type=UBUS` 让 atserver 改走 at-daemon，从根本上消除串口争用。

## 1. 涉及的文件

| 文件 | 用途 |
|------|------|
| `xgp.config.snippet` | 追加到 `xgp.config` 的配置片段（mt5700webui + QModem 选项修正） |
| `prepare.sh.snippet` | 追加到 `prepare.sh` 的拉取片段（把 mt5700webui 放进 `package/zz/`） |
| `at-webserver.xgpv3.default` | XGPV3 上 `/etc/config/at-webserver` 的推荐默认值（UBUS 模式） |

## 2. 前提

- QModem feed 已启用，且 `ubus-at-daemon` 包被选中（`tom_modem` 的
  `ENABLE_UBUS_DAEMON` 依赖会自动拉入，但建议在 `xgp.config` 显式置 y）。
- 目标设备具备 Rust 交叉编译能力（LEDE 的 `packages` feed 自带 rust 支持，
  `at-webserver` 的 Makefile 通过 `feeds/packages/lang/rust/rust-package.mk` 引用）。
- QModem 侧 `use_ubus=1`（当前 QModem 通过 `91-qmodem-use-ubus` uci-defaults
  默认开启）。

## 3. 集成步骤

1. 按 `prepare.sh.snippet` 修改构建仓库的 `prepare.sh`，拉取 mt5700webui。
2. 按 `xgp.config.snippet` 修改 `xgp.config`。
3. （可选）把 `at-webserver.xgpv3.default` 作为 `files/etc/config/at-webserver`
   放进构建仓库，默认走 UBUS + `serial_port='auto'` 自动识别 AT 串口。
4. 触发构建。镜像内应包含 `/usr/bin/atserver`、`/etc/init.d/at-webserver`、
   `/etc/config/at-webserver`、`/www/5700/` 与 LuCI 菜单项。

## 4. 刷机后验证

```sh
# at-daemon 已在运行并打开串口
ubus list at-daemon -v
ubus call at-daemon list

# atserver 日志应出现自动识别结果与 UBUS 连接行
#   "[UBUS] 自动识别 AT 端口: /dev/ttyUSB1 (来源: at-daemon list, 兜底: ...)"
logread -f | grep -i ubus

# 手工发一条 AT，验证 ubus 通道（at_port 以 list 的实际输出为准）
ubus call at-daemon sendat '{"at_port":"/dev/ttyUSB1","at_cmd":"AT+CSQ"}'

# 打开 http://<路由>/5700/ 的 AT 终端收发
```

## 5. 关于串口（at_port）自动识别

XGPV3 实测 at-daemon 打开的是 `/dev/ttyUSB1`，但 PCIe 5G 模组走 MHI，
换模组或 USB/MHI 枚举顺序变化都会让 `/dev/ttyUSBx` 漂移。因此默认配置
不再写死端口：

```sh
option serial_port 'auto'                # 自动识别
option serial_port_custom '/dev/ttyUSB1' # XGPV3 实测兜底值
```

`auto` 在 UBUS 模式下让 atserver 启动时执行 `ubus call at-daemon list`，
取 `is_open=1` 的端口（即 QModem 实际持有的 AT 口）；at-daemon 尚未就绪时
最多等约 30 秒，再退回 `/dev` 节点扫描，最后才用 `serial_port_custom`。
每次断线重连都会重新识别，模组重插后无需改配置或重启服务。

需要固定端口时，把 `serial_port` 写成 `/dev/ttyUSBx` 即关闭自动识别。
可在 LuCI `服务 → AT WebServer → 串口设备` 选择「自动检测」或具体设备，
并用 `ubus call at-daemon list` 核对当前端口。详见根目录 `UBUS_ADAPTATION.md`
的 4.3.1 节。

## 6. 回退

如需恢复独立直连串口，把 `connection_type` 改回 `SERIAL`（或 `NETWORK`）并重启
`/etc/init.d/at-webserver restart` 即可，无需重刷。
