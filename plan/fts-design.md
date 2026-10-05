# 高速文件传输 CLI 工具 — 开发规划与设计文档

> **项目代号**:`fts`(File Transfer System)
> **文档版本**:v1.1
> **日期**:2026-10-05
> **状态**:设计草案,已按确认约束修订
> **目标读者**:架构师、核心开发者、项目评审人
>
> **v1.1 修订要点**:依据确认约束(**Windows 为唯一平台**、**Tailscale 组网**、**GB 级文件**、**点对点**、**CLI + 可编程接口**)重写"性能工程""技术选型""降级方案"三节,并新增 Windows 与 Tailscale 专属分析。v1.0 中面向 Linux 的方案(`io_uring` / `sendfile` / `splice` / `monoio`)已移除,原因见第 2.2 节。

---

## 目录

- [1. 概述](#1-概述)
- [2. 需求确认与平台约束](#2-需求确认与平台约束)
  - [2.1 已确认约束](#21-已确认约束)
  - [2.2 Windows 平台约束分析](#22-windows-平台约束分析)
  - [2.3 Tailscale 组网约束分析](#23-tailscale-组网约束分析)
  - [2.4 关键取舍](#24-关键取舍)
- [3. 背景:为什么能比 SMB 快](#3-背景为什么能比-smb-快)
- [4. 总体架构](#4-总体架构)
- [5. 传输协议设计](#5-传输协议设计)
- [6. 性能工程(Windows)](#6-性能工程windows)
- [7. 多接口方案](#7-多接口方案)
- [8. 技术选型](#8-技术选型)
- [9. 跨国与降级方案](#9-跨国与降级方案)
- [10. 日志与可观测性方案](#10-日志与可观测性方案)
- [11. 可靠性机制](#11-可靠性机制)
- [12. 测试与验收](#12-测试与验收)
- [13. 开发路线图](#13-开发路线图)
- [14. 风险与权衡](#14-风险与权衡)
- [15. 附录](#15-附录)

---

## 1. 概述

### 1.1 项目定位

`fts` 是一个用 Rust 编写的高速文件传输工具,面向 **Windows 平台**。它以**局域网**为主战场,辅以**跨国场景(经 Tailscale 组网)**,在极端网络下具备自动降级能力。它既可作为命令行工具使用,也可被其他程序集成调用。

### 1.2 核心目标与硬约束

| 类别 | 内容 |
|---|---|
| **主打** | 极致传输速度,显著快于 SMB |
| **硬约束 1** | **禁止使用无校验的裸 UDP 直传** |
| **硬约束 2** | 保留端到端完整性校验,校验不可关闭 |
| **硬约束 3** | 支持跨国极端网络,提供多级降级,确保尽量成功 |
| **硬约束 4** | 提供完整日志管理方案,可调试、可复现 |
| **已确认约束** | Windows 唯一平台;Tailscale 组网;GB 级文件;点对点;CLI + 可编程接口 |

### 1.3 结论先行

四条硬约束共同指向同一个技术答案:

> **快 + 必须可靠 + 不能用裸 UDP 无校验直传 → 采用 QUIC 作为默认传输层。**

QUIC 运行在 UDP 之上,但**自带可靠传输、逐包完整性保护、TLS 1.3 加密与拥塞控制**。它既获得 UDP 的多流并发、无队头阻塞、0-RTT 优势,又不违反"不能无校验直传"的底线。因此本项目**不自研裸 UDP 协议**,而是:

- **QUIC** 作为默认传输层;
- **TCP** 作为受限网络回退;
- 传输层通过 trait 抽象,便于后续替换。

> **⚠️ 重要预期管理(Windows 现实)**:由于平台限定为 Windows,**v1.0 设想中的 Linux 零拷贝快路径全部不可用**(无 `io_uring`、无 `sendfile`/`splice`)。这意味性能上限低于 Linux 实现,且 **在干净局域网传 GB 级单文件时,对 SMB3 的优势可能有限**。真正的差异化优势集中在三处:**跨国 Tailscale 链路、丢包韧性、以及可编程接口**。详见第 2.2、3.3 节。

---

## 2. 需求确认与平台约束

### 2.1 已确认约束

| 维度 | 确认结论 | 对设计的影响 |
|---|---|---|
| 主要场景 | **局域网为主**,辅以跨国(经 Tailscale) | 优化重点为 LAN 吞吐;跨国走降级 |
| 网络层 | **Tailscale 组网**(基于 WireGuard) | NAT/中继由 Tailscale 承担,本工具不建 P2P |
| 文件特征 | **GB 级大文件** | 分块流式传输、条纹化、断点续传;无需小文件批处理 |
| 平台 | **Windows(唯一)** | 运行时与 I/O 方案全面改写(见 2.2) |
| 并发规模 | **点对点为主** | 无需大规模连接调度,架构可简洁 |
| 接口 | **CLI + 可编程接口** | 见第 7 节 |
| 安全合规 | 需加密(韩国《个人信息保护法》对个人信息传输要求加密) | QUIC 内建 TLS 1.3 满足 |

### 2.2 Windows 平台约束分析

这是 v1.1 相对 v1.0 的最大变更。Windows 的 I/O 与网络模型与 Linux 差异根本,原方案中的以下技术**在 Windows 上不可用**:

| v1.0 设想的 Linux 技术 | Windows 状态 | 替代方案 |
|---|---|---|
| `io_uring` | ❌ 不存在(Linux 专属) | **IOCP**(I/O 完成端口) |
| `sendfile` / `splice` | ❌ 不存在 | overlapped I/O + 大缓冲;`TransmitFile` 仅限裸 TCP |
| `monoio` 运行时 | ❌ 仅 Linux/mio 回退,非真正的完成式后端 | **tokio**(IOCP)或 **compio**(IOCP 后端) |
| `SO_REUSEPORT` | ❌ 语义不同 | 单 socket + 多线程处理 |
| GSO/GRO(UDP 卸载) | ⚠️ 对应 USO/URO,但**实测不可靠** | **默认关闭**,谨慎可选(见下) |

**Windows 的可用能力:**

- **IOCP** 是 Windows 异步 I/O 的基石,提供高效的多路复用线程模型([Microsoft 文档](https://learn.microsoft.com/en-us/windows/win32/fileio/i-o-completion-ports))。`tokio` 在 Windows 上即通过 IOCP 实现异步。
- **compio** 是 thread-per-core 运行时,Windows 后端基于 **IOCP**(Linux 用 io_uring),可作为高性能可选项([compio](https://github.com/compio-rs/compio)、[compio-driver](https://lib.rs/crates/compio-driver))。
- **文件读取**:overlapped I/O + 大缓冲 + `FILE_FLAG_SEQUENTIAL_SCAN`;并行读多个分块以压满磁盘与网络。
- **UDP 卸载(USO/URO)存在但不可信**:Linux 的 GSO/GRO 在 Windows 对应 USO/URO([Microsoft USO 文档](https://learn.microsoft.com/en-us/windows-hardware/drivers/network/udp-segmentation-offload-uso-))。但 Mozilla 在 Firefox 中**实测后回滚了这两项**:URO 在 Windows on ARM + WSL 下返回错误的分段长度,URO 被禁用;USO 导致**丢包增加**,甚至有个别用户报告**网卡驱动崩溃**([Firefox UDP I/O 实践](https://max-inden.de/post/fast-udp-io-in-firefox/))。
  - **结论**:`fts` **默认不使用** USO/URO,作为**实验性开关**提供,且必须能一键回退。

> **直接后果**:Windows + QUIC 下,**文件到网络的真正零拷贝不可实现**——QUIC 必须在用户态对每段数据加密并分包,数据必然经手用户态缓冲。我们能做的是**减少拷贝次数、并行读、加大缓冲、用硬件 AES 加速加密**,而不是消除拷贝。

### 2.3 Tailscale 组网约束分析

Tailscale 基于 WireGuard 构建 mesh 网络,这对本工具是**双重影响**:

**利好(简化设计):**

- **NAT 穿透与中继由 Tailscale 全权负责**。因此 v1.0 中的"方案 E:通道降级(直连→中继→摆渡)"**大幅简化**——我们只需连接对端的 Tailscale IP(`100.64.0.0/10` 网段),直连或 DERP 中继由 Tailscale 自动决定。
- 局域网内 Tailscale 会建立**直连**,性能接近原生。

**代价(新增约束):**

| 约束 | 说明 | 应对 |
|---|---|---|
| **MTU 更小** | Tailscale 默认 MTU 约 **1280**,远低于以太网 1500 | QUIC 的 PMTU 探测**上限下调至 1280**,不盲目探测至 1500 |
| **用户态开销** | Windows 上 Tailscale 使用**用户态 WireGuard**(WinTun 驱动),每包需穿越内核↔用户态 | 用**大分块**减少包数量;接受 CPU 开销 |
| **双重加密** | WireGuard(ChaCha20-Poly1305)+ QUIC(TLS 1.3)两层加密 | 接受;用硬件加速;或评估内层复用会话密钥 |
| **DERP 中继** | 跨境打洞失败时走 DERP 中继,**延迟更高、带宽受限** | 探测须能识别"是否在走中继",作为降级触发信号 |
| **尾部延迟** | 用户态 WireGuard 在小包高频下有明显每包开销 | 避免小包;关闭不必要的探测包 |

> Tailscale 官方数据显示,其用户态实现的性能高度依赖 **MTU 与系统调用频率**——提高 MTU 减少系统调用可带来数倍吞吐提升([Tailscale 吞吐改进](https://tailscale.com/blog/throughput-improvements))。在 Linux 上 Tailscale 已借 GSO/GRO 突破 10Gb/s([Tailscale more-throughput](https://tailscale.com/blog/more-throughput));**但 Windows 上的用户态路径与 UDP 卸载均受限**,这是本项目性能天花板的主要来源。

### 2.4 关键取舍

1. **Windows 单一平台**:放弃 Linux 快路径,换取交付聚焦。收益是只需维护一套平台代码;代价是性能上限较低。
2. **QUIC 双重加密**:在 Tailscale 之上再套 TLS 1.3,CPU 成本翻倍,但换来可靠性、多流与协议独立性。可评估"内层轻量化"但**不推荐自研传输**。
3. **不自研 P2P**:直接复用 Tailscale 的连接与中继能力,把复杂度交给成熟组件。
4. **GB 级大文件**:优化方向明确为"吞吐 + 断点续传",不必为海量小文件做批处理设计。

---

## 3. 背景:为什么能比 SMB 快

### 3.1 SMB 的性能代价

- **协议啰嗦**:打开/读写/关闭每个操作都要往返,元数据(lease/oplock/ACL)开销大。
- **CPU 负担重**:签名(SMB Signing)与加密显著消耗 CPU。
- **往返延迟敏感**:10GbE+ 下瓶颈常是往返延迟与 CPU,而非带宽([社区讨论](https://www.reddit.com/r/synology/comments/1aolirp/smb_direct_rdma/))。
- **只有 SMB Direct(RDMA)才真正快**:绕过 TCP/IP 栈,微秒级延迟、低 CPU,但需要 RDMA 网卡([Tuxera 文档](https://docs.tuxera.com/docs/legacy-docs/how-to-articles/smb-multichannel-and-smb-direct))。

### 3.2 SSH 系工具更慢

ESnet 官方测试:**88ms RTT、100Gbps 容量下,scp/sftp/rsync 仅约 32 Mbps,而单流 HTTP 达 5.2 Gbps、4 流 Globus 达 9.5 Gbps**——差距超 100 倍。原因:SSH 内置 2MB 缓冲、无并行,且**默认不做文件级端到端校验**([ESnet Fasterdata](https://fasterdata.es.net/data-transfer-tools/say-no-to-scp/))。

### 3.3 差异化定位(据实修正)

| 工具 | 传输 | 并发 | 端到端校验 | 目标场景 |
|---|---|---|---|---|
| SMB3 | TCP | 有限(multichannel) | 部分 | 通用文件共享 |
| scp/sftp | TCP(SSH) | 无 | ❌ | 兼容性 |
| **本项目 `fts`** | **QUIC(多流)** | ✅ | ✅(BLAKE3) | **高吞吐 + 可编程接口** |

> **必须诚实面对的现实**:在**干净局域网传 GB 级单文件**这一最理想场景下,SMB3(尤其启用 Multichannel / SMB Direct)本已接近线速,`fts` 的领先幅度**可能不大**,甚至因 Windows + QUIC 的用户态开销而互有胜负。
>
> `fts` 真正的、可观测的领先出现在:
> 1. **跨国链路**:SMB 几乎无法跨越公网,Tailscale + QUIC 多流 + BBR 才是主战场;
> 2. **丢包/高延迟**:QUIC 的抗队头阻塞与 FEC 带来数量级改善;
> 3. **CPU 效率**:无 SMB 的元数据往返与签名开销;
> 4. **可编程接口**:SMB 无法被程序内嵌调用。
>
> 因此本项目应**以"跨国 + 可编程"为卖点**,而非笼统宣称"LAN 上比 SMB 快 N 倍"。

---

## 4. 总体架构

```
┌──────────────────────────────────────────────────┐
│  接口层 (Interfaces) — Windows 优先                │
│  CLI · C-ABI DLL · 命名管道 · gRPC/HTTP · pyo3    │
├──────────────────────────────────────────────────┤
│  核心库层 (Core Library, 纯 Rust)                  │
│  session · transfer · protocol · checksum · resume│
├──────────────────────────────────────────────────┤
│  探测与决策层 (Path Probe & Degradation Policy)    │
│  RTT/丢包/带宽探测 · 降级状态机 · 中继识别          │
├──────────────────────────────────────────────────┤
│  传输层 (Transport, trait 抽象可插拔)              │
│  QUIC(默认) · TCP(回退) · 命名管道(同机)          │
└──────────────────────────────────────────────────┘
                     │
                     ▼
┌──────────────────────────────────────────────────┐
│  Tailscale (WireGuard) —— 组网/NAT/中继(外部)      │
└──────────────────────────────────────────────────┘
```

**关键设计:**
- 传输层用 `trait Transport` 抽象,核心库不依赖具体协议;QUIC 默认,TCP 回退,同机进程走命名管道。
- **新增"探测与决策层"**:显式承担路径特征探测与降级决策,与传输解耦(见第 9 节)。
- **Tailscale 视为外部网络层**:本工具不实现 NAT 穿透/中继,只连接 Tailscale 地址并**探测其链路质量**。

---

## 5. 传输协议设计

### 5.1 为什么是 QUIC

| 约束/担忧 | QUIC 的答案 |
|---|---|
| 不能无校验 | 每包完整性保护,TLS 1.3 AEAD 防篡改 |
| 不能丢包不重传 | 可靠传输,自带重传与拥塞控制 |
| 要快 | 多流并行、无队头阻塞、0-RTT |
| 要加密 | TLS 1.3 内建,满足合规 |

> Quinn 是纯 Rust 的 QUIC 实现,支持双向流 / 单向流 / 不可靠数据报([Quinn Data Transfer](https://quinn-rs.github.io/quinn/quinn/data-transfer.html))。其中"不可靠数据报"本项目**不使用**,只走可靠流,以符合硬约束 1。

### 5.2 流模型

- **1 条控制流**(bidirectional):协商文件名、大小、分块表、校验方式、续传偏移。
- **N 条数据流**(unidirectional):按固定大小块(**8~32MB**,因 GB 级大文件可放大)条纹化并行发送。N 依 BDP 自适应。
- 每条流独立重传,**单流丢包不拖累其他流**。

### 5.3 完整性(不可关闭)

- **分块哈希**:每块算 BLAKE3,写入 manifest。
- **整体哈希**:文件级 BLAKE3,或 Merkle 树根。
- **Merkle 树**:支持并行校验、断点续传只重传缺失块、接收端边收边校验。
- **BLAKE3** 采用 SIMD 多线程,校验速度可达 10GB/s+,不构成瓶颈。

### 5.4 协议版本化

首包携带协议版本 + 能力位图,支持未来扩展而不破坏兼容。

### 5.5 MTU 与分块(Tailscale 感知)

- **PMTU 上限下调至 1280**(Tailscale 默认 MTU),探测失败时回退 1200(QUIC 最低安全 MTU)。
- 大分块(8~32MB)在 MTU 更小的 Tailscale 链路上**尤其重要**——块越大,单位数据的包头与决策开销越低,可抵消每包开销。

---

## 6. 性能工程(Windows)

> **本节为 v1.1 全面重写。** 原 Linux 方案(`io_uring` / `sendfile` / `splice` / thread-per-core)在 Windows 上不可用,已替换为 Windows 等价手段。

| 层次 | Windows 手段 | 收益 |
|---|---|---|
| 1. 减少往返 | QUIC 0-RTT、流水线发送、**大分块(8~32MB)** | 高 RTT 下最关键 |
| 2. 减少拷贝 | overlapped I/O + 大缓冲 + `FILE_FLAG_SEQUENTIAL_SCAN`;避免中间缓冲 | 降低内存带宽与 CPU |
| 3. 异步 I/O | **IOCP**(tokio 在 Windows 的原生后端) | 高并发重叠 I/O |
| 4. 并行读盘 | 多分块并行读取以压满磁盘吞吐 | 避免磁盘成为瓶颈 |
| 5. 并行校验 | BLAKE3 多线程 SIMD | 校验不拖后腿 |
| 6. 加密加速 | 依赖硬件 AES-NI / ChaCha20 SIMD(由 ring/quinn 自动使用) | 抵消双重加密成本 |
| 7. 可选 runtime | **compio**(thread-per-core,IOCP 后端) | 减少调度抖动 |
| 8. 可选 UDP 卸载 | USO/URO **实验性**,默认关闭 | 有收益但**实测不稳**,需可回退 |

**明确不可用(避免误设计):**

| 技术 | 原因 |
|---|---|
| `sendfile`/`splice` 零拷贝 | Windows 无此系统调用;且 QUIC 用户态加密使零拷贝天然不可行 |
| `TransmitFile` | 仅对**裸 TCP socket** 有效,无法用于 QUIC 的加密载荷 |
| `io_uring` | Linux 专属 |
| USO/URO 默认开启 | Firefox 实测回滚:丢包增加、驱动崩溃风险([来源](https://max-inden.de/post/fast-udp-io-in-firefox/)) |

**关键取舍:** 高速局域网下**压缩是负优化**,默认关闭;跨国拥塞时段可开 `zstd`。

### 目标性能(设计目标,非承诺;已按 Windows 下调)

| 场景 | 目标 |
|---|---|
| 10GbE LAN,GB 级大文件 | ≥ 60~80% 线速(受用户态 QUIC 与双重加密限制) |
| 1GbE LAN | 打满线速 |
| Tailscale 同城/国内 | 接近 Tailscale 直连上限 |
| Tailscale 跨国(直连) | 显著优于 SMB/scp;多流 + BBR 发挥价值 |
| Tailscale 跨国(DERP 中继) | 保成功优先,吞吐受中继限制 |
| 校验开销 | 吞吐损失 < 5% |

> 上述 LAN 数字**低于 v1.0 的 Linux 目标**(原为 90% 线速),这是平台约束的诚实反映,须在 P0 实测校准。

---

## 7. 多接口方案

按"**一套核心、多种外壳**"实现,并按 **Windows 优先**排序:

| 接口 | 技术 | 适用对象 | 优先级 |
|---|---|---|---|
| **CLI** | clap,子命令齐全 | 人类/脚本/PowerShell | P0 |
| **Rust 库** | crate,公开稳定 API | Rust 程序 | P0 |
| **C ABI DLL** | `cdylib` + cbindgen,导出 `extern "C"` | C/C++/C#/Go(P/Invoke) | P1 |
| **命名管道 IPC** | Windows Named Pipe | 同机多进程(最自然) | P1 |
| **本地 gRPC/HTTP** | tonic / axum,监听 localhost | 跨语言、前端 | P2 |
| **Python 绑定** | pyo3 + maturin | 数据/脚本生态 | P2 |

### 接口设计原则

- **同一份核心逻辑**,所有外壳仅薄封装。
- **异步 + 进度回调**:长传输须能查询进度、取消、恢复;gRPC 用 server-streaming 推送进度事件。
- **统一任务模型**:`submit → task_id → observe(cancel/resume/status) → result`,CLI、库、IPC、gRPC 共用同一语义。
- **DLL 导出稳定 C ABI**,便于 C#/C++ 直接集成。

### CLI 形态示例

```powershell
fts send .\data.bin --to 100.64.0.5:4433 --streams auto
fts recv --listen :4433 --out .\downloads
fts status <task_id>
fts daemon --pipe \\.\pipe\fts --grpc :50051
```

---

## 8. 技术选型

| 用途 | 推荐 crate | 说明 |
|---|---|---|
| 传输 | `quinn`(QUIC) | 纯 Rust、成熟;Windows 由 `quinn-udp` 支持 |
| 异步运行时 | **`tokio`**(默认,IOCP) | 通用稳定;Windows 原生 |
| 高性能运行时(可选) | **`compio`** | thread-per-core,IOCP 后端;仅在有明确收益时启用 |
| ~~io_uring 运行时~~ | ~~`monoio` / `glommio`~~ | **❌ Windows 不可用,移除** |
| 哈希 | `blake3` | SIMD 并行,极快 |
| 压缩 | `zstd` | 默认关闭,按需开启 |
| CLI | `clap` | |
| gRPC | `tonic` + `prost` | |
| HTTP | `axum` | |
| 序列化 | `serde` / `bincode` | 控制帧用 bincode |
| FFI | `cbindgen` | 生成 C 头文件 |
| 命名管道 | `tokio::net::windows::named_pipe` | Windows 原生 |
| Python | `pyo3` + `maturin` | |
| 错误 | `thiserror` / `anyhow` | |
| 观测 | `tracing` + `metrics` | |

### 工程结构(建议 Cargo workspace)

```
filetransfer/
├── crates/
│   ├── fts-core/        # 协议、会话、校验（无 IO 依赖，可测试）
│   ├── fts-probe/       # 路径探测 + 降级决策状态机
│   ├── fts-transport/   # QUIC / TCP / 命名管道 trait 实现
│   ├── fts-io/          # Windows overlapped I/O、并行读盘
│   ├── fts-cli/         # 命令行
│   ├── fts-ffi/         # C ABI (cdylib)
│   ├── fts-ipc/         # 命名管道服务
│   └── fts-grpc/        # 本地 gRPC/HTTP 服务
└── benches/             # 基准
```

---

## 9. 跨国与降级方案

### 9.1 设计总纲

跨国场景下,**"能成功"优先级高于"最快"**。降级与日志是同一个"探测-决策-记录"闭环的两端:

```
探测(path probe) → 决策(选档位) → 执行 → 记录(日志/指标) → 重估 → 再降级
                                        ↑___________________________|
```

三条原则:
1. **降级要有阶梯,不是开关**,且**可自动回升**(需滞回避免抖动)。
2. **校验永不可关闭**;降级只调整"怎么传",不调整"是否验"。
3. **日志必须能回答"为什么慢/为什么降级"**。

### 9.2 Tailscale 带来的简化(相对 v1.0)

- **无需自建 NAT 穿透与中继**:直接连接对端 Tailscale IP(`100.64.0.0/10`),由 Tailscale 决定直连或 DERP。
- **但需新增"链路类型识别"**:探测当前是**直连**还是**DERP 中继**——中继意味着高延迟、受限带宽,应作为降级触发信号。
- **MTU 上限按 1280 处理**(见 5.5)。

### 9.3 握手后的路径探测

握手只证明"能通",不证明链路质量。握手后立即探测(前 2~5 秒,期间用保守档位):

| 探测项 | 方法 | 用途 |
|---|---|---|
| RTT / 抖动 | QUIC 内建 RTT + 主动 ping | 判断长肥管道(LFN) |
| 丢包率 | 已知序列探测块 | 决定是否上 FEC |
| 带宽估计 | 主动探测 + 被动测量 | 决定流数与限速 |
| PMTU | DPLPMTUD,**上限 1280** | 避免分片 |
| 链路类型 | 延迟/路径特征推断直连 vs DERP | 触发降级 |
| UDP 可达性 | 握手是否成功 | 决定是否回退 TCP |

### 9.4 降级决策规则

```
IF udp_blocked OR quic_handshake_timeout   → 传输层降级 (方案 A)
IF link_type == DERP_relay                 → 下调流数、放宽超时 (方案 E')
IF loss_rate > 5% AND rtt > 100ms          → 启用 FEC   (方案 C)
IF loss_rate > 2%                          → 切 BBR     (方案 B)
IF retransmit_ratio > 15%                  → 减块尺寸   (方案 D)
```

### 9.5 方案 A:传输层降级

**触发**:QUIC 握手超时 / UDP 被防火墙阻断 / 持续丢包。

| 档 | 传输 | 说明 |
|---|---|---|
| A0 | QUIC (UDP 443) | 默认 |
| A1 | QUIC (UDP 自定义端口) | 换端口绕开封禁 |
| A2 | TCP 多连接 | 绕开单流窗口限制 |
| A3 | TCP 单连接 | 最兼容 |
| A4 | TCP + TLS(443) | 伪装 HTTPS,穿透严格网络 |

> Windows 防火墙常阻止入站 UDP,回退 TCP 是高频路径,必须有自动化测试。

### 9.6 方案 B:拥塞控制 + 流数自适应

- **BBRv3 ↔ CUBIC**:BBR 基于带宽探测而非丢包判断拥塞,在**高延迟 + 随机丢包**下显著优于 CUBIC([APNIC](https://blog.apnic.net/2020/01/10/when-to-use-and-not-use-bbr/));同条件实测 BBR 约 905 Mbps、CUBIC 约 860 Mbps([arXiv 2025](https://arxiv.org/html/2510.22461v1))。**但 BBR 丢包时重传量可能远高于 CUBIC**,深拥塞/浅缓冲下退回 CUBIC。
- **流数自适应**:N 流按 BDP 调整。高 RTT 下流数是提速关键([ESnet](https://fasterdata.es.net/data-transfer-tools/say-no-to-scp/)),但过高会互抢带宽,需收敛。

### 9.7 方案 C:抗丢包 —— ARQ + FEC(关键)

纯 ARQ 在"高 RTT + 丢包"下效率极低(每次丢包等一个 RTT)。解法是**前向纠错**:

- 用**喷泉码 / RaptorQ** 做应用层 FEC,发送端加冗余包,接收端无需重传即可恢复([Raptor 综述](http://www.conf-icnc.org/2012/Raptor%20overview%20for%20ICNC%2030Jan2012%20distribute.pdf))。
- **混合策略**:低丢包纯 ARQ;中高丢包 FEC + ARQ,冗余率自适应([ARQ vs FEC](https://www.haivision.com/blog/broadcast-video/low-latency-video-packet-loss-arq-fec/))。
- **代价**:FEC 无论有无丢包都占带宽([Patsnap](https://www.patsnap.com/resources/blog/articles/fec-vs-arq-protocols-for-wireless-communication/)),故仅在探测到丢包时开启。

> **回应硬约束 1**:FEC 是纠删码,不是放弃校验——用数学冗余保证可靠,接收端仍逐块验证 BLAKE3。

### 9.8 方案 D:块尺寸 / 速率 / MTU 降级

- **块尺寸**:32MB(低延迟高带宽)→ 8MB → 1MB → 256KB(高丢包)。
- **速率抑制(pacing)**:主动限速避免触发限流,反而提升有效吞吐。
- **MTU**:探测失败回退 1200。
- **压缩反转**:高速关压缩;跨境拥塞时段开 `zstd`。

### 9.9 方案 E':链路降级(Tailscale 感知)

| 档 | 链路 | 策略 |
|---|---|---|
| E0 | Tailscale 直连(同 LAN) | 全速,高流数 |
| E1 | Tailscale 直连(跨国) | 多流 + BBR + 适度 FEC |
| E2 | Tailscale DERP 中继 | 降流数、放宽超时、开 FEC、可开压缩 |
| E3 | Tailscale 不可用 | 临时回退公网直连+TCP |

> 相比 v1.0,v1.1 **删除了自建中继与摆渡方案**——中继由 Tailscale 的 DERP 提供,摆渡在点对点 Windows 场景下非必要。

### 9.10 方案 F:资源降级

CPU 吃满 / 内存不足 / 极小带宽时:降低校验并行度(仍全量校验)、降流数到 1、关闭非必要遥测。

### 9.11 降级阶梯汇总

| 档位 | 传输 | 拥塞控制 | 抗丢包 | 块尺寸 | 压缩 | 典型触发 |
|---|---|---|---|---|---|---|
| **L0 理想** | QUIC 多流 | BBRv3 | 纯 ARQ | 32MB | 关 | Tailscale 同 LAN |
| **L1 良好** | QUIC 多流 | BBRv3 | ARQ | 16MB | 关 | Tailscale 国内直连 |
| **L2 一般** | QUIC | BBRv3 | ARQ+FEC(低) | 8MB | 按需 | 跨国直连 |
| **L3 较差** | QUIC | BBRv3 | ARQ+FEC(高) | 4MB | 开 | 中高丢包 |
| **L4 恶劣** | QUIC→TCP | CUBIC | FEC | 1MB | 开 | UDP 被封 |
| **L5 极端** | TCP 单连接 | CUBIC | FEC+小 | 256KB | 开 | 严格封锁 |
| **L6 保底** | TCP+TLS | CUBIC | FEC+小 | 128KB | 开 | 全 UDP 阻断 |

> L0→L6 自动下探,亦须能**自动回升**(滞回防抖)。

---

## 10. 日志与可观测性方案

### 10.1 设计目标

日志要能回答:**传了什么?多快?为什么慢/降级?失败在哪一步?**

### 10.2 三平面分离

| 平面 | 内容 | 格式 | 用途 |
|---|---|---|---|
| **日志 Logs** | 离散事件、错误、降级决策 | 结构化 JSON(`tracing`) | 事后排查 |
| **指标 Metrics** | 吞吐、RTT、丢包、重传率、FEC 冗余率、校验速度 | 时序(Prometheus / 环形缓冲) | 实时观测 |
| **事件日志 Journal** | 协议状态机有序事件序列 | 追加写,可回放 | 精确复现 |

> `tracing` 是 Rust 结构化日志的事实标准,适合异步 + 上下文场景([tracing](https://docs.rs/tracing)、[实践指南](https://reintech.io/blog/structured-logging-rust-tracing-guide))。

### 10.3 上下文与关联 ID

每条日志携带:`session_id`、`transfer_id`、`peer`、`direction`、`trace_id`。由此可从"某文件传失败"追到"第 42 块第 3 次重传超时"。

### 10.4 日志级别策略

| 级别 | 默认? | 内容 |
|---|---|---|
| `error` | ✅ | 传输失败、校验失败、致命错误 |
| `warn` | ✅ | **每次降级、每次重连** |
| `info` | ✅ | 会话/文件开始结束、吞吐汇总 |
| `debug` | ❌(`-v`) | 每块传输、流调度、窗口调整 |
| `trace` | ❌(`-vv`) | 每包级(慎用) |

> **默认 `info` 须足以诊断 90% 问题**;降级事件**永远在 `warn`**,不会被默认关掉。

### 10.5 必须记录的关键事件

1. 每次**降级/回升**:`from=L0 to=L2 reason=loss_7.2pct rtt=145ms link=direct`
2. 每次**重传**:块 ID、第几次、耗时
3. 每次**校验**:块/文件哈希、耗时、结果
4. **路径快照**(定期):RTT、丢包、带宽、**链路类型(直连/DERP)**
5. **握手/回退细节**:哪步失败、超时值
6. **断点续传**:从哪续、checkpoint 位置

### 10.6 脱敏(合规)

韩国《个人信息保护法》(PIPA)对个人信息传输有加密与最小化要求:

- **路径脱敏**:完整路径默认不进日志,只记哈希或末段名。
- **内容永不出现**:只记哈希。
- **凭据永不记录**。
- 诊断包导出前**二次脱敏**。

### 10.7 轮转与保留

按大小轮转(如 64MB)、保留 N 份;**异步 + 环形缓冲**,磁盘满时丢弃 `debug/trace` 但**保留 `warn/error`**。绝不因写日志拖慢传输。

### 10.8 诊断包(一键排障)

```powershell
fts diagnose --task <id> --out diag.zip
```

含:脱敏日志 + 指标快照 + 事件日志 + 系统信息(Windows 版本/网卡/MTU)+ **Tailscale 状态(直连/DERP)** + 路径探测结果 + 本次降级历史。

### 10.9 事件日志可回放

```powershell
fts replay diag.journal        # 离线重放，还原状态机决策
```

### 10.10 panic 与崩溃

`panic hook` 崩溃前刷入上下文;结合 `backtrace`;崩溃后由 checkpoint 保证续传。

---

## 11. 可靠性机制

| 机制 | 说明 |
|---|---|
| **块级断点续传** | journal 记录已确认块,中断后只补缺失块 |
| **Checkpoint** | 每 N 块持久化状态,崩溃/断电可恢复 |
| **幂等重试** | 重试不产生重复文件 |
| **指数退避** | 重连退避,避免风暴 |
| **看门狗** | 长时间无进展 → 主动降级或重启会话 |
| **多级校验** | 块哈希 + 文件哈希,交付前必验 |
| **降级保底** | 阶梯一路下探到 L6,尽最大努力成功 |
| **原子落盘** | 临时文件 + 校验通过后原子重命名(Windows `MoveFileEx` 原子替换) |

---

## 12. 测试与验收

降级逻辑必须通过**主动注入故障**验证:

- **`tc netem` 仿真**(在 Linux 侧或 WSL 中模拟对端):`delay`、`loss`、`reorder`、`duplicate`、`limit`。
- **Windows 网络仿真**:可用 `clumsy` 或 `New-NetQosPolicy` 注入丢包/延迟。
- **Tailscale 专项测试**:分别在**直连**与**DERP 中继**两种链路下验证降级识别与策略切换。
- **降级矩阵测试**:L0~L6 每档验证能自动触发、正确传输、正常回升。
- **真实链路**:韩国 ↔ 中国(海底光缆,晚高峰拥塞)、韩国 ↔ 日本、韩国 ↔ 美国西海岸;云节点用 AWS `ap-northeast-2`(首尔)、`ap-northeast-1`(东京)、`us-west-2`。
- **验收标准**:注入 **5% 丢包 + 150ms RTT** 下传输仍成功,且降级决策被正确记录。
- **Fuzz**:协议解析(`cargo-fuzz`)。
- **性能回归 CI**:每 PR 跑固定基准,超阈值报警。

> 参考:QUIC 在丢包下明显优于 TCP 单流,有测试显示全双工 10% 丢包下仍保持 80% 以上性能([dgway 2025](https://dgway.com/blog_E/2025/08/13/quic-vs-tcp-which-performs-better-in-lossy-networks/)、[CACM](https://cacm.acm.org/research/taking-a-long-look-at-quic/))。

### 性能基准

- **对照基线**:`scp`、`rsync`、SMB3、`iperf3`(理论上限)。
- **矩阵**:GB 级文件 × 网络(1/10GbE)× 场景(LAN / 同城 / 跨国直连 / DERP)。
- **本地化**:KT / SKT / LG U+;Naver Cloud、KT Cloud、Kakao Cloud、AWS 首尔区。

---

## 13. 开发路线图

按 1~3 人小团队估算,约 **4~5 个月到 v1.0**(Windows 单一平台,省去跨平台成本)。

| 阶段 | 周期 | 目标 | 交付物 | 验收标准 |
|---|---|---|---|---|
| **P0 可行性** | 2~3 周 | 验证 Windows + QUIC 能否达标 | 最小原型:`send`/`recv` 单文件 | LAN ≥ 3Gb/s(先跑通再优化) |
| **P1 MVP** | 4~6 周 | 可用可靠 | 多流并发、BLAKE3 校验、断点续传、CLI、**探测引擎 + 降级状态机骨架**、结构化日志 | LAN ≥ 5Gb/s,校验全开,断线可续 |
| **P2 性能与降级** | 6~8 周 | 逼近平台上限 + 跨境可用 | IOCP 调优、并行读盘、**FEC(RaptorQ)+ TCP 回退 + BBR/CUBIC 切换 + Tailscale 链路识别** | LAN 达平台上限的 80%;跨境注入故障仍成功 |
| **P3 多接口** | 4~6 周 | 可编程 | C-ABI DLL + 命名管道 + 本地 gRPC/HTTP、daemon 模式、`fts diagnose` | C#/C++ 调用 demo 通过,进度/取消可用 |
| **P4 硬化发布** | 4~5 周 | 稳定 | fuzz、CI 基准、文档、v1.0 | 无严重 bug,基准有回归门禁 |

**建议**:先做 P0 把**真实数字**跑出来。Windows + QUIC + Tailscale 的实际性能尚未有公开基准,**这是最大的未知数**——P0 的数据将决定后续目标是否需要再次校准。

### 里程碑

| 里程碑 | 关键指标 |
|---|---|
| M1 原型跑通 | 端到端传 1GB,校验通过 |
| M2 MVP | LAN ≥ 5Gb/s,支持续传 |
| M3 性能与降级达标 | LAN 达平台上限 80%;跨境故障注入通过 |
| M4 接口完备 | CLI + DLL + 命名管道 + gRPC 可用 |
| M5 v1.0 | fuzz 通过 + 基准门禁 + 文档 |

---

## 14. 风险与权衡

| 风险 | 说明 | 缓解 |
|---|---|---|
| **Windows 性能上限低** | 无零拷贝、用户态 QUIC + 双重加密 | P0 实测校准目标;用大分块 + 硬件加密 |
| **USO/URO 不稳** | Firefox 实测丢包/驱动崩溃 | 默认关闭,仅作实验开关 |
| **Tailscale MTU 小(1280)** | 包数多、CPU 高 | 大分块;PMTU 上限 1280 |
| **DERP 中继带宽受限** | 跨境打洞失败时走中继 | 识别中继并降级;保成功优先 |
| **LAN 对 SMB 优势不明显** | GB 单文件下 SMB 已近线速 | **以跨国 + 可编程为卖点**,不夸大 LAN 领先 |
| UDP 被 Windows 防火墙封 | 常见 | 自动回退 TCP(A2~A4) |
| BBR 重传偏高 | 深拥塞下重传上升 | 浅缓冲/深拥塞退回 CUBIC |
| FEC 开销 | 干净链路无谓占带宽 | 按实测丢包率自适应 |
| 范围膨胀 | "多接口"易失控 | 按优先级分批,P0/P1 只做 CLI + 库 |

---

## 15. 附录

### 15.1 术语表

| 术语 | 含义 |
|---|---|
| QUIC | 基于 UDP 的可靠传输协议,内建 TLS 1.3 与多流 |
| Tailscale | 基于 WireGuard 的 mesh VPN,自动 NAT 穿透与中继 |
| DERP | Tailscale 的中继服务器,用于无法直连时转发 |
| WireGuard | 现代 VPN 协议,Tailscale 的底层 |
| IOCP | Windows I/O 完成端口,异步 I/O 基石 |
| USO / URO | Windows 的 UDP 分段/合并卸载(对应 Linux GSO/GRO) |
| BBR | 基于带宽模型的拥塞控制,适合高延迟随机丢包 |
| CUBIC | 基于丢包的拥塞控制,Linux 默认 |
| FEC | 前向纠错,用冗余数据恢复丢失 |
| RaptorQ | 喷泉码家族的高效纠删码 |
| ARQ | 自动重传请求 |
| BDP | 带宽时延积 |
| DPLPMTUD | QUIC 路径 MTU 探测 |
| BLAKE3 | 现代高速密码学哈希,SIMD 并行 |
| Merkle 树 | 分层哈希,支持并行/增量校验 |

### 15.2 参考资料

**传输与拥塞控制**
1. ESnet Fasterdata — [Say No to scp/sftp/rsync](https://fasterdata.es.net/data-transfer-tools/say-no-to-scp/)
2. Quinn — [Data Transfer 文档](https://quinn-rs.github.io/quinn/quinn/data-transfer.html)
3. APNIC — [When to use and not use BBR](https://blog.apnic.net/2020/01/10/when-to-use-and-not-use-bbr/)
4. arXiv (2025) — [Should BBR be the default TCP Congestion Control?](https://arxiv.org/html/2510.22461v1)
5. dgway (2025) — [QUIC vs TCP in Lossy Networks](https://dgway.com/blog_E/2025/08/13/quic-vs-tcp-which-performs-better-in-lossy-networks/)
6. CACM — [Taking a Long Look at QUIC](https://cacm.acm.org/research/taking-a-long-look-at-quic/)

**FEC / 纠错**
7. ICNC (2012) — [Raptor codes Application Layer FEC](http://www.conf-icnc.org/2012/Raptor%20overview%20for%20ICNC%2030Jan2012%20distribute.pdf)
8. Haivision — [ARQ vs FEC](https://www.haivision.com/blog/broadcast-video/low-latency-video-packet-loss-arq-fec/)
9. Patsnap — [FEC vs ARQ Protocols](https://www.patsnap.com/resources/blog/articles/fec-vs-arq-protocols-for-wireless-communication/)

**Windows 平台**
10. Microsoft — [I/O Completion Ports](https://learn.microsoft.com/en-us/windows/win32/fileio/i-o-completion-ports)
11. Microsoft — [UDP Segmentation Offload (USO)](https://learn.microsoft.com/en-us/windows-hardware/drivers/network/udp-segmentation-offload-uso-)
12. Mozilla — [Fast UDP I/O for Firefox in Rust](https://max-inden.de/post/fast-udp-io-in-firefox/)
13. compio — [thread-per-core runtime with IOCP/io_uring](https://github.com/compio-rs/compio)
14. quinn-udp — [docs.rs](https://docs.rs/quinn-udp)

**Tailscale**
15. Tailscale — [Userspace isn't slow, some kernel interfaces are!](https://tailscale.com/blog/throughput-improvements)
16. Tailscale — [Surpassing 10Gb/s with Tailscale](https://tailscale.com/blog/more-throughput)
17. Tuxera — [SMB Multichannel and SMB Direct](https://docs.tuxera.com/docs/legacy-docs/how-to-articles/smb-multichannel-and-smb-direct)

**可观测性**
18. `tracing` — [docs.rs](https://docs.rs/tracing)
19. Reintech — [Structured Logging in Rust with tracing](https://reintech.io/blog/structured-logging-rust-tracing-guide)

### 15.3 已确认事项(原待确认项)

| 编号 | 事项 | 结论 |
|---|---|---|
| Q1 | 大文件还是海量小文件? | **GB 级大文件** |
| Q2 | 是否 Linux 优先? | **Windows 唯一平台** |
| Q3 | 跨境主力链路? | **Tailscale 组网**;具体链路待 P0 实测 |

### 15.4 后续待定

| 编号 | 事项 | 影响 |
|---|---|---|
| N1 | 是否启用 compio(thread-per-core)作为可选运行时? | 需 P0 对比 tokio 后再定 |
| N2 | 双重加密是否接受?是否评估内层轻量化? | 影响 CPU 与安全模型 |
| N3 | LAN 场景是否需与 SMB Direct(RDMA)对照? | 影响基准设计 |

---

*文档结束*
