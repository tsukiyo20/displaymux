# displaymux

轻量的跨平台**显示器信号源切换**工具：通过 DDC/CI 控制显示器的输入源（VCP `0x60`
Input Select），把一台显示器在 Mac / Windows / Linux 之间切来切去。

- **命令行**：`dmux`
- **图形界面**：`dmux-gui`（[GPUI](https://gpui.rs)，Zed 的 GPU 原生 UI 框架）

> 目标形态是一个**常驻的小工具**：启动快、占用低、点一下就切。
> 因此刻意**不使用 WebView**（Tauri / Electron / wry 这类每个窗口都要拖一个
> Web 引擎进程，对"偶尔点一下"的工具属于压倒性开销）。

参考实现：[haimgel/display-switch](https://github.com/haimgel/display-switch)
（把 30 美元的 USB 切换器变成 KVM）。displaymux 面向的是**图形界面 + 手动/自动
切换**，而不是它的「USB 设备插拔触发」模式；两者共用同一套 macOS 底层
（`ddc-macos` 的作者就是 display-switch 的作者）。

---

## 当前状态

在真实硬件上验证过的部分，和还没做的部分，分得很清楚：

| 能力 | 状态 |
|---|---|
| 枚举 DDC 显示器 | ✅ 实机验证 |
| 读取当前输入源 / 亮度 | ✅ 实机验证（8/8 稳定） |
| 解析显示器 capabilities，得出可切端口列表 | ✅ 实机验证（含单元测试） |
| **切换输入源** | ✅ 实机验证（显示器确实切走） |
| 显示器被另一台机器占用时仍可读可写 | ✅ 实机验证 |
| 「把显示器抢回来」的持续重发（`dmux hold`） | ✅ 逻辑跑通，实机双向验证待另一台机器配合 |
| 亮度调节 | ✅ 实机验证 |
| GPUI 图形界面 | ✅ 实机验证（gpui 1.24，截图确认文字、端口列表、当前源高亮） |
| 菜单栏常驻（托盘） | ✅ 实机验证（截图确认图标、菜单内容、勾选、点击派发） |
| 全局快捷键 / 开机自启 | ⬜ 未做 |
| Windows / Linux 后端 | ⬜ 未做（架构已预留，见下） |

唯一实测平台：**macOS 27 / Apple M4**，显示器 R32U81（Mac 接 USB-C，另一台接 DP）。

---

## 快速开始

```bash
# 构建命令行工具（不含 GUI，依赖少、构建快）
cargo build --release --bin dmux --bin dmux-ddc-helper

# 看有哪些显示器和端口
./target/release/dmux list
./target/release/dmux status

# 切换（支持别名：hdmi1 / dp2 / usbc1，也支持 0x11 或 17）
./target/release/dmux set hdmi1

# 在 10 秒内反复重发，直到本机这一路被选中
./target/release/dmux hold dp2 --every=2 --for=10
```

`dmux-ddc-helper` 必须和 `dmux` 放在同一目录（或放在 `PATH` 里，或用环境变量
`DMUX_DDC_HELPER` 指定绝对路径）。

图形界面：

```bash
cargo build --release --features gui --bin dmux-gui
./target/release/dmux-gui
```

---

## ⚠️ 屏幕黑了怎么办

**DDC 输入切换是 fire-and-forget 的。** 如果你切到了一个**没有信号**的输入
（对方机器关机、线没插、或者你切错了口），显示器可能**停在那一路显示"无信号"**，
而不是自动回切——这时你反而看不到自己的屏幕了。

恢复顺序：

1. **按显示器上的输入源按钮**（或 OSD → Input），选回本机那一路。最快、最可靠。
2. 显示器的输入项在本文里都用**原始 VCP 值**标注（如 `0x10`），方便和 OSD 对号。
3. 如果按不到（例如显示器在远处），试电源重插。
4. 也可以用脚本重发（**前提是 DDC 通道还活着**，见下）：

```bash
tools/recover-input.sh 0x10 20     # 反复把输入抢回 0x10，每个尝试用新进程
```

> **为什么脚本可能也没用**：一次 DDC 写入会把该进程的 I2C 通道弄坏
> （见 [hardware-notes 2.1](docs/hardware-notes.md)），而且当显示器正在显示别的
> 输入时，读回会失败。所以恢复脚本必须**每 2 秒换一个新进程**重发——这也是它
> 存在的原因。但**如果通道整体处于坏态，任何软件手段都无效**，只能走 OSD。

**给使用者的建议**：切之前先确认目标输入**确实有信号源开着**。本工具不会替你
判断这一点——它只知道显示器声明了哪些口。

---

## 设计要点（都是被硬件逼出来的）

完整实测记录见 **[docs/hardware-notes.md](docs/hardware-notes.md)**，这里只列结论：

1. **一次操作一个进程。** 在 Apple Silicon 上，一个进程里「写完再读」会把 DCP 的
   I2C 通道弄坏（`MacOS kernel I/O error 0x10000003`），且**进程内无法恢复**。
   所以写入走一次性进程（`dmux-ddc-helper set-input`），用完即弃。

2. **读取则共用常驻进程。** 实测读不会破坏通道（8/8 稳定），因此 `dmux-ddc-helper
   serve` 常驻，把 UI 刷新从「每次开进程 + I/O Kit 枚举」的约 190ms 降到约 116ms。

3. **新句柄的第一次事务必失败。** `serve` 启动时先做一次预热事务并重试，避免调用方
   看到 `invalid DDC/CI length`。

4. **不把"读回等于目标值"当成功判据。** 显示器切走后本机读不回来，那是**预期行为**。
   `SwitchOutcome::verdict()` 明确把「写入被 ACK 但读不回」判为成功。

5. **`hold` 是抢回显示器的关键。** 目标输入没有信号时显示器会自动回切，所以要周期
   重发请求；一旦另一台机器停止驱动它的输入，显示器就会稳定停在 Mac 这一路。

6. **DDC 写入被 ACK ≠ 显示器照做。** 所以措辞是 "sent / acknowledged"，不谎称已切换。

架构分层：

```
dmux-gui (GPUI)  ─┐
                  ├─→  displaymux::ddc  ─→  dmux-ddc-helper（一次性 / 常驻）
dmux (CLI)       ─┘        │                        │
                           │                        └─→ ddc-hi / ddc-macos
                           └─ 超时、重试、切换裁决
                                 displaymux::vcp ── VCP 值映射 + capabilities 解析
```

**GUI 侧线程模型**：`ddc::Reader` 持有 `std::process::Child`，因此**不是 `Send`**，
必须整个活在专属工作线程里；UI 线程只通过 channel 通信，并用定时器轮询结果，
**绝不阻塞在 DDC 调用上**（一次读要 120–200ms）。

---

## 目录结构

```
src/
  lib.rs                     crate 入口：导出下面三个模块
  vcp.rs                     MCCS/VCP：输入源命名、capabilities 解析、别名解析
  ddc.rs                     监督层：进程管理、超时、重试、切换裁决
  tray.rs                    菜单栏常驻：状态项、菜单、点击派发
  bin/
    dmux.rs                  命令行界面
    dmux-ddc-helper.rs       DDC 操作执行器（serve = 常驻只读模式）
    dmux-gui.rs              GPUI 图形界面
rust-toolchain.toml          锁定 Rust 1.99.0（gpui 1.24 需要新 std API）
docs/hardware-notes.md       硬件实测笔记（改 ddc 层前必读）
docs/gpui-versioning.md      gpui 版本策略与社区调研
tools/memwatch.sh            测量进程内存峰值/稳态
tools/mem-baseline.sh        更可信的内存口径（含 PID 身份校验）
tools/recover-input.sh       切到无信号输入后，把显示器抢回来
```

### 菜单栏常驻

`dmux-gui` 会在菜单栏放一个状态项，关掉窗口也继续运行（`QuitMode::Explicit`），
菜单里可以直接切信号源：

```
R32U81 - 0x10 DisplayPort 2
─────────────────────────────
Refresh
Reclaim DisplayPort 2
DisplayPort 1
✓ DisplayPort 2          ← 当前输入
HDMI 1
HDMI 2
─────────────────────────────
Show window
Quit dmux
```

实现上有两个约束（都写在 [src/tray.rs](src/tray.rs) 里）：macOS 要求状态项在
**主线程且事件循环已运行**时创建，所以它建在 GPUI 的 `run` 闭包内；`tray-icon`
的点击走全局回调，因此转发进 channel、由 GPUI 定时器在**主线程**轮询，保证所有
UI 变更都发生在 GPUI 线程上。

---

## 平台支持

| 平台 | 后端 | 状态 |
|---|---|---|
| macOS (Apple Silicon) | `ddc-macos` → 私有 `CoreDisplay`/`IOAVService` | ✅ 实测可用 |
| macOS (Intel) | `ddc-macos` → `IOFramebufferI2CInterface` | 未验证 |
| Windows | Monitor Configuration API（`SetVCPFeature`） | 未做 |
| Linux | `ddcutil` / `/dev/i2c-*` | 未做 |

「一次操作一个进程」和「预热」这两条补偿**只对 macOS 后端有意义**；Windows/Linux
预期没有「写完即坏」的问题，加进后端层实现时不要无差别照搬（见 hardware-notes 第 5 节）。

> macOS 上内建 HDMI 口在部分机型不支持 DDC（`ddc-macos` 与 `m1ddc` 都如此），
> 外接显示器走 USB-C / DP 才可靠。

---

## 开发

```bash
cargo test                     # VCP 解析与别名解析的单元测试
cargo build --release --bin dmux
cargo clippy --all-targets
```

### 构建 GPUI 的注意事项

依赖在 **gpui 1.24** 上，用社区分发版 `gpui-unofficial`（按 Zed release tag
自动同步发布），并**用 `=` 精确锁版本**——gpui 的 minor 号来自 Zed 的 release
tag，**没有兼容性含义**：

```toml
gpui = { package = "gpui-unofficial", version = "=1.24.1-pre", default-features = false, optional = true }
gpui_platform = { package = "gpui-platform-gpui-unofficial", version = "=1.24.1-pre",
                  default-features = false, features = [
    "font-kit",         # 否则 macOS 上完全没有文字
    "runtime_shaders",  # macOS 没有完整 Xcode 时必需
], optional = true }
```

三个容易踩的点：

- **两个 feature 都挂在 `gpui_platform` 上，不在 `gpui` 上。** 1.24 起平台后端
  拆成了独立 crate，这两个 feature 都转发给 `gpui_macos`。

- **`font-kit`**：它在 gpui 的 **default feature 集合**里，所以一旦写
  `default-features = false` 就会被静默关掉，macOS 后端随即回落到
  `NoopTextSystem` —— **窗口、布局、配色全部正常，但一个字形都不画**。
  这个坑不会以编译错误或运行时报错的形式出现，Zed 官方也踩过
  （[zed#59241](https://github.com/zed-industries/zed/pull/59241)）。

- **入口 API 变了**：`Application::new()` 已不存在，平台实例改为显式传入，
  所以入口是 `gpui_platform::application().run(..)`。

> **排查口诀**：界面正常但没有文字 → 先跑
> `cargo tree -e normal | grep -c font-kit`，`0` 就是病因。

`rust-toolchain.toml` 锁了 **1.99.0**。这不是跟风：gpui 1.24 用到
`slice_as_array` 等新 std API，旧工具链会直接编译失败。换 gpui 版本或改用
fork 之前，请先读 [docs/gpui-versioning.md](docs/gpui-versioning.md)。

完整的实测记录见 [docs/hardware-notes.md](docs/hardware-notes.md#26-default-features--false-会静默关掉整个文字渲染)。

---

## 路线图

- [ ] GPUI 界面闭环：端口列表、当前源高亮、切换、抢回
- [x] 菜单栏常驻（托盘 + 菜单，可切源）
- [ ] 全局快捷键
- [ ] 「另一台机器关机就自动切回来」的自动策略（轮询 + hold）
- [ ] Windows 后端
- [ ] Linux 后端（ddcutil）
- [ ] 内存/启动基准测试（`tools/memwatch.sh` 已就位）

## 许可

MIT，见 [LICENSE](LICENSE)。
