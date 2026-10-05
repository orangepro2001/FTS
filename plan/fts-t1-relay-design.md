# fts T1 单中继 — 详细设计

> **项目**:高速文件传输工具 `fts`
> **文档版本**:v1.0
> **日期**:2026-10-05
> **父文档**:`fts-design.md` v1.1(第 9 节降级方案)
> **范围**:T1(单中继)的完整设计,含两条实现路线、探测、选路、协议、可观测性与验收

---

## 目录

- [1. 结论与范围](#1-结论与范围)
  - [1.1 一句话结论](#11-一句话结论)
  - [1.2 设计不变量](#12-设计不变量)
- [2. 两条实现路线](#2-两条实现路线)
- [3. 触发:何时需要中继](#3-触发何时需要中继)
- [4. 探测:如何判定当前链路类型](#4-探测如何判定当前链路类型)
- [5. 中继选择算法](#5-中继选择算法)
- [6. T1.0 设计:Tailscale Peer Relay 集成(首选)](#6-t10-设计tailscale-peer-relay-集成首选)
- [7. T1.1 设计:fts 应用层中继(备选)](#7-t11-设计fts-应用层中继备选)
- [8. 完整性、安全与信任模型](#8-完整性安全与信任模型)
- [9. 失败处理与回退](#9-失败处理与回退)
- [10. 可观测性](#10-可观测性)
- [11. 接口设计](#11-接口设计)
- [12. 与降级阶梯的整合](#12-与降级阶梯的整合)
- [13. 测试与验收](#13-测试与验收)
- [14. 路线图插入点](#14-路线图插入点)
- [15. 风险与开放问题](#15-风险与开放问题)
- [16. 附录:默认参数表](#16-附录默认参数表)

---

## 1. 结论与范围

### 1.1 一句话结论

> **在 Tailscale 场景下,T1 的第一选择不是"自己写中继",而是"启用 Tailscale Peer Relays + 在 fts 里做探测/校验/编排"。** 只有当你在**没有 Tailscale** 的公网环境使用 fts 时,才需要自建应用层中继。

理由基于 Tailscale 官方实测数据:

| 路径 | 实测吞吐 | 来源 |
|---|---|---|
| DERP 中继(德里→明尼阿波利斯,跨洋) | **约 2.2 Mbit/s** | [Tailscale 跨国网络实测](https://tailscale.com/blog/peer-relays-international-networks) |
| **Peer Relay 中继(同一链路)** | **27~35 Mbit/s(12.5 倍)** | 同上 |
| 直连基线(同一 ISP 国际出口) | 约 31.1 Mbit/s | 同上 |

**含义**:Peer Relay 把吞吐拉回到"接近直连基线"的水平,而 DERP 只能给到 2.2 Mbit/s。这正是你的跨境痛点所在,也是 T1 的价值所在。

### 1.2 设计不变量

T1 不得破坏父文档已确立的硬约束:

| 不变量 | T1 中的体现 |
|---|---|
| **校验不可关闭** | 中继路径下 BLAKE3 分块哈希 + 文件哈希照常全量执行 |
| **禁止无校验 UDP 直传** | 中继转发的是可靠 QUIC 流,不是裸 UDP 数据报 |
| **失败可续传** | checkpoint + Merkle 树在换路径后照常工作 |
| **可调试** | 中继路径变更、选路决策全部记入日志与事件 journal |

---

## 2. 两条实现路线

T1 有两条**互补**(而非二选一)的路线:

| | **T1.0 网络层中继** | **T1.1 应用层中继** |
|---|---|---|
| 实现 | 复用 **Tailscale Peer Relays** | 自建 **fts-relay** 守护进程 |
| 代码量 | **近乎为零**(仅探测/编排) | 中等(rendezvous + 转发 + 选路) |
| 前提 | 节点在 tailnet 内 | 无需 Tailscale,公网可用 |
| 加密 | WireGuard 端到端(Tailscale 保证) | 每跳 QUIC TLS + 可选端到端 AEAD |
| 吞吐 | 接近直连 | 取决于中继节点带宽 |
| 成熟度 | 公开 beta,可视化尚在完善 | 自控,但需自测 |
| **默认** | ✅ **首选** | ⚠️ 仅当无 Tailscale 时启用 |

**选型规则:**

```
IF 节点在 tailnet 内:
    → T1.0(启用并校验 Peer Relay)
ELIF 需要公网中继(无 Tailscale):
    → T1.1(自建 fts-relay)
```

> **不要同时启用**。若已在 tailnet 内又自建应用层中继,等于在 Tailscale 的中继之上再套一层中继,徒增延迟与故障面。

---

## 3. 触发:何时需要中继

中继不是默认开启,而是**按探测结果触发**。触发信号分三类:

### 3.1 硬信号(确定性)

| 信号 | 判定方式 | 动作 |
|---|---|---|
| 当前链路为 **DERP 中继** | 见第 4 节探测 | 立即请求切至 Peer Relay |
| **UDP 被阻断**,直连不可达 | QUIC 握手失败 + Tailscale 报告 direct 失败 | 用中继(Peer Relay 或 DERP) |
| 硬 NAT / 对称 NAT | `tailscale netcheck` 的 `MappingVariesByDestIP=true` | 预期无法直连,主动走中继 |
| 直连**反复建立失败** | 连续 3 次直连尝试失败 | 切中继 |

### 3.2 软信号(性能)

| 信号 | 阈值(建议初值) | 动作 |
|---|---|---|
| 实测吞吐远低于中继预期 | `实测 < 中继可用带宽 × 0.7` | 尝试切换中继路径 |
| RTT 抖动高 | 抖动 > 3× 基线 | 记录并重估路径 |
| 重传率过高 | > 15% | 结合方案 D 降级 |

### 3.3 触发决策流

```
握手成功
   │
   ├─ 探测链路类型(第4节)
   │
   ├─ IF link == direct AND 直连质量良好 → 保持直连,结束
   │
   └─ ELSE(直连不可用 / 走 DERP / 硬 NAT)
         │
         ├─ 存在可用 Peer Relay?(tailnet 内)
         │     ├─ 是 → 切换并校验(T1.0)
         │     └─ 否 → 有无 fts-relay 候选?
         │              ├─ 有 → 走 T1.1
         │              └─ 无 → 维持 DERP,记录告警(提示部署中继)
```

---

## 4. 探测:如何判定当前链路类型

这是 T1 的**关键基础能力**——不知道当前走什么路径,就无法决策。三种手段,按可靠性排序:

### 4.1 手段一:`tailscale status --json`(首选)

Tailscale 官方文档明确:`tailscale status` 可区分三种连接类型([Connection types 文档](https://tailscale.com/docs/reference/connection-types)):

- 输出含 `direct` → 直连
- 输出含 `relay` → 经 DERP
- 输出含 `peer-relay` → 经 Peer Relay

JSON 输出中每个 peer 携带其**当前地址与 relay 信息**(`CurAddr` 非空通常表示直连,空表示被中继)。

> ⚠️ **实现注意**:具体 JSON 字段名以**本机 Tailscale 版本**为准,须在 P0 阶段实测确认并写进单元测试;不要硬编码未验证的字段名。

### 4.2 手段二:`tailscale ping`(校验用)

`tailscale ping <peer>` 的输出可直接判定路径:

| 输出特征 | 判定 |
|---|---|
| `via <ip>:<port>` | 直连 |
| `via DERP(xxx)` | DERP 中继 |
| peer relay 相关标识 | Peer Relay |

Tailscale 会在 ping 过程中**持续尝试升级**路径,因此多次 ping 可能看到 DERP → Peer Relay 的实时升级过程——这本身就是**校验 T1.0 生效**的黄金手段。

### 4.3 手段三:被动推断(降级兜底)

当无法调用 Tailscale CLI 时(精简部署、权限受限),用被动信号推断:

| 被动信号 | 推断 |
|---|---|
| RTT 突然从个位数/几十 ms 跳到数百 ms | 可能已滑落至 DERP |
| 吞吐骤降至个位数 Mbit/s | 可能已滑落至 DERP |
| 路径 MTU 变化 | 链路类型可能切换 |

> **注意**:被动推断不可靠(与丢包、拥塞混淆)。因此 **CLI 主动探测为首选**,被动推断仅作兜底,且必须打上 `confidence=low` 标记。

### 4.4 探测时机与频率

| 时机 | 动作 |
|---|---|
| 握手完成后 | 强制探测一次 |
| 传输进行中 | 每 **30s** 或每完成 **1GB** 探测一次 |
| 吞吐/RTT 异常时 | 立即触发探测 |
| 路径变更事件 | 记入 journal(`from=direct to=DERP`) |

> **性能保护**:探测不得干扰传输。`tailscale status` 涉及进程调用,应设 200ms 超时并异步执行,失败不阻塞传输。

---

## 5. 中继选择算法

### 5.1 候选来源

| 来源 | 说明 |
|---|---|
| Tailnet peer 列表 | 标记为 relay 能力的节点(T1.0) |
| 静态配置 | `relay_candidates = ["100.64.0.9:51820", ...]`(T1.1) |
| manifest 提示 | 源节点在 manifest 中推荐中继 |

### 5.2 评分函数

对每个候选中继 `R`,计算:

```
score(R) = RTT(A→R) + RTT(R→B)          # 主项:往返延迟之和
         + α · jitter(R)                 # 抖动惩罚
         + β · load_ratio(R)             # 负载惩罚(利用率)
         + γ · relay_penalty             # 若 R 本身也在走 DERP,重罚
```

选择 `argmin score(R)`,约束条件:

1. **A 与 B 都对 R 有 ACL 访问权限**(T1.0 必须,否则不可用);
2. R 可达且在线;
3. R 有容量余量(`load_ratio < 0.85`);
4. R 距两侧的 RTT 均在可接受范围。

**关键直觉**:选**往返延迟之和最小**的中继,而非"离某一端最近"。对韩国↔美国西海岸这类跨洋链路,中继应放在**双方共同的优质枢纽**(如东京、新加坡、美西),而非偏向任一端。

### 5.3 防抖(滞回)

频繁切换中继会造成吞吐抖动。规则:

```
仅当 新候选 score < 当前中继 score × 0.8  才切换   # 20% 迟滞
且 距上次切换 > 60s                                # 冷却期
```

### 5.4 中继不可用时的次序

```
1. 首选 Peer Relay(score 最优)
2. 次选其他 Peer Relay
3. Tailscale DERP(始终可用的基线)
4. 直连重试(后台持续尝试)
```

---

## 6. T1.0 设计:Tailscale Peer Relay 集成(首选)

### 6.1 原理

Tailscale Peer Relay 的工作机制([官方文档](https://tailscale.com/docs/reference/connection-types)、[beta 公告](https://tailscale.com/blog/peer-relays-beta)):

1. 一个连接性良好的节点被配置为中继,监听**单个 UDP 端口**;
2. 当两节点直连失败时(Tailscale 自动)**双方各自向中继发起入站 UDP 连接**;
3. 中继在内存中**双向转发**加密的 WireGuard 包,**从不解密**;
4. 偏好顺序**自动**为:直连 → Peer Relay → DERP;
5. 全程端到端 WireGuard 加密。

### 6.2 部署步骤(运维侧)

基于官方文档与实测博客([设置指南](https://tailscale.com/blog/peer-relays-international-networks)):

| 步骤 | 命令 / 动作 |
|---|---|
| 1. 在中继节点启用 | `tailscale set --relay-server-port=<port>` |
| 2. 路由器端口转发 | 将 UDP 端口由公网 IP 转发到中继节点 |
| 3. 配置 ACL 授权 | 在 tailnet policy 中允许设备使用该中继 |
| 4.(如需要)静态端点 | 自动发现失败时用 `--relay-server-static-endpoints` 显式指定 `IP:port` |

> **部署建议**:在你的跨境链路上放 **2 个中继**——免费额度即支持 **2 个 Peer Relay**(所有套餐,含免费版,永久有效)。建议放在**双方共同的优质枢纽**(如东京 + 美西),覆盖韩国↔亚洲、韩国↔美洲两个方向。

### 6.3 fts 侧需要做什么

fts **不实现转发**,只做四件事:

| 职责 | 说明 |
|---|---|
| **① 探测** | 第 4 节,判定当前是否走 DERP |
| **② 编排/提示** | 若 tailnet 无可用 Peer Relay,输出**可操作告警**(附部署命令),而非静默等待 |
| **③ 校验生效** | 切换后通过 `tailscale ping` 确认已升级到 peer-relay 路径 |
| **④ 记录** | 路径变更记入日志与事件 journal |

### 6.4 关键交互:检测到走 DERP 时

```
[warn] path=DERP relay_region=ord rtt=452ms throughput=2.1Mbps
[warn] no peer relay available on tailnet
[hint] enable one: tailscale set --relay-server-port=51820 on a well-connected node
[hint] then add a tailnet ACL grant so this device may use it
```

这条 hint 是**用户价值的关键落点**——把"为什么慢"直接翻译成"怎么修"。

### 6.5 ACK/校验流程

```
1. 记录切换前:link=DERP, rtt, throughput
2. 等待 Tailscale 自动升级(通常 1~2 个 ping 内)
3. tailscale ping 校验:link=peer-relay
4. 记录切换后:link=peer-relay, rtt, throughput
5. 记入 journal: {event: path_upgrade, from: DERP, to: peer_relay, gain: 12.5x}
```

---

## 7. T1.1 设计:fts 应用层中继(备选)

**仅当无 Tailscale 时启用。** 这是一套自带 NAT 穿透兜底的中继机制。

### 7.1 拓扑

```
   源 A ──┐                    ┌── 目标 B
          │  (各自向中继发起    │
          │   出站 QUIC 连接)   │
          └──► fts-relay ◄─────┘
              (rendezvous + 转发)
```

**核心设计:双方都只发起出站连接**——这是 NAT 友好性的来源(与 Tailscale Peer Relay 同一思路:不要求任一方可被入站连接)。

### 7.2 会合(rendezvous)协议

```
1. 源 A 生成 transfer_token = sign(源私钥, {xfer_id, chunk_manifest_hash, expire})
   并带外传给 B(经已有控制通道 / manifest)
2. A 连接 relay: HELLO {role=source, token}
3. B 连接 relay: HELLO {role=dest,   token}
4. relay 校验 token 签名与有效期,配对 A↔B,返回 PAIRED {session_id}
5. relay 开始双向转发
```

**token 设计要点**:
- **签名**:源私钥签名 → relay 与 B 都能验证真伪;
- **短有效期**(如 10 分钟)→ 防重放;
- **一次性**:配对成功后失效;
- **绑定 manifest 哈希** → 防止 token 被挪用到其他文件。

### 7.3 转发模式

| 模式 | 说明 | 取舍 |
|---|---|---|
| **(a) 连接终结式**(推荐) | relay 终结 QUIC,分别与 A、B 建立会话,转发**流字节** | 简单、NAT 友好、易调试;relay 可见明文 |
| (b) 数据报透明转发 | relay 转发 UDP 载荷,QUIC 端到端 | 保 E2E 加密;复杂,且与 Tailscale Peer Relay 职责重叠 |

**T1.1 默认采用 (a)**:因为 (b) 本质上是在重造 Peer Relay,而 Peer Relay 已由 Tailscale 提供。

### 7.4 relay 守护进程(fsd)

```
fts relay serve --listen :51820 --max-sessions 32 --max-bandwidth <limit>
```

职责:
- 接受出站会合连接、配对、转发;
- **不解析文件内容**,只转发字节流;
- 记录会话元数据(时长、字节数、路径)用于可观测性;
- 支持带宽上限与最大会话数(防滥用)。

### 7.5 多中继链

```
IF 单中继带宽不足:
    可将多个分块流分配到不同中继(stream-level relay striping)
```

> **重要**:这只是**路径聚合**(把不同流分到不同中继),**不是**父文档讨论的"协助节点转发 swarm"。它不违反流量守恒(每个中继仍只是转发同一份数据,源上行仍是上限),只是绕过多条劣化路径。

---

## 8. 完整性、安全与信任模型

### 8.1 完整性(不可绕过)

无论哪条路线,**BLAKE3 分块哈希 + Merkle 根**照常执行:

- 每个分块到达后立即校验分块哈希;
- 损坏分块 → **丢弃并重取**(可换中继重取);
- 传输完成校验文件级哈希。

> **中继无法伪造数据**:即使中继是恶意的,也无法通过哈希校验。这是 T1 相对"裸 UDP 直传"的根本安全优势。

### 8.2 信任模型

| 场景 | 中继是否可信 | 加密层次 |
|---|---|---|
| **T1.0(tailnet 内)** | 可信(Tailscale ACL 管控,W**ireGuard 端到端**)| WireGuard E2E(中继不解密) |
| **T1.1(tailnet 外)** | 默认**不完全可信** | 每跳 QUIC TLS + **可选端到端 AEAD** |

### 8.3 端到端加密选项(T1.1)

当 fts-relay 属于**不完全可信节点**时,启用端到端分块加密:

```
每个分块: XChaCha20-Poly1305(key = HKDF(transfer_token_secret), nonce = chunk_id)
```

- 密钥经带外通道协商,**不经中继**;
- 中继只见密文;
- **成本**:一次 AEAD 运算,硬件加速下近乎免费。

> **默认策略**:T1.0 关(已有 WireGuard);T1.1 在"中继非自有"时开。

### 8.4 合规

韩国《个人信息保护法》(PIPA)要求个人信息传输加密。两条路线均满足:**T1.0 为 WireGuard 端到端加密,T1.1 为 QUIC TLS + 可选内层 AEAD**。

---

## 9. 失败处理与回退

| 失败情形 | 处理 |
|---|---|
| 中继不可达 | 回退 DERP(T1.0)或直连重试(T1.1) |
| 中继中途掉线 | 用 checkpoint **续传**,并重选中继 |
| 中继过载 | 选路算法自动重选(滞回保护) |
| token 过期/被拒 | 重新会合,生成新 token |
| 中继返回损坏数据 | 哈希校验拒绝 → 换中继重取 |
| 全部中继失败 | 逐级降级至 L4/L5/L6(见第 12 节) |

**关键原则**:中继是**优化路径**,不是**依赖路径**。任何中继失败都不得导致传输失败——必须能回退,且能续传。

---

## 10. 可观测性

沿用父文档的**三平面**设计,补充 T1 专属内容。

### 10.1 必记事件

| 事件 | 级别 | 字段 |
|---|---|---|
| 路径探测 | `debug` | `link`, `rtt`, `jitter`, `relay_region` |
| **路径升级** | **`warn`** | `from=DERP to=peer_relay gain=12.5x` |
| **路径降级/滑落** | **`warn`** | `from=direct to=DERP reason=...` |
| 中继选择决策 | `info` | 各候选 `score`,选中者 |
| 中继切换 | `warn` | `old`, `new`, `reason` |
| 中继失败 | `warn` | `relay`, `error`, `fallback_to` |
| token 拒绝 | `warn` | `reason`(过期/签名错/重放) |

### 10.2 专属指标

| 指标 | 说明 |
|---|---|
| `fts_path_type{link}` | 当前链路类型(带标签:direct/peer_relay/derp) |
| `fts_relay_rtt_seconds` | 中继往返延迟 |
| `fts_relay_throughput_bytes` | 中继吞吐 |
| `fts_relay_reselect_total` | 中继重选次数 |
| `fts_relay_gain_ratio` | 切换前后吞吐增益(验证 T1 价值) |

### 10.3 诊断包增强

`fts diagnose` 的 zip 增加:

- `tailscale-status.json`(脱敏快照)
- `tailscale-ping.txt`(路径校验记录)
- `path-history.jsonl`(完整路径变更时间线)
- 中继选择评分明细

---

## 11. 接口设计

### 11.1 CLI

```powershell
# 发送时指定中继策略
fts send .\data.bin --to 100.64.0.5:4433 --relay auto      # 默认:按探测自动
fts send .\data.bin --to 100.64.0.5:4433 --relay off       # 禁用中继
fts send .\data.bin --to 100.64.0.5:4433 --relay 100.64.0.9  # 指定中继节点

# 查看当前路径（诊断利器）
fts path --to 100.64.0.5
# → link=peer_relay via=100.64.0.9 rtt=42ms (was DERP(ord) 452ms)

# 自建中继(T1.1)
fts relay serve --listen :51820 --max-sessions 32
fts relay status
```

### 11.2 编程接口

**任务配置新增字段:**

```jsonc
{
  "relay_policy": "auto",          // auto | off | prefer | require
  "relay_candidates": [],          // 显式候选中继
  "e2e_encryption": "auto"         // auto | on | off (T1.1)
}
```

**事件流新增类型:**

| 事件 | 负载 |
|---|---|
| `path.detected` | `{link, rtt, relay_region}` |
| `path.upgraded` | `{from, to, gain}` |
| `path.degraded` | `{from, to, reason}` |
| `relay.selected` | `{relay, score, candidates}` |
| `relay.failed` | `{relay, error, fallback}` |

> 与父文档的**统一任务模型**一致:`submit → task_id → observe → result`,中继事件作为 observe 流的一部分。

---

## 12. 与降级阶梯的整合

T1 强化父文档第 9 节的 **方案 E'**(链路降级),修订后的阶梯:

| 档位 | 链路 | T1 行为 |
|---|---|---|
| **L0 理想** | Tailscale 直连(同 LAN) | 不启用中继 |
| **L1 良好** | Tailscale 国内直连 | 不启用中继,持续探测 |
| **L2 一般** | 跨国直连 | 监控;**一旦滑落 DERP 即触发 T1** |
| **L3 较差** | **DERP / 直连劣化** | ✅ **启用 T1.0(Peer Relay)**;无 Tailscale 则 T1.1 |
| **L4 恶劣** | UDP 被封 | 经 Peer Relay / DERP(仍 UDP over tailnet);T1.1 走 TCP 回退 |
| **L5 极端** | 严格封锁 | TCP 单连接 + 中继 |
| **L6 保底** | TCP+TLS | 最后手段 |

**新增规则**:路径**回升**同样适用滞回(第 5.3 节),避免在直连/中继间反复抖动。

---

## 13. 测试与验收

### 13.1 T1.0 测试

| 用例 | 方法 | 期望 |
|---|---|---|
| DERP→Peer Relay 升级 | 制造硬 NAT(如对称 NAT/CGNAT),`tailscale ping` 观察 | 1~2 个 ping 内升级,吞吐显著提升 |
| 无中继时的提示 | tailnet 不配置中继 | 输出可操作 hint,不静默 |
| 路径滑落检测 | 传输中切断直连 | 记录 `path.degraded` 事件 |
| ACL 拒绝 | 撤销 ACL grant | 正确识别不可用并回退 |

### 13.2 T1.1 测试

| 用例 | 方法 | 期望 |
|---|---|---|
| 双方 NAT 后通过中继 | 两台硬 NAT 主机,禁直连 | 经中继成功传输,哈希校验通过 |
| 中继中途掉线 | 传输中 kill relay | checkpoint 续传成功 |
| 恶意中继返回坏数据 | 注入损坏字节 | 哈希拒绝,换中继重取 |
| token 重放 | 重复使用 token | 第二次被拒 |
| 多中继分流 | 配置 2 个中继 | 分块流正确分配 |

### 13.3 验收标准

| 指标 | 目标 |
|---|---|
| **DERP 场景吞吐提升** | ≥ **5×**(官方同链路实测 12.5×,保守取 5× 作为门禁) |
| UDP 全阻断下 | 传输仍**成功**(经中继/DERP) |
| 硬 NAT 下 | 传输仍**成功** |
| 中继故障 | **100%** 可续传,无数据损坏 |
| 路径变更 | **100%** 被记录,可从 journal 还原 |

### 13.4 真实链路验证

- 韩国 ↔ 中国(海底光缆,晚高峰)
- 韩国 ↔ 日本
- 韩国 ↔ 美国西海岸(跨太平洋高 RTT)
- 节点:AWS `ap-northeast-2`(首尔)、`ap-northeast-1`(东京)、`us-west-2`
- **建议中继布点**:东京 + 美西(覆盖亚洲与美洲方向)

---

## 14. 路线图插入点

在父文档 v1.1 路线图基础上:

| 阶段 | T1 相关内容 |
|---|---|
| **P0 可行性** | 探测能力最小验证:`tailscale status --json` / `ping` 解析跑通 |
| **P1 MVP** | 路径探测 + 链路类型日志(仅观测,不做切换) |
| **P2 性能与降级** | ✅ **T1.0 完整实现**(触发/选路/校验/回退);如无 Tailscale 需求则不启动 T1.1 |
| **P3 多接口** | `fts path` 命令 + 中继事件流暴露 |
| **P4 硬化** | ACL 拒绝、token 重放、多中继等边界用例 |

**T1.1 的门禁**:仅当出现"**在没有 Tailscale 的公网环境使用 fts**"的真实需求时才投入。

---

## 15. 风险与开放问题

| 风险 | 说明 | 缓解 |
|---|---|---|
| **Peer Relay 仍在 beta** | 官方称可视化与调试能力"仍在完善" | fts 自带路径记录,不完全依赖 Tailscale 可视化 |
| **免费额度 2 个中继** | 超出需扩容 | 合理布点(东京+美西);按需联系 Tailscale |
| **中继成单点** | 中继故障影响传输 | 多中继候选 + 滞回重选 + checkpoint 续传 |
| **T1.1 与 Peer Relay 职责重叠** | 在 tailnet 内自建会重复 | 严格按第 2 节选型规则,不同时启用 |
| **被动推断不可靠** | 混淆丢包与路径滑落 | 仅作兜底,标 `confidence=low` |
| **Windows 中继节点用户态开销** | 若中继跑在 Windows | 建议中继节点用 Linux 云 VM |
| **token 安全** | T1.1 的会合凭据泄露 | 短有效期 + 一次性 + 绑定 manifest |

### 开放问题

| 编号 | 问题 | 需确认 |
|---|---|---|
| O1 | `tailscale status --json` 具体字段名(需 P0 实测) | Tailscale 版本相关 |
| O2 | 是否接受在中继上做**带宽整形**以保护其他业务? | 运维策略 |
| O3 | T1.1 的端到端 AEAD 默认是否开启? | 安全策略 |
| O4 | 中继布点(东京/美西/新加坡)与你的实际跨境方向是否匹配? | 待你确认 |

---

## 16. 附录:默认参数表

| 参数 | 默认值 | 说明 |
|---|---|---|
| `relay_policy` | `auto` | 自动按探测决定 |
| 探测间隔 | 30s 或每 1GB | 取先到者 |
| CLI 探测超时 | 200ms | 不阻塞传输 |
| 切换滞回系数 | 0.8 | 新候选需优 20% 才切 |
| 切换冷却期 | 60s | 防抖 |
| `load_ratio` 上限 | 0.85 | 超限不选中继 |
| token 有效期 | 10 分钟 | T1.1 |
| 最大会话数 | 32 | T1.1 relay |
| 端到端 AEAD | `auto` | T1.1 非自有中继时开 |
| 中继候选数(免费) | 2 | Tailscale Peer Relay |
| 建议中继布点 | 东京 + 美西 | 覆盖韩国↔亚洲/美洲 |

---

*文档结束*
