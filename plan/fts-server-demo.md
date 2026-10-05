# fts 编排 server —— 详细案例(多任务并行 / gRPC 控制两端 / 最小 Web UI)

> **版本**:v1.0
> **日期**:2026-10-06
> **父文档**:`fts-p5-integration-design.md`、`API.md`、`fts-api-spec.md`
> **定位**:一个贴近真实应用场景的**编排 server** 案例:输入两端主机与路径,server 通过 gRPC
> 分别指挥两端完成传输,带进度条、错误回报、最小 Web UI,并支持 A1→B1 / A2→B2 / A3→B3 并行。
> 代码在 [`orch/`](../orch/),可复制、可复现、可扩展。

---

## 1. 需求(来自实际场景)

1. 输入两个客户端的 IP + 端口,server 先确定这两端**存在且活着**;
2. 输入「从哪个 IP 的哪个路径 → 到哪个 IP 的哪个路径」;
3. 由 server **发 gRPC** 按任务控制两端;
4. 要有**进度条**与**错误回报**;
5. 必要时候有**最小化 GUI**;
6. 做**多任务并行**测试:A1→B1、A2→B2、A3→B3;
7. 作为**详细案例**留档。

## 2. 关键约束:fts 是纯 push 模型 —— 决定了 server 的形态

fts 里**没有任何"命令接收端去拉取"的 RPC**。数据连接永远由**源端**发起去连**目标端**:

```
控制流:  Hello ↔ Hello → Manifest(分块哈希表+Merkle根) → ManifestAck(可带续传位图)
        → [数据流 N 条并行] → Finish → FinishAck
```

因此 server **不是数据中转方**,而是一个 **gRPC 客户端**,分别指挥两端。这条映射是整个案例的核心:

```
server ──Capabilities──> A(源)   探活 + 读 A 的身份(指纹/监听端口)
server ──Capabilities──> B(目标) 探活 + 读 B 的身份
server ──ReserveInbox──> B       在 B 上预注册落盘目录 → ticket
server ──Submit───────> A        paths=源路径, dst_addr=B:数据端口,
                                 fingerprint=B指纹, ticket=ticket → task_id
server ──Observe──────> A        NDJSON 事件流 → 进度 / 结果
        (A 自己直连 B 传数据,字节不经过 server)
```

为什么必须这样拆:
- **探活**用 `Capabilities`(而不是 ping)才能同时拿到协议版本、能力位、证书指纹、数据端口;
- **落盘目录**必须经目标端 `ReserveInbox` 授权(ticket),这是 fts 的授权边界 —— 编排方不能凭空指定对端写哪;
- **提交**必须发给**源端**:源端才持有文件、才发起数据连接;
- **进度/结果**从源端 `Observe` 拿(事件流描述的是发送任务)。

### 2.1 两处必须先补的缺口(本案例的原型扩展)

| 缺口 | 问题 | 解决 |
|---|---|---|
| 无 RPC 可读指纹/端口 | 指纹只在 daemon 启动时打印到 stderr,编排方无从得知 | `CapabilitiesReply` 新增 `fingerprint`(tag 4)与 `listen_addr`(tag 5) |
| gRPC 默认只绑 `127.0.0.1` | 跨主机编排连不上 | daemon 用 `--grpc 0.0.0.0:50051` 暴露(配置项,无代码改动) |

proto 只**追加字段**(proto3 未知字段被忽略),旧客户端完全向后兼容,`tests/grpc_smoke.py` 无需修改。

---

## 3. 架构与改动清单

### 3.1 改动

| 位置 | 改动 |
|---|---|
| `proto/fts.proto` | `CapabilitiesReply` + `fingerprint`(4) + `listen_addr`(5) |
| `src/grpc.rs` | `GrpcService` 经共享 `Registry` 携带身份;`capabilities()` 返回两字段 |
| `src/daemon.rs` | `run()` 加载证书算指纹(`tls::load_or_generate_cert` + `Blake3`),写入 `Registry`;REST `/v1/capabilities` 同步两字段 |
| `orch/`(新) | Python 编排 server:核心引擎 + CLI + FastAPI/SSE + 单页 Web UI |
| 文档 | 本文件;`README.md`、`plan/dev-log.md` 同步 |

指纹用的是 daemon **数据监听同一张缓存证书**(`cert_cache/fts-cert.der`),所以拿到的值正是发送端 pin 所需的值,跨重启稳定。

### 3.2 server 文件结构

```
orch/
  gen_proto.py          从 ../proto/fts.proto 生成 fts_pb2*.py
  core.py               编排引擎(事件总线 + 探活 + 编排 + 取消)
  cli.py                --check / --pairs / --run-config-tasks / --serve
  app.py                FastAPI:REST + SSE(Web UI 后端)
  static/index.html     单页 Web UI(原生 JS + EventSource)
  config.example.json   endpoints + tasks 样例
  requirements.txt      grpcio grpcio-tools fastapi uvicorn
  run_local_demo.ps1    单机 6-daemon 并行演示 + 自动校验 + 清理
  README.md             使用说明
```

### 3.3 数据模型(server 侧)

- **Endpoint**:`name`、`grpc_addr` + 发现结果(`alive`/`control_ok`/`data_ok`/`fingerprint`/`listen_addr`/`data_addr`/版本);
- **Task**:`src`/`src_path`/`dst`/`dst_path`/选项 + 运行时(`state`/进度字节/当前文件/吞吐/ETA/`error_code`/`error_message`/`warnings`);
- **data_addr 推导**:daemon 上报的 `listen_addr` 多为 `0.0.0.0:4433`,server 用 `grpc_addr` 的 host 替换通配 host,得到「真实 IP:数据端口」再发起编排。

### 3.4 进度与错误的聚合规则

事件类型(`Observe` 的 NDJSON)与 server 的处理:

| 事件 | server 处理 |
|---|---|
| `task` | 置 RUNNING |
| `file` | 记录 `files_total`、当前文件名 |
| `progress` | `bytes_done/total`(**当前文件**)、吞吐、ETA |
| `file_done` | 累加 `done_bytes`、`files_done+1` |
| `degradation` | 记入 warnings(QUIC 回退 TCP 时可见) |
| `result` | 终态 `state` + `checksum_ok` + 错误三元组 |
| `error` | 记入 warnings(如 `lagged`) |
| `path` / 未知 | 忽略(契约:未知事件被下游忽略) |

- **整体进度** = 已完成文件字节和 + 当前文件进度(单文件传输即当前文件);
- **"最小化验证"**:终态 `COMPLETED` **且** `checksum_ok=true` 才判 PASS。COMPLETED 的语义就是
  「传完且接收端逐块 BLAKE3 校验通过」,所以 PASS 本身即端到端完整性保证;
- **兜底**:`Observe` 流不会在 `result` 后自动关闭,server 收到终态主动断开;**传输极快导致订阅晚于
  result** 时事件会全丢,因此每次收尾都拉一次 `Status` 补齐 `error_message` 并兜底终态。

---

## 4. 部署与运行

### 4.1 单机复现(无多台主机,推荐先跑这个)

```powershell
python -m pip install -r orch\requirements.txt
python orch\gen_proto.py
powershell -ExecutionPolicy Bypass -File orch\run_local_demo.ps1
```

脚本会:起 6 个 daemon(A* 源 / B* 目标,各独立 `--grpc/--listen/--out/--cert-cache`,
因此 6 个不同指纹)→ 生成测试文件 → 并行跑 A1→B1 / A2→B2 / A3→B3 → 校验落盘文件大小 → 清理。

端口分配:

| 主机 | gRPC | 数据(QUIC+TCP) |
|---|---|---|
| A1 / A2 / A3 | 50051 / 50052 / 50053 | 4441 / 4442 / 4443 |
| B1 / B2 / B3 | 50061 / 50062 / 50063 | 4451 / 4452 / 4453 |

**实测输出**(64 MiB × 3 并行):

```
=== health check ===
[OK ] A1 127.0.0.1:50051 指纹=91e7b8f94072 数据口=127.0.0.1:4441 protocol=1 fts=0.1.0
[OK ] A2 127.0.0.1:50052 指纹=68a7e429c51f 数据口=127.0.0.1:4442 protocol=1 fts=0.1.0
[OK ] A3 127.0.0.1:50053 指纹=4d733269db1a 数据口=127.0.0.1:4443 protocol=1 fts=0.1.0
[OK ] B1 127.0.0.1:50061 指纹=c010a8862f6d 数据口=127.0.0.1:4451 protocol=1 fts=0.1.0
[OK ] B2 127.0.0.1:50062 指纹=7eb1c0e186ec 数据口=127.0.0.1:4452 protocol=1 fts=0.1.0
[OK ] B3 127.0.0.1:50063 指纹=d5334b90e235 数据口=127.0.0.1:4453 protocol=1 fts=0.1.0

=== parallel A1->B1 / A2->B2 / A3->B3 ===
[PASS] A1->B1  COMPLETED checksum_ok=True  64.0MB  0.34s
[PASS] A2->B2  COMPLETED checksum_ok=True  64.0MB  0.34s
[PASS] A3->B3  COMPLETED checksum_ok=True  64.0MB  0.33s

=== landed files ===
[OK] .demo\recv\B1\payload.bin  67108864 bytes
[OK] .demo\recv\B2\payload.bin  67108864 bytes
[OK] .demo\recv\B3\payload.bin  67108864 bytes

DEMO PASSED
```

> 回环传输极快(64 MB 约 0.3 s),进度条可能只看到 0%→100% 的跳变;真实网络上会平滑。

### 4.2 真多机(6 台或至少 2 台)

**每台机器的 daemon**(以 B1 为例;Windows,建议 Tailscale 内网 IP):

```powershell
fts daemon --grpc 0.0.0.0:50051 --listen 0.0.0.0:4433 `
           --out D:\recv --cert-cache C:\fts-cert --max-sessions 3 `
           --rest 127.0.0.1:5050
```

要点:
- **`--grpc 0.0.0.0:50051`** 才能被 server 跨主机访问(默认只绑 localhost);
- 防火墙放行 **50051/tcp**(控制面)与 **4433/udp+tcp**(数据面);
- `--cert-cache` 固定目录使指纹跨重启稳定;
- server 只需知道每台的 **gRPC 地址**,指纹/数据端口自动发现。

**server 侧配置** `orch/config.json`:

```json
{
  "timeout_sec": 3,
  "defaults": { "transport": "auto", "streams": 0, "chunk_size": 0 },
  "endpoints": [
    { "name": "A1", "grpc_addr": "100.64.0.11:50051" },
    { "name": "A2", "grpc_addr": "100.64.0.12:50051" },
    { "name": "A3", "grpc_addr": "100.64.0.13:50051" },
    { "name": "B1", "grpc_addr": "100.64.0.21:50051" },
    { "name": "B2", "grpc_addr": "100.64.0.22:50051" },
    { "name": "B3", "grpc_addr": "100.64.0.23:50051" }
  ],
  "tasks": [
    { "name": "A1->B1", "src": "A1", "src_path": "C:\\data\\payload.bin", "dst": "B1", "dst_path": "D:\\recv" },
    { "name": "A2->B2", "src": "A2", "src_path": "C:\\data\\payload.bin", "dst": "B2", "dst_path": "D:\\recv" },
    { "name": "A3->B3", "src": "A3", "src_path": "C:\\data\\payload.bin", "dst": "B3", "dst_path": "D:\\recv" }
  ]
}
```

**运行**:

```powershell
python orch\cli.py --config orch\config.json --check          # 先确认 6 台都活着
python orch\cli.py --config orch\config.json --run-config-tasks
# 或临时指定配对:
python orch\cli.py --config orch\config.json --pairs "A1:B1,A2:B2,A3:B3" `
    --src-path C:\data\payload.bin --dst-path D:\recv
```

### 4.3 文件夹传输 + 传输前强制清空目标(force-clean 部署)

fts 核心(W8)支持**递归目录传输**:源路径填目录即可,目录按**相对路径结构**落盘 —— 目录参数以
自身 basename 为根,整体落在目标目录下(传 `D:\data\srcdir` → 目标得到 `<dst_path>\srcdir\...`,
子目录结构保留;空目录不传,符号链接跳过防环)。编排 server 无需特殊处理,`src_path` 填目录路径即可。

**传输前清空目标目录**(force-clean 部署)是任务级选项 `purge`,编排流程变为:

```
ReserveInbox(B) → PurgeInbox(token, force=true) → Submit(A) → Observe(A)
```

- 复用 W6 的 `PurgeInbox` 语义:清空 token 绑定目录的**全部直接内容**(子目录整删),目录本身保留;
  仅目录模式 token 可 purge、必须显式 force=true(fts 三重防护)。
- 编排层在 Web UI 提供「传输前清空目标目录(危险)」勾选,CLI 提供 `--purge`,config 任务加 `"purge": true`。
- purge 统计(删除的文件数/文件夹数)记入任务的 warnings,并在 Web UI 任务卡片展示。

```powershell
# 文件夹 + purge:整个文件夹(含子目录)传到 B1,传输前清空 D:\recv
python orch\cli.py --config orch\config.json --pairs "A1:B1" `
    --src-path D:\data\srcdir --dst-path D:\recv --purge
```

实测(单机模拟,3 路并行文件夹 + purge):三路全 COMPLETED + checksum_ok,`srcdir\a.txt / b.bin /
c.log / sub\nested.txt` 完整落盘,预埋的残留物(stale 文件 + stale 子目录)按统计精确清除。

> **注意**:purge 会不可恢复地删除目标目录全部内容,只用于"目标目录归本次任务所有"的部署场景。

### 4.4 Web UI


```powershell
python orch\cli.py --config orch\config.json --serve --host 127.0.0.1 --port 8080
```

浏览器打开 [http://127.0.0.1:8080](http://127.0.0.1:8080):

- **主机表**:名称 / gRPC 地址 / 控制面 / 数据面 / 数据地址 / 证书指纹 / 版本,「刷新探活」一键重探;
- **新建任务表单**:源主机、源文件路径、目标主机、目标目录、传输层/流数/分块;
- **任务列表**:状态徽标 + 进度条 + 百分比/吞吐/ETA + 当前文件 + 错误信息 + 取消按钮,由 SSE 实时驱动;
- 重复提交即可做多任务并行测试。

---

## 5. 事件 / 退出码 / 错误码对照

**fts 传输错误码**(server 上报的 `error_code`;也是源端退出码):

| 码 | 名称 | 含义 |
|---|---|---|
| 0 | OK | 成功 |
| 1 | IO_ERROR | 本地 I/O(如源文件不存在) |
| 2 | INVALID_ARG | 参数错误(含重名 basename) |
| 3 | CONNECT_FAILED / INCOMPATIBLE_VERSION | 连接失败 / 协议版本不符 |
| 4 | CHECKSUM_FAILED | 校验失败,**数据不可信** |
| 5 | INTERRUPTED | 中断(接收端留有 checkpoint,可续传) |
| 6 | CANCELLED | 已取消 |
| 7 | AUTH_FAILED | 指纹不匹配 / ticket 被拒 |

**编排层自己的错误码**(负数,与上面区分):`-1 ORCH_ERROR`、`-2 HOST_UNREACHABLE`(探活失败)、`-3 NO_DATA_ADDR / NO_FINGERPRINT`(对端信息缺失,通常是 daemon 版本过旧)。

**server 退出码**(`cli.py`):`0` 全部 PASS;`1` 有任务失败;`2` 参数/配置错误。

---

## 6. 验收用例(均已实测)

| # | 用例 | 判据 | 结果 |
|---|---|---|---|
| S1 | daemon `Capabilities` | 返回 64-hex 指纹与非空 listen_addr;旧 `grpc_smoke.py` 不受影响 | ✅ |
| S2 | 探活 | 关掉/伪造一台 → 该主机标 XX 并报控制面不可达;`--check` 退出码非 0 | ✅ |
| S3 | 单任务 A1→B1 | COMPLETED + checksum_ok;目标目录出现文件;Web UI 进度条走满 | ✅ |
| S4 | 并行 A1→B1 / A2→B2 / A3→B3 | 三条同时 RUNNING 且全部 COMPLETED,PASS | ✅ |
| S5 | 错误回报 | 源路径不存在 → FAILED,`IO_ERROR(1)` + 精确 message,退出码 1 | ✅ |
| S6 | Web UI 全流程 | 表单提交 → SSE 实时进度 → COMPLETED(浏览器实测) | ✅ |
| S7 | 文档复现 | 照本文 `run_local_demo.ps1` 从零跑通 | ✅ |
| S8 | 文件夹 + purge ×3 并行 | 递归子树(含子目录)完整落盘;预埋残留物按统计精确清除 | ✅ |
| S9 | 小文件竞态 | 7 字节文件 10 连传全 PASS(接收端 Finish 竞态修复后) | ✅ |

取消(S6 之外)由 `Cancel` RPC 实现,注意现有 daemon 的 `Cancel` **不回发 result 事件**,故 server 收到 `ok=true` 后自行置 CANCELLED(错误码 6)。

---

## 7. 排查手册

| 现象 | 原因 | 处理 |
|---|---|---|
| 探活「控制面不可达」 | daemon 未起 / 端口不通 / gRPC 仍绑 localhost | 确认 `--grpc 0.0.0.0:50051`,防火墙放行 |
| 控制面通、数据面「未知」 | 数据端口(4433)被防火墙挡,或 QUIC/UDP 被禁 | 放行 4433/udp+tcp;必要时 `--transport tcp` |
| 「对端未上报指纹」 | daemon 构建过旧 | 用支持自描述的构建重新 `cargo build --release` 并重启 daemon |
| 任务 FAILED `AUTH_FAILED(7)` | 指纹不该匹配(证书目录被换)或 ticket 被拒 | 清 `--cert-cache` 后重试;确认 server 自动 pin 的是目标端指纹 |
| 任务 `CONNECT_FAILED(3)` + degradation 事件 | QUIC 不可达,已回退 TCP | 正常降级;若要强制,设 `transport` |
| 多任务到**同一目标**变慢 | 目标端 `--max-sessions` 默认 3,超出的会话在 accept 队列排队 | 调大 `--max-sessions`,或分散到不同目标 |
| 目标路径不可写 | gRPC 的 `ReserveInbox` **无目录预检**(REST 有) | 该错误会在传输阶段以 IO 错误暴露;必要时先建目录或走 REST 注册 |
| daemon 日志大量「接收会话失败: early eof」 | 编排器探活会对数据端口做 TCP 探测,探测连接被当成会话接受后立即 EOF | **正常现象**,无害可忽略 |
| `Finish 后仍有 1 块未收到`(小文件偶发) | 旧版接收端在 Finish 与 accept 数据流之间存在竞态 | 已在 2026-10-06 修复(recv.rs Finish 后补收队列中的流);出现请升级 fts |
| 进度条不动/跳变 | 回环太快,事件只在 1 Hz 采样 | 真实网络下正常;不影响终态判定 |

### 安全声明(重要)

`Capabilities` 现在会返回证书指纹,且跨主机编排要求 gRPC 绑 `0.0.0.0` —— 而 **REST/gRPC 控制面
没有任何鉴权**。因此这套 server **只能用于可信网络**(LAN、Tailscale 内网),并建议用防火墙/ACL
把 50051 限制为只允许 server 所在主机访问。这是现有契约的已知边界,本案例不引入 server 侧鉴权。

---

## 8. 已知边界(诚实清单)

- server 不搬数据,是纯编排;源端 push 模型未改;
- 递归目录传输由 fts 核心(W8)提供,server 直接把目录路径作为 `src_path` 透传即可;`--keep-going`(任一文件失败即中止整个任务)仍不做;
- 一份任务只提交单个源路径(`paths=[src_path]`);多文件可扩展为表单多值,核心引擎已按 `paths` 列表传递;
- 目标路径授权走 inbox 目录模式(空 `file_name`,TTL 600 s,一次性校验后仍可用至过期);
- 阶段内不做:server 侧鉴权、任务持久化(daemon 重启丢任务表)、失败自动重试调度。

---

## 9. 复现清单(最小步骤)

```powershell
# 一次性
cargo build --release
python -m pip install -r orch\requirements.txt
python orch\gen_proto.py

# 每次演示
powershell -ExecutionPolicy Bypass -File orch\run_local_demo.ps1

# 交互使用
python orch\cli.py --config orch\config.json --serve
```
