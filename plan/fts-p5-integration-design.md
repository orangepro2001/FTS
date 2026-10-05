# fts P5 — 集成能力规划(inbox / 多文件 / 并发会话 / gRPC)

> **版本**:v1.0
> **日期**:2026-10-05
> **父文档**:`fts-design.md` v1.1、`fts-api-spec.md` v1.0
> **背景**:集成方将通过 gRPC/REST 把 fts 嵌入业务:多个客户端(A/B/C)向同一服务端 Z 传输,每个客户端有独立目标目录,每客户端批量十几个文件。P3 的 daemon 一次只会话、单目录、单文件,不满足。
> **原则**:合理但好用(Ponytail)—— 每项先满足集成主路径,复杂能力(递归目录、断点级 UI)不做。

---

## 1. 四个工作项总览

| # | 工作项 | 解决的问题 | 规模 |
|---|---|---|---|
| W1 | **inbox 预注册** | 接收目录 per-client/task,而非 daemon 启动时全局唯一 | 小 |
| W2 | **发送端多文件** | 十几个文件共享一次握手,消除逐文件握手/manifest 往返 | 中 |
| W3 | **接收端并发会话** | 慢客户端(A 走 DERP)不队头阻塞快客户端(B/C 在 LAN) | 小 |
| W4 | **gRPC(tonic)** | 集成方点名 gRPC 调用方式 | 中 |

依赖关系与执行顺序:**W1 → W2 →(W3、W4 可并行)**。W1/W2 都扩展线协议,一起改一次;W3 只动 daemon 循环;W4 依赖 W1 的 Inbox 语义暴露成 RPC。

---

## 2. W1:inbox 预注册(per-task 接收目录)

### 2.1 语义

```
集成方(Z 侧程序)                    daemon(Z)                 发送端(A)
      │  POST /v1/inbox                  │                          │
      │  {out_dir, file_name?}           │                          │
      │◄─ {token, expires_in}            │                          │
      │  (把 token 交给 A 的调度方)        │                          │
      │                                  │◄─ Manifest{ticket} ──────│
      │                                  │ 查表:token→out_dir       │
      │                                  │ 文件落入 out_dir/文件名    │
```

- **token**:一次性(用后即焚),有效期默认 10 分钟(对齐 t1-relay token 设计),UUID。
- **绑定**:`file_name` 可选。`Some(name)` = 只接受该文件名(防 token 挪用);`None` = 接受任意文件名(**目录模式**,"客户端 B 的 12 个文件都落 D:\fromB" 用这个)。
- **兼容**:Manifest 无 ticket → 落 daemon 默认 `--out` 目录,行为与 P3 完全一致。
- **拒绝行为**:ticket 不存在/过期/文件名不匹配 → `ManifestAck{ok:false, reason}`,发送端以 `AUTH_FAILED`(退出码 7)失败。不降级、不回退默认目录(静默落错目录比失败更糟)。

### 2.2 接口

REST(已有 daemon):
```
POST /v1/inbox          {out_dir, file_name?}  → {token, expires_in: 600}
DELETE /v1/inbox/{token}                        → 撤销(可选)
```
CLI(发送):
```
fts send ... --ticket <TOKEN>
```
REST submit 新字段:`ticket: Option<String>`。

### 2.3 线协议变更

`Manifest` 增加:`ticket: Option<String>`(`#[serde(default)]`,旧端点兼容:未知字段忽略)。
接收端自检顺序:Hello → Manifest(root 自检)→ **ticket 查表** → 确定落盘目录 → ManifestAck。

---

## 3. W2:发送端多文件(单连接多 manifest)

### 3.1 语义

```
fts send file1.bin file2.bin D:\logs --to <ADDR> --ticket <T> ...
```
- 位置参数接受**多个路径**;目录展开为其中**直接文件**(不递归,避免相对路径结构与重名问题;文档明示)。
- 展开后**重名检测**:两个路径 basename 相同 → 参数错误退出码 2(接收端按文件名落盘,重名必冲突)。
- 单 QUIC/TCP 连接复用:握手一次;**逐文件** `Manifest_i → [chunks_i] → Finish_i → FinishAck_i` 串行;每个文件独立 BLAKE3/续传位图(机制不变)。
- 一个文件失败:默认**中止整个任务**(快失败;十几个文件里第 3 个校验失败,继续传剩下的是浪费)。`--keep-going` 留待真实需求出现再加(ponytail)。
- inbox 目录模式 + 多文件 = 集成主路径:一次 `POST /v1/inbox {out_dir}`(不带 file_name)拿 token,一次 submit 传 12 个文件。

### 3.2 事件流扩展

```
{"type":"task",...}
{"type":"file","name":"a.bin","index":1,"total":12}          ← 新事件类型
{"type":"progress",...}                                       ← 当前文件字节
{"type":"file_done","name":"a.bin","checksum_ok":true,"seconds":3.2}  ← 新事件类型
...
{"type":"result","state":"COMPLETED",...}                     ← 整体终态
```
未知事件类型被下游忽略(契约 11.2),向后兼容。

### 3.3 REST submit 变更

```jsonc
POST /v1/transfers
{ "paths": ["C:\a.bin", "C:\b.bin"],   // 替代 src_path(保留兼容:单路径仍可用 src_path)
  "ticket": "...", ... }
```

### 3.4 续传

机制不变(per-file checkpoint)。多文件时接收端逐文件处理,中断后重发同批文件:已完成文件秒过(checkpoint 位图满 → 发送端跳过),半成品文件续传。

---

## 4. W3:接收端并发会话

- **`fts daemon --max-sessions N`**(默认 **3**):每个会话独立 tokio 任务,`Semaphore(N)` 限流;超出者在 QUIC/TCP accept 队列等待(现有行为,天然排队,无需应用层队列)。
- 为什么默认 3 不是 1 或 10:隔离慢客户端(2 个快 + 1 个慢互不拖累)同时磁盘顺序写压力可控;10 个并发写盘在 HDD 上互相拖垮(ponytail:默认保守,压测后再调)。
- **CLI `fts recv` 保持单会话语义**(传一个文件/一批文件即退出)—— 面向人;并发是 daemon(面向集成)的能力。
- 会话间隔离:每会话独立落盘目录(经 W1 ticket)或共享默认目录(不同文件名,不冲突)。
- 结构改动:`recv.rs` 拆出 `bind_listeners()` 与 `handle_incoming()`(单会话处理),daemon 循环 `semaphore + spawn`。

---

## 5. W4:gRPC(tonic)

### 5.1 范围(api-spec 7.2 的 P5 子集)

```protobuf
service Fts {
  rpc Capabilities(Empty) returns (CapabilitiesReply);
  rpc ReserveInbox(ReserveRequest) returns (InboxToken);   // W1 的 gRPC 形态
  rpc Submit(TransferRequest) returns (TransferHandle);
  rpc Observe(ObserveRequest) returns (stream TransferEvent);  // server-streaming
  rpc Status(TaskRef) returns (TransferStatus);
  rpc Cancel(TaskRef) returns (Ack);
}
```
- proto 定义放 `proto/fts.proto`(对齐 api-spec 7.2,新增 ReserveInbox;字段编号沿用 api-spec,新增字段用新 tag)。
- daemon 同进程提供:**REST(axum)与 gRPC(tonic)并存**。gRPC 独立端口(默认 `127.0.0.1:50051`,对齐 api-spec)。
- 事件流:内部已有 `broadcast<String>`(NDJSON 文本)。gRPC 侧把 JSON 字符串解析为 proto `TransferEvent`(oneof);解析失败的事件跳过(向前兼容)。

### 5.2 构建风险(执行时第一件事验证)

`tonic-build` 默认依赖系统 `protoc`。两个方案:
1. 若本机有 protoc → 直接 tonic-build;
2. 无 protoc → **protox**(纯 Rust proto 编译器,tonic-build 的 file_descriptor 输入),零系统依赖。

### 5.3 安全

对齐 api-spec 7.6:仅 localhost,无鉴权(与 REST 同级),文档声明。

---

## 6. 测试与验收

| # | 用例 | 验收 |
|---|---|---|
| T1 | inbox 目录模式:A 传 3 个文件 → 全部落指定目录 | 退出码 0;文件名/内容一致;token 用后失效 |
| T2 | inbox 文件绑定:token 绑 a.bin,发 b.bin | 发送端退出码 7;b.bin 未落盘 |
| T3 | token 过期/撤销 | 同 T2 |
| T4 | 无 ticket 回归 | 落默认 `--out`,行为与 P3 一致 |
| T5 | 多文件:12 文件(3 目录展开)单连接传输 | 握手仅 1 次(日志确认);12 文件全部校验通过 |
| T6 | 多文件中断续传:第 7 个文件时强杀 → 重发同批 | 前 6 个秒过,第 7 个续传 |
| T7 | 重名检测 | 退出码 2 |
| T8 | 并发会话:daemon `--max-sessions 2`,两个发送端同时传 | 都成功;第三个发送端排队等待 |
| T9 | gRPC:ReserveInbox → Submit → Observe 流 → Status | Python grpcio 走通全流程 |
| T10 | 回归:单文件无 ticket、续传、TCP 回退、cancel | 与 P3 基线一致 |

## 7. 明确不做(本轮)

- 递归目录传输(相对路径结构):等真实需求
- `--keep-going`(部分失败继续):同上
- gRPC TLS / 远程暴露:api-spec 7.6 明确默认 localhost
- 接收端向 REST 推 per-session 事件:daemon 日志已够用(ponytail)
- Windows 服务化:NSSM 外部解决

## 8. 执行顺序

1. W1+W2:协议 v1.1(ticket + 多文件会话)→ 发送端多文件 CLI → REST(inbox + submit.paths)
2. W3:recv.rs 拆分 + daemon `--max-sessions`
3. W4:proto + tonic(protox 或 protoc)→ daemon 集成 gRPC
4. T1~T10 回归 + dev-log/README 更新
