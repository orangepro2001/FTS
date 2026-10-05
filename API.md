# fts 接口文档(Interface Specification)

> **文档版本**:v1.0
> **发布日期**:2026-10-05
> **适用产品**:fts v0.1.0(线协议 fts-protocol v1 / C ABI v1 / REST v1 / gRPC fts.v1)
> **文档性质**:描述**当前实际交付**的对外接口,与实现同步维护;设计愿景与契约演进规则见 `plan/fts-api-spec.md`(设计文档),两者冲突时以本文档为准。
> **目标读者**:集成方开发者(任何语言)、运维、测试工程师。

---

## 目录

1. [概述与接口选型](#1-概述与接口选型)
2. [通用约定](#2-通用约定)
3. [任务模型(所有接口共享)](#3-任务模型所有接口共享)
4. [事件模型(NDJSON)](#4-事件模型ndjson)
5. [inbox 预注册(接收目录分配)](#5-inbox-预注册接收目录分配)
6. [gRPC 接口(fts.v1)](#6-grpc-接口ftsv1)
7. [REST 接口(HTTP/JSON + SSE)](#7-rest-接口httpjson--sse)
8. [CLI 参考](#8-cli-参考)
9. [C ABI(fts.dll)](#9-c-abi-ftsdll)
10. [错误码总表](#10-错误码总表)
11. [安全模型](#11-安全模型)
12. [版本与兼容性承诺](#12-版本与兼容性承诺)
13. [附录 A:典型集成流程](#附录-a典型集成流程)
14. [附录 B:已知限制](#附录-b已知限制)

---

## 1. 概述与接口选型

`fts` 是 Windows 平台的高速文件传输工具(QUIC 多流 + BLAKE3 校验 + 断点续传 + TCP 自动回退)。所有对外接口共享同一套任务语义与传输引擎:

| 接口 | 技术 | 默认地址 | 适用场景 |
|---|---|---|---|
| **gRPC** | tonic(proto: `proto/fts.proto`,包 `fts.v1`) | `127.0.0.1:50051`(明文) | 程序集成主路径;需要事件流 |
| **REST** | axum(HTTP/1.1 JSON + SSE) | `127.0.0.1:5050` | 脚本 / curl / 无 gRPC 栈的语言 |
| **CLI** | `fts.exe` 子命令 | — | 人 / PowerShell / CI |
| **C ABI** | `fts.dll` + `include/fts.h` | — | C/C++/C#/Python ctypes 原生嵌入 |

选择决策:程序化集成首选 **gRPC**;同机原生进程选 **C ABI**;脚本用 **CLI `--json`** 或 REST。

> daemon 启动示例:`fts daemon --rest 127.0.0.1:5050 --listen 0.0.0.0:4433 --grpc 127.0.0.1:50051 --out D:\downloads`
> 运维脚本:`.\fts-daemon.ps1 start|stop|restart|status`。

---

## 2. 通用约定

| 约定 | 值 |
|---|---|
| 字符编码 | UTF-8(所有 JSON、路径、字符串字段) |
| REST 数据格式 | 请求/响应 `application/json`;事件流为 `text/event-stream`(SSE) |
| gRPC 明文 | 默认无 TLS,仅 localhost;**不得**暴露到非本机网络 |
| 时间 | 秒(`uint64`/`double`);预估时间字段命名 `*_seconds`/`expires_in` |
| 空字符串语义 | gRPC proto3 无 Option:可空字段用空串表示未提供(如 `fingerprint=""`) |
| 未知字段/事件 | 接收方**必须忽略**(前向兼容承诺) |
| 幂等性 | 提交类接口(Submit/ReserveInbox)**不幂等**(每次调用产生新资源);Cancel 幂等(重复取消返回 ok) |
| 并发 | 同一 task_id 的事件流可多路订阅;任务表无上限(受内存限制) |

**认证模型**(三层,均非用户账号体系):

1. **传输身份**:自签证书 + 指纹固定(TOFU)。接收端启动时打印证书 BLAKE3 指纹,发送端以 `fingerprint` 字段固定;不匹配 → 认证失败(退出码 7 / gRPC `UNKNOWN`+error_code 7)。
2. **落盘授权**:`ticket`(inbox token)决定文件落到哪个目录;无 token → 落 daemon 默认 `--out`。
3. **无用户鉴权**:REST/gRPC 仅监听 localhost;任何本机进程均可调用(含 Cancel)。见[安全模型](#11-安全模型)。

---

## 3. 任务模型(所有接口共享)

### 3.1 状态机

```
RUNNING ──► COMPLETED     (全部文件传输完成且 BLAKE3 校验通过)
   │  └──► FAILED         (任一文件失败/校验失败;快失败,后续文件不再传输)
   └─────► CANCELLED      (调用方取消)
```

- **COMPLETED 的语义是"传输完成且校验通过"**;校验失败一律 FAILED(`error_code=4`),不存在"传完了但内容不确定"的中间态。
- 终态不可逆;任务记录保留在内存(daemon 重启即清空;持久化审计走日志)。

### 3.2 传输语义

| 语义 | 说明 |
|---|---|
| 多文件 | 单连接逐文件传输;**任一文件失败即中止整个任务**(快失败,无 keep-going) |
| 目录递归 | 路径为目录时**递归展开全部子目录**,保留相对路径结构在接收端重建(`fts send dir` → 接收端出现 `<ticket目录>/dir/...` 整棵子树)。协议内路径分隔符统一 `/`;空目录不会被重建;目录符号链接不递归(防环) |
| 路径安全 | manifest 中的相对路径在接收端强制校验:拒绝 `..`/`.`/空组件/反斜杠/盘符(防路径穿越);非法即 CHECKSUM 类失败 |
| 重名 | 多路径展开后**相对路径**重复 → 提交即拒绝(退出码 2 / gRPC INVALID_ARGUMENT) |
| 断点续传 | 接收端每秒落 checkpoint(per-file,含子目录内文件);同 manifest 重发自动只补缺失块;**无需调用方干预**(提交同一任务即续传) |
| 传输层 | `auto`(默认)= QUIC 失败 4s 内自动回退 TCP;`quic`/`tcp` 强制单栈 |
| 校验 | BLAKE3 逐块 + Merkle 根,**不可关闭**(硬约束) |

---

## 4. 事件模型(NDJSON)

Observe(gRPC stream)与 REST SSE 推送同一事件流:每行一个 JSON 对象(SSE 场景包在 `data:` 中)。

| type | 载荷字段 | 级别 | 说明 |
|---|---|---|---|
| `task` | `task_id`, `state` | info | 任务创建(始终第一条) |
| `progress` | `bytes_done`, `bytes_total`, `throughput_bps`, `eta_seconds`(可为 null) | debug | 每秒;当前**文件**的字节进度 |
| `file` | `name`, `index`(1 起), `total` | info | 多文件任务中某文件开始 |
| `file_done` | `name`, `checksum_ok`, `seconds`, `bytes` | info | 某文件完成 |
| `path` | `link`(`direct`/`peer_relay`/`derp`/`unknown`), `rtt_ms`(可 null) | info | Tailscale 链路探测结果(仅发送端) |
| `degradation` | `from`, `to`(如 `L0`→`L4`), `reason`(如 `quic_unreachable_tcp_fallback`) | **warn** | 降级事件,永不被过滤 |
| `result` | `state`, `checksum_ok`, `duration_seconds`,失败时附 `error_code`, `error` | info | 终态(始终最后一条) |
| `error` | `message` | error | 流内部异常(如订阅滞后 `lagged`) |

```jsonc
{"type":"task","task_id":"3f2b8c1a-...","state":"RUNNING"}
{"type":"file","name":"a.bin","index":1,"total":12}
{"type":"progress","bytes_done":1048576,"bytes_total":1073741824,"throughput_bps":94371840,"eta_seconds":11}
{"type":"file_done","name":"a.bin","checksum_ok":true,"seconds":3.2,"bytes":1073741824}
{"type":"degradation","from":"L0","to":"L4","reason":"quic_unreachable_tcp_fallback"}
{"type":"result","state":"COMPLETED","checksum_ok":true,"duration_seconds":42.5}
```

**订阅注意**:事件通道为环形广播(容量 256)。订阅晚于提交将丢失早期事件;`result` 可改由 Status 轮询兜底。

---

## 5. inbox 预注册(接收目录分配)

daemon 的接收目录默认是启动参数 `--out`;集成方通过 **inbox token** 把某个(批)传输指到任意目录。

### 5.1 两种 token 模式

| 模式 | 注册方式 | 有效语义 |
|---|---|---|
| **目录模式** | `file_name` 为空 | TTL(600s)内**可复用**;匹配任意文件名 → 全部落 `out_dir`。批量场景用这个。 |
| **文件绑定模式** | `file_name` 给定 | **一次性**:该文件名兑换成功即焚毁;其他文件名兑换**失败且焚毁**(防枚举) |

通用规则:

- TTL 600 秒,自创建起算(过期兑换失败,失败信息 `ticket 已过期`)。
- 兑换失败统一报 `AUTH_FAILED`(退出码 7),接收端不会把文件落到错误目录。
- 无 ticket 的传输 → 落 daemon 默认 `--out`(完全向后兼容)。
- daemon 重启后 token 全部失效(内存表)。

### 5.2 流程

```
集成方(Z 侧)                    发送方(A 侧调度)
   │ POST /v1/inbox                 │
   │◄── {token}                     │
   │──── token 传给 A 的调度 ──────►│
   │                                │ Submit{ticket: token, paths:[...]}
```

### 5.3 网络凭据(代建立 SMB 会话)

接收目录为需要认证的 SMB/UNC 路径时,注册时可携带凭据,fts 调 Windows `WNetAddConnection2` **代为建立会话**:

```jsonc
// POST /v1/inbox
{
  "out_dir": "\\\\nas01\\share\\deploy",
  "credential": { "user_name": "svc_fts", "password": "***", "domain": "CORP" }  // domain 可省
}
```

语义与边界:

| 项 | 行为 |
|---|---|
| 会话生命周期 | 进程级(临时连接),daemon 退出即失效;**不写入凭据管理器、不持久化** |
| 日志 | 密码**绝不**进入日志/错误消息/事件;日志只出现 `user=... password=***` |
| 作用 | 建立 `\\host\share` 会话后,该 daemon 的所有后续该主机文件操作走此凭据 |
| 失败 | 400 + 可读原因(1326 凭据无效 / 5 无权限 / 1219 会话冲突(附 `net use … /delete` 提示)/ 53/67 路径不存在) |
| 限制 | 仅 UNC 路径;本地路径带 credential → INVALID_ARGUMENT |
| 替代 | 更推荐 `cmdkey` 预存凭据(见 §11.1),credential 接口用于集成方动态持有凭据的场景 |

凭据建立**成功后**才做目录可写预检与发 token —— token 的发放即代表"该目录此刻可写"。

### 5.4 清空目录(PurgeInbox,危险操作)

配合 force 级"干净复制":传输前把 token 绑定目录**内部**清空(目录本身保留),使后续传输落入干净目录。

```
POST /v1/inbox/{token}/purge        {"force": true}
rpc PurgeInbox(PurgeRequest{token, force}) returns (PurgeReply)
```

```jsonc
// 200
{"ok": true, "files_deleted": 2, "directories_deleted": 1, "dir": "D:\\fromA"}
```

**三重安全保障**(fts 侧只负责准确执行,"是否允许删"由集成端决策):

1. **仅目录模式 token**:文件绑定 token 的授权范围只是一个文件,调用 purge → `403`(gRPC `PERMISSION_DENIED`)—— 授权范围与破坏范围严格对齐;
2. **必须显式 `force: true`**:缺省/false → `400`(gRPC `INVALID_ARGUMENT`);
3. **范围严格受限**:只删除 token 绑定目录的**直接内容**(子目录整删、文件删),不触碰目录本身与目录外任何路径;每次操作打 `[warn] PURGE` 日志(含统计与 token,可审计)。

| 错误 | HTTP | gRPC |
|---|---|---|
| force 未显式 true | 400 | INVALID_ARGUMENT |
| 文件绑定 token | 403 | PERMISSION_DENIED |
| token 无效/过期 | 404 | NOT_FOUND |

标准顺序:**ReserveInbox → PurgeInbox(force=true) → Submit → Observe**。

---

## 6. gRPC 接口(fts.v1)

- 端点:`127.0.0.1:50051`(明文 h2c)
- proto:`proto/fts.proto`;生成方式见 README(protoc 或参考 tests/grpc_smoke.py 的 grpc_tools 动态生成)

### 6.1 Capabilities

探测协议版本与能力。**集成方必须先调用**,不得假设能力存在。

```
rpc Capabilities(Empty) returns (CapabilitiesReply)
```

| CapabilitiesReply 字段 | 类型 | 值 |
|---|---|---|
| protocol_version | string | `"1"` |
| capabilities | uint32 位图 | bit3 = resume(0x8) |
| fts_version | string | 如 `"0.1.0"` |

### 6.2 ReserveInbox

```
rpc ReserveInbox(ReserveInboxRequest) returns (InboxToken)
```

| 请求字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| out_dir | string | ✅ | 接收目录(daemon 侧本地路径) |
| file_name | string | — | 空 = 目录模式;否则文件绑定模式 |

| 响应字段 | 类型 | 说明 |
|---|---|---|
| token | string | UUID;放入 TransferRequest.ticket |
| expires_in | uint64 | 600(秒) |

错误:`INVALID_ARGUMENT`(out_dir 空)。

### 6.3 Submit

提交传输任务(异步,立即返回 task_id)。

```
rpc Submit(TransferRequest) returns (TransferHandle)
```

| TransferRequest 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| paths | repeated string | ✅(与旧字段二选一) | 文件/目录列表;目录**递归展开**(保留相对结构);相对路径重复 → INVALID_ARGUMENT |
| dst_addr | string | ✅ | 对端 `host:port`(接收端 daemon/recv 监听地址) |
| fingerprint | string | 与 insecure 二选一 | 对端证书 BLAKE3 指纹(64 hex) |
| insecure | bool | — | true = 跳过证书校验(仅测试);与 fingerprint 互斥 |
| streams | uint32 | — | 并行流数,0 = 4,上限 64 |
| chunk_size | uint64 | — | 分块字节,0 = 8 MiB |
| transport | string | — | `""`/`auto`/`quic`/`tcp` |
| ticket | string | — | inbox token |

| 响应字段 | 类型 |
|---|---|
| task_id | string(UUID) |

错误:`INVALID_ARGUMENT`(参数问题);提交成功后传输失败**不**走 gRPC 错误,通过 Status/Observe 观察。

### 6.4 Observe(server-streaming)

```
rpc Observe(ObserveRequest{task_id}) returns (stream TransferEvent{raw_json})
```

- `raw_json` 为[第 4 节](#4-事件模型ndjson)事件的 NDJSON 原文;解析失败的事件跳过。
- 流结束条件:收到 `result` 事件后调用方主动结束;服务端在任务终态后仍可能保持流(靠 result 判断)。
- 错误:`NOT_FOUND`(任务不存在)。

### 6.5 Status

```
rpc Status(TaskRef{task_id}) returns (TransferStatus)
```

| TransferStatus 字段 | 类型 | 说明 |
|---|---|---|
| task_id | string | |
| state | string | `RUNNING` / `COMPLETED` / `FAILED` / `CANCELLED` |
| checksum_ok | bool | |
| error_code | int32 | 契约错误码(见[第 10 节](#10-错误码总表));无错误为 0 |
| error / error_message | string | 错误名 / 可读详情 |

错误:`NOT_FOUND`。

### 6.6 Cancel

```
rpc Cancel(TaskRef{task_id}) returns (Ack{ok})
```

幂等;对已结束任务返回 `ok=true` 无副作用。取消后任务进 `CANCELLED`,接收端半成品保留 checkpoint(重发即续传)。

### 6.7 调用示例(Python)

完整可运行脚本:`tests/grpc_smoke.py`。骨架:

```python
import grpc, fts_pb2, fts_pb2_grpc
ch = grpc.insecure_channel("127.0.0.1:50051")
stub = fts_pb2_grpc.FtsStub(ch)

tk  = stub.ReserveInbox(fts_pb2.ReserveInboxRequest(out_dir=r"D:\fromA"))
h   = stub.Submit(fts_pb2.TransferRequest(
        paths=["C:\\data\\a.bin", "C:\\data\\b.bin"],
        dst_addr="100.64.0.5:4433", fingerprint=FP, ticket=tk.token))
for ev in stub.Observe(fts_pb2.ObserveRequest(task_id=h.task_id)):
    import json; e = json.loads(ev.raw_json)
    if e.get("type") == "result": break
st  = stub.Status(fts_pb2.TaskRef(task_id=h.task_id))
assert st.state == "COMPLETED" and st.checksum_ok
```

---

## 7. REST 接口(HTTP/JSON + SSE)

- 端点:`127.0.0.1:5050`;所有请求/响应 `application/json`;错误统一 `{"error": "<原因>"}`。

| # | 方法 | 路径 | 对应 gRPC |
|---|---|---|---|
| 1 | GET | `/v1/capabilities` | Capabilities |
| 2 | POST | `/v1/inbox` | ReserveInbox(可带 credential) |
| 3 | DELETE | `/v1/inbox/{token}` | —(撤销) |
| 3b | POST | `/v1/inbox/{token}/purge` | PurgeInbox(危险,见 §5.4) |
| 4 | POST | `/v1/transfers` | Submit |
| 5 | GET | `/v1/transfers` | 任务列表(gRPC 无对应) |
| 6 | GET | `/v1/transfers/{id}` | Status |
| 7 | POST | `/v1/transfers/{id}/cancel` | Cancel |
| 8 | GET | `/v1/transfers/{id}/events` | Observe(SSE) |

### 7.1 GET /v1/capabilities

```jsonc
// 200
{"protocol_version":"1","fts_version":"0.1.0","capabilities":8,
 "rest_endpoints":["GET /v1/capabilities", "..."]}
```

### 7.2 POST /v1/inbox

```jsonc
// 请求
{"out_dir": "D:\\fromA", "file_name": ""}        // file_name 空 = 目录模式
// 200
{"token": "7dc85a06-...", "expires_in": 600}
```

### 7.3 DELETE /v1/inbox/{token}

```jsonc
// 200
{"ok": true}      // token 不存在也返回 ok:false(200)
```

### 7.4 POST /v1/transfers

```jsonc
// 请求(202)
{"paths": ["C:\\data\\a.bin", "C:\\data\\dirB"],   // 或 "src_path": 单文件(旧兼容)
 "dst_addr": "100.64.0.5:4433",
 "fingerprint": "<64hex>",                          // 或 "insecure": true
 "streams": 4, "chunk_size": 0,
 "transport": "auto", "ticket": "<token>"}
// 202 Accepted
{"task_id": "3f2b8c1a-..."}
// 400 Bad Request
{"error": "必须提供 fingerprint 或 insecure=true"}
```

### 7.5 GET /v1/transfers

```jsonc
// 200
{"tasks": [{"task_id": "...", "state": "RUNNING"}, ...]}
```

### 7.6 GET /v1/transfers/{id}

```jsonc
// 200
{"task_id":"...","state":"COMPLETED","checksum_ok":true}
// FAILED 时附加:
{"task_id":"...","state":"FAILED","checksum_ok":false,"error_code":4,"error":"CHECKSUM_FAILED","error_message":"..."}
// 404
{"error": "任务不存在"}
```

### 7.7 POST /v1/transfers/{id}/cancel

```jsonc
// 200(幂等)
{"ok": true}          // 404:任务不存在
```

### 7.8 GET /v1/transfers/{id}/events(SSE)

```
data: {"type":"task","task_id":"...","state":"RUNNING"}
data: {"type":"progress","bytes_done":...,"bytes_total":...,...}
data: {"type":"result","state":"COMPLETED","checksum_ok":true,...}
```

事件模型同[第 4 节](#4-事件模型ndjson);`404` 任务不存在。

### 7.9 调用示例(PowerShell)

```powershell
$r = Invoke-RestMethod -Method Post -Uri http://127.0.0.1:5050/v1/inbox `
     -ContentType application/json -Body '{"out_dir":"D:\\fromA"}'
$body = @{ paths=@("C:\data\a.bin","C:\data\b.bin"); dst_addr="100.64.0.5:4433";
           fingerprint=$fp; ticket=$r.token } | ConvertTo-Json
$t = Invoke-RestMethod -Method Post -Uri http://127.0.0.1:5050/v1/transfers `
     -ContentType application/json -Body $body
curl.exe http://127.0.0.1:5050/v1/transfers/$($t.task_id)/events     # SSE
```

---

## 8. CLI 参考

```text
fts send <PATH>... --to <ADDR> [--fingerprint <HEX> | --insecure]
        [--chunk-size <B>=8MiB] [--streams <N>=4] [--ticket <T>]
        [--transport auto|quic|tcp] [--json] [-v]
    PATH  文件或目录(可多个;目录展开直接文件,不递归;展开后重名 → 退出码 2)

fts recv --listen <ADDR> [--out <DIR>=.] [--transport auto|quic|tcp]
        [--cert-cache <DIR>=.fts] [--json] [-v]
    处理一个会话后退出(面向人);常驻用 daemon

fts daemon [--rest 127.0.0.1:5050] [--listen 0.0.0.0:4433] [--out <DIR>=.]
        [--cert-cache .fts] [--max-sessions 3] [--grpc 127.0.0.1:50051|""](禁用)
```

**`--json` 模式**:stdout 只输出[事件](#4-事件模型ndjson) NDJSON;人类日志走 stderr。**脚本必须检查退出码**:

| 退出码 | 含义 |
|---|---|
| 0 | 成功且校验通过 |
| 1 | 通用 / I/O 错误 |
| 2 | 参数错误(clap 或重名检测) |
| 3 | 连接失败 / 协议版本不匹配 |
| **4** | **校验失败 —— 数据不可信,必须处理** |
| 5 | 传输中断(接收端保留 checkpoint,重发即续传) |
| 6 | 被取消 |
| 7 | 认证失败(指纹不匹配 / ticket 拒绝) |

---

## 9. C ABI(fts.dll)

头文件 `include/fts.h`;P3 最小集成面(阻塞式一体)。**ABI 版本**:`fts_abi_version() == 1`,结构变化时递增。

```c
const char* fts_version(void);          // 静态字符串,勿释放
uint32_t    fts_abi_version(void);
const char* fts_last_error(void);       // 下一次任何 fts_* 调用前有效;需保留必须拷贝
int32_t     fts_send_file(const char* src_path, const char* dst_addr,
                          const char* fingerprint_hex,   /* NULL + insecure!=0 */
                          int32_t insecure, uint32_t streams,   /* 0 = 4 */
                          uint32_t chunk_size,           /* 0 = 8 MiB */
                          const char* transport);        /* NULL/"auto"/"quic"/"tcp" */
```

- 返回值:0 成功;否则[错误码](#10-错误码总表)(与 CLI 退出码一致)。
- **内存规则**:所有返回字符串由 fts 托管,宿主**不得释放** —— 无跨边界 free。
- 线程安全:各函数可任意线程调用;`fts_last_error` 为全局单槽(多线程宿主建议调用后立即拷贝)。
- 不支持:多文件(逐次调用)、ticket、事件回调(P3 面最小;需要时升级 ABI v2)。
- 验证脚本:`python tests\ffi_smoke.py <指纹>`。

---

## 10. 错误码总表

错误码跨接口统一(CLI 退出码 = REST `error_code` = gRPC `TransferStatus.error_code` = C ABI 返回值):

| 码 | 名称 | 含义 | REST HTTP |
|---|---|---|---|
| 0 | OK | 成功 | 200 |
| 1 | IO_ERROR | 本地 I/O / 其他 | 200(任务态) |
| 2 | INVALID_ARG | 参数错误(含重名文件) | 400 |
| 3 | CONNECT_FAILED | 连接失败 | 200(任务态) |
| 3 | INCOMPATIBLE_VERSION | 协议版本不匹配 | 200(任务态) |
| **4** | **CHECKSUM_FAILED** | **校验失败,数据不可信** | 200(任务态) |
| 5 | INTERRUPTED | 中断(接收端有 checkpoint,可续传) | 200(任务态) |
| 6 | CANCELLED | 被取消 | 200(任务态) |
| 7 | AUTH_FAILED | 指纹不匹配 / ticket 拒绝 | 200(任务态) |

- 传输类错误(3/4/5/6/7)发生在**任务运行期**,REST/gRPC 提交仍返回成功;通过 Status/Observe 观察。gRPC 请求级错误只有 `INVALID_ARGUMENT` 与 `NOT_FOUND`。
- 错误码 v1 内不复用、不改语义;新增用新码。

---

## 11. 安全模型

| 层 | 机制 | 边界 |
|---|---|---|
| 传输加密 | QUIC = TLS 1.3(ring);TCP 回退**无 TLS**(仅建议测试环境) | — |
| 端到端完整性 | BLAKE3 逐块 + Merkle 根,不可关闭 | 中间人无法篡改内容 |
| 身份 | 自签证书 + 指纹固定(TOFU);`--insecure` 仅测试 | 首次连接务必抄录指纹 |
| 落盘授权 | inbox ticket;无 token → 默认目录 | token = 授权凭证,泄露即可向该目录写文件 |
| 网络暴露 | REST/gRPC 强制 localhost | **不得**加反向代理暴露公网(无鉴权) |

### 11.1 网络路径(SMB / UNC)接收目录

接收目录可以是 UNC 路径(如 `\\NAS\share\fts`),用于把文件直接落到 SMB 共享:

- **凭据由 Windows 会话负责,fts 不弹凭据框**(CLI/服务程序没有 UI)。接收端 daemon 的 Windows 账号必须已能访问该共享:域环境通常自动;工作组用 `cmdkey /add:<主机> /user:<用户> /pass:<密码>`(存凭据管理器,一次即可)或 `net use \\NAS\share /user:<用户> <密码>`。
- daemon **启动时**与 **inbox 注册时**都会做目录可写预检(建临时文件探测),凭据/权限问题在提交传输**之前**暴露,错误信息附带可操作提示(如 1326 → 给出 cmdkey 命令)。
- 传输速率受 SMB 带宽限制;fts 内部吞吐不受影响。

```powershell
# 一次性建立凭据后,UNC 直接作为接收目录
cmdkey /add:nas01 /user:svc_fts /pass:***
fts daemon -c config.json          # config.json: "out_dir": "\\\\nas01\\share\\fts"
```

常见 OS 错误映射(自动附带提示):5 拒绝访问(权限)、1326 登录失败(凭据)、53/67 路径不存在(主机/共享名)、1219 凭据会话冲突(`net use * /delete`)。

---

## 12. 版本与兼容性承诺

| 维度 | 当前 | 承诺(v1.x) |
|---|---|---|
| gRPC 包 | `fts.v1` | 字段只增不改/不删;新 RPC 新增 |
| REST 路径 | `/v1/...` | 路径与已发布字段稳定;新增字段向后兼容 |
| 线协议 | fts-protocol v1(Hello 协商) | 版本不匹配即拒绝(INCOMPATIBLE_VERSION) |
| C ABI | `fts_abi_version()==1` | 变更递增版本号 |
| 事件 | 未知 type 必须忽略 | 新事件类型不破坏旧消费方 |
| 错误码 | 0-7 已分配 | 不复用;新增用新码 |

破坏性变更(删字段/改语义)将提升主版本(REST `/v2`、gRPC `fts.v2`、ABI v2)。

---

## 附录 A:典型集成流程

**force 级干净部署(多客户端 → 同一接收端,每客户端独立目录):**

```
0. Z:   fts daemon -c config.json --max-sessions 3           (记录启动日志中的证书指纹)
1. A:   POST /v1/inbox {"out_dir":"D:\\fromA"}               → tokenA(目录模式)
2. A:   POST /v1/inbox/{tokenA}/purge {"force":true}         ← 可选:干净复制(危险,集成端自行确认)
3. A:   POST /v1/transfers {paths:[...], dst_addr, fingerprint, ticket:tokenA} → task_id
4. A:   GET /v1/transfers/{task_id}/events                   → file/progress/file_done/result
5. B、C:重复 1-4(各自 token;Z 端最多 max_sessions 个会话并发,其余排队)
失败处理:error_code=5 → 原样重发(续传);error_code=4 → 数据不可信,人工介入;
          网络路径凭据失效 → ReserveInbox 重带 credential。
```

**单文件快速通道(CLI):**

```powershell
fts send .\big.bin --to 100.64.0.5:4433 --fingerprint <FP> --json
if ($LASTEXITCODE -ne 0) { throw "exit=$LASTEXITCODE" }
```

## 附录 B:已知限制

- 回环基准 ≠ 真实网络(两台物理机数字待测)。
- 无 Nack 级块重传:单块损坏 → 整个文件失败(重发续传)。
- 无 FEC/BBR(设计见 `plan/fts-design.md` 第 9 节)。
- TCP 回退会话全局互斥(QUIC 会话并发);TCP 回退无 TLS。
- REST/gRPC 无用户鉴权;daemon 重启后任务表与 inbox token 清空。
- `fts_last_error` 为全局单槽(多线程 C 宿主需立即拷贝)。

---

*文档结束 — API.md v1.0(与 fts v0.1.0 实现同步)*
