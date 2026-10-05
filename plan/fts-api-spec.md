# fts 接口与协议规范(调用文档)

> **项目**:高速文件传输工具 `fts`
> **文档版本**:v1.0(契约冻结)
> **日期**:2026-10-05
> **父文档**:`fts-design.md` v1.1、`fts-t1-relay-design.md`
> **性质**:**契约先行**——本规范在实现之前冻结。实现须符合本规范;若实现需要偏离,须先修订本规范。
> **目标读者**:集成方开发者(任何语言)、fts 实现者、测试工程师

---

## 目录

- [1. 交付概览](#1-交付概览)
  - [1.1 接口矩阵](#11-接口矩阵)
  - [1.2 两套协议,不要混淆](#12-两套协议不要混淆)
  - [1.3 契约冻结原则](#13-契约冻结原则)
- [2. 统一任务模型](#2-统一任务模型)
- [3. 接口 A:命令行(CLI)](#3-接口-acommand-linecli)
- [4. 接口 B:Rust 库 API](#4-接口-brust-库-api)
- [5. 接口 C:C ABI DLL](#5-接口-cc-abi-dll)
- [6. 接口 D:命名管道 IPC](#6-接口-d命名管道-ipc)
- [7. 接口 E:本地 gRPC / HTTP](#7-接口-e本地-grpc--http)
- [8. 线协议规范(fts-protocol v1)](#8-线协议规范fts-protocol-v1)
- [9. 事件与错误模型](#9-事件与错误模型)
- [10. 多语言调用示例](#10-多语言调用示例)
- [11. 版本化与兼容性政策](#11-版本化与兼容性政策)
- [12. 附录](#12-附录)

---

## 1. 交付概览

### 1.1 接口矩阵

`fts` 对外提供五种接口,全部共享同一套核心逻辑与任务语义:

| # | 接口 | 技术 | 适用对象 | 优先级 | 何时用 |
|---|---|---|---|---|---|
| **A** | **CLI** | clap,含 `--json` 输出模式 | 脚本 / PowerShell / CI | **P0** | 人工操作、脚本编排 |
| **B** | **Rust 库** | crate,稳定公开 API | Rust 程序 | **P0** | 深度集成、嵌入式使用 |
| **C** | **C ABI DLL** | `cdylib` + cbindgen | C / C++ / **C#** / Go(P/Invoke) | **P1** | Windows 原生程序集成 |
| **D** | **命名管道 IPC** | Windows Named Pipe(消息模式) | 同机多进程 | **P1** | 与常驻 daemon 通信 |
| **E** | **本地 gRPC/HTTP** | tonic / axum,监听 localhost | 跨语言 / 前端 | **P2** | 语言无绑定、远程管理 |

> **选型建议(Windows)**:C#/C++ 程序优先用 **C 接口**(最直接);脚本用 **CLI**;同机多进程共享一个 daemon 用 **命名管道**;跨语言或需要进度推送用 **gRPC**。

### 1.2 两套协议,不要混淆

这是集成方最容易误解的地方,务必区分:

| | **线协议(wire protocol)** | **控制协议(control API)** |
|---|---|---|
| 文档位置 | 第 8 节(`fts-protocol v1`) | 第 6、7 节 |
| 谁与谁通信 | **两个 fts 端点之间**(源 ↔ 目标,可能经中继) | **你的程序 ↔ fts daemon** |
| 内容 | QUIC 流上的分块、manifest、校验、续传 | 任务提交、状态查询、事件订阅 |
| 你会直接实现它吗 | **不会**(除非你要写第三方端点) | **会**(这是你调用 fts 的接口) |

> 99% 的集成方只需要 **控制协议**(CLI / C ABI / IPC / gRPC)。线协议是 fts 内部两端协商用的,你无需关心——除非你要用其他语言实现一个与 fts 兼容的端点。

### 1.3 契约冻结原则

- 本文档定义的所有**函数签名、消息结构、错误码**在 v1.x 内保证向后兼容;
- 任何破坏性变更必须提升主版本号(v2.0);
- 新增能力一律通过**能力位图(capability bitmap)**协商,不改变既有字段含义;
- 所有结构**预留保留字段**,以便将来扩展。

---

## 2. 统一任务模型

**五种接口共享同一个状态机与语义**——这是"一套核心、多种外壳"的契约体现。

### 2.1 状态机

```
                 ┌──────────┐
   submit ──────►│ PENDING  │
                 └────┬─────┘
                      │ 建立连接 / 探测路径
                      ▼
                 ┌──────────┐   cancel   ┌───────────┐
                 │ RUNNING  │───────────►│ CANCELLED │
                 └────┬─────┘            └───────────┘
                      │
        ┌─────────────┼─────────────┐
        │             │             │
        ▼             ▼             ▼
  ┌───────────┐ ┌───────────┐ ┌───────────┐
  │ COMPLETED │ │  FAILED   │ │  PAUSED   │
  └───────────┘ └───────────┘ └─────┬─────┘
                                    │ resume
                                    └────► RUNNING
```

| 状态 | 含义 | 是否终态 |
|---|---|---|
| `PENDING` | 已提交,未开始传输 | 否 |
| `RUNNING` | 传输中(含降级/切换中继) | 否 |
| `PAUSED` | 暂停,可恢复 | 否 |
| `COMPLETED` | 完成,**且校验通过** | ✅ 是 |
| `FAILED` | 失败(附错误码与原因) | ✅ 是 |
| `CANCELLED` | 被取消 | ✅ 是 |

> **重要**:`COMPLETED` 的语义是"传输完成 **且** BLAKE3 校验通过"。校验失败一律为 `FAILED`,绝不出现"传完了但内容不确定"的中间态。

### 2.2 生命周期四步

所有接口都遵循同一语义:

```
① submit    → 返回 task_id
② observe   → 订阅事件流(进度/降级/路径变更)
③ control   → pause / resume / cancel
④ result    → 终态 + 校验结果 + 统计
```

### 2.3 TaskId

- 类型:UUID v4 的字符串形式(如 `"3f2b8c1a-..."`);
- 全局唯一,跨接口一致(CLI 生成的 task_id 可被 gRPC 查询);
- daemon 重启后仍可在日志/journal 中检索。

### 2.4 进度报告

| 字段 | 类型 | 说明 |
|---|---|---|
| `bytes_total` | u64 | 总字节数(源端已知时) |
| `bytes_done` | u64 | 已完成(含校验通过的) |
| `chunks_total` | u32 | 总块数 |
| `chunks_done` | u32 | 已完成块数 |
| `throughput_bps` | u64 | 瞬时吞吐(滑动窗口) |
| `eta_seconds` | u32? | 预估剩余秒数(不可估时为 null) |
| `path` | object | 当前路径状态(见第 9 节) |

> **契约**:`bytes_done` 只统计**已校验通过**的块。因此进度条不会出现"到 100% 才发现校验失败"的情况。

---

## 3. 接口 A:命令行(CLI)

### 3.1 子命令总览

| 子命令 | 作用 |
|---|---|
| `fts send` | 发送文件 |
| `fts recv` | 接收文件 |
| `fts status` | 查询任务状态 |
| `fts cancel` | 取消任务 |
| `fts pause` / `fts resume` | 暂停 / 恢复 |
| `fts path` | 查看当前链路类型(直连/中继) |
| `fts diagnose` | 导出诊断包 |
| `fts relay serve` | 启动自建中继(T1.1) |
| `fts daemon` | 启动常驻服务(供 IPC/gRPC 调用) |
| `fts version` | 版本信息(含协议版本) |

### 3.2 关键参数

```
fts send <PATH> --to <ADDR> [OPTIONS]
fts recv --listen <ADDR> --out <DIR> [OPTIONS]
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `--to <ADDR>` | — | 目标地址,如 `100.64.0.5:4433` |
| `--streams <N\|auto>` | `auto` | 并发流数 |
| `--chunk-size <BYTES>` | `auto` | 分块大小;`auto` 按 BDP 自适应 |
| `--checksum <blake3>` | `blake3` | **不可关闭**(硬约束);仅保留字段以便未来算法升级 |
| `--relay <auto\|off\|prefer\|require\|ADDR>` | `auto` | 中继策略 |
| `--transport <quic\|tcp\|auto>` | `auto` | 传输层 |
| `--compress <auto\|on\|off>` | `auto` | 压缩 |
| `--resume` | `false` | 从 checkpoint 续传 |
| `--json` | `false` | **机器可读输出(集成必用)** |
| `-v` / `-vv` | — | 提升日志级别 |

### 3.3 退出码(契约)

| 退出码 | 含义 |
|---|---|
| `0` | 成功且校验通过 |
| `1` | 通用错误 |
| `2` | 参数错误 |
| `3` | 连接失败 |
| `4` | **校验失败**(数据不可信,必须处理) |
| `5` | 传输中断且不可续传 |
| `6` | 被取消 |
| `7` | 权限 / 认证失败 |

> 脚本集成**必须**检查退出码,尤其 `4`(校验失败)——它意味着数据不可信。

### 3.4 `--json` 输出模式(集成关键)

启用 `--json` 后,stdout 输出 **NDJSON**(每行一个 JSON 对象),stderr 保持人类可读日志:

```jsonc
{"type":"task","task_id":"3f2b8c1a-...","state":"RUNNING"}
{"type":"progress","bytes_done":1048576,"bytes_total":10737418240,"throughput_bps":94371840,"eta_seconds":113}
{"type":"path","link":"direct","rtt_ms":12}
{"type":"path","link":"peer_relay","relay":"100.64.0.9","rtt_ms":42,"previous":"derp"}
{"type":"degradation","from":"L1","to":"L3","reason":"loss_rate_6.4pct"}
{"type":"result","state":"COMPLETED","checksum_ok":true,"duration_seconds":42.5}
```

**契约**:事件类型至少包含 `task` / `progress` / `path` / `degradation` / `result` / `error`。新增事件类型不破坏兼容(未知类型应被忽略)。

### 3.5 示例

```powershell
# 基本发送
fts send .\data.bin --to 100.64.0.5:4433

# 脚本集成:JSON 输出 + 检查退出码
fts send .\data.bin --to 100.64.0.5:4433 --json
if ($LASTEXITCODE -ne 0) { Write-Error "传输失败: $LASTEXITCODE" }

# 查看链路类型
fts path --to 100.64.0.5
```

---

## 4. 接口 B:Rust 库 API

### 4.1 稳定性承诺

- 公开 API 位于 `fts` crate 的根;
- 内部类型不导出;
- 语义化版本:破坏性变更提升主版本。

### 4.2 核心 API(异步)

```rust
use fts::{Client, TransferConfig, Direction, Event, TaskId};

#[tokio::main]
async fn main() -> Result<(), fts::Error> {
    let client = Client::builder()
        .relay_policy(RelayPolicy::Auto)
        .build()?;

    // ① 提交任务
    let cfg = TransferConfig::send("./data.bin")
        .to("100.64.0.5:4433")
        .streams(Streams::Auto)
        .chunk_size(ChunkSize::Auto)
        .checksum(Checksum::Blake3);   // 不可选 None

    let task: TaskHandle = client.submit(cfg).await?;
    let id: TaskId = task.id();

    // ② 订阅事件
    let mut events = task.events();
    while let Some(ev) = events.next().await {
        match ev {
            Event::Progress(p) => println!("{}%", p.percent()),
            Event::PathChanged { to, .. } => println!("path -> {:?}", to),
            Event::Degraded { from, to, reason } => eprintln!("degraded {from:?}->{to:?}: {reason}"),
            Event::Finished(r) => { assert!(r.checksum_ok); break; }
            _ => {}
        }
    }

    // ③ 控制
    // task.pause().await?;
    // task.cancel().await?;
    Ok(())
}
```

### 4.3 关键类型

| 类型 | 作用 |
|---|---|
| `Client` | 客户端入口,配置连接/中继/传输策略 |
| `Client::submit(cfg) -> TaskHandle` | 提交任务 |
| `TaskHandle` | 任务句柄:`id()` / `events()` / `pause()` / `resume()` / `cancel()` / `status()` |
| `TransferConfig` | 传输配置(方向、路径、地址、流数、块大小、校验) |
| `Event` | 事件枚举(Progress / PathChanged / Degraded / Finished / Failed / …) |
| `Checksum` | 校验算法枚举(**无 `None` 变体**) |
| `RelayPolicy` | 中继策略(Off / Auto / Prefer / Require / Specific) |
| `Error` | 错误类型(见第 10 节错误码) |

> **设计约束**:`Checksum` 枚举**故意不提供"关闭"选项**——这是把硬约束"校验不可关闭"编码进类型系统,让它在编译期就不可违反。

### 4.4 阻塞式 API(便于嵌入式)

对不便使用 async 的宿主,提供阻塞封装:

```rust
let result = client.submit_blocking(cfg, |ev| {
    if let Event::Progress(p) = ev { print!("\r{}%", p.percent()); }
})?;
```

### 4.5 服务器端

```rust
use fts::{Server, ServerConfig};

let server = Server::bind("0.0.0.0:4433")
    .out_dir("./downloads")
    .on_complete(|r| println!("received: {:?}", r.path))
    .build()?;
server.run().await?;
```

---

## 5. 接口 C:C ABI DLL

> **为什么用 C ABI**:Windows 上被 C#/C++/Go 调用的稳定跨语言边界,标准做法是 **C ABI**——它定义了明确的调用约定;Rust 自身 ABI 不稳定,直接暴露 Rust 类型不可行。头文件用 `cbindgen` 从源码生成([FFI 实践](https://www.ottorask.com/blog/creating-an-ffi-compatible-c-abi-library-in-rust))。`abi_stable` 仅适用于 Rust↔Rust 插件场景,本项目不需要([abi_stable](https://docs.rs/abi_stable/))。

### 5.1 交付物

| 文件 | 说明 |
|---|---|
| `fts.dll` | 动态库(x64 / ARM64) |
| `fts.h` | C 头文件(cbindgen 生成) |
| `fts.lib` | 导入库(MSVC) |
| `Fts.cs` | 官方 C# P/Invoke 封装(便捷层) |

### 5.2 头文件骨架

```c
/* fts.h — 由 cbindgen 生成,此处为契约示意 */
#ifndef FTS_H
#define FTS_H

#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---- 错误码 ---- */
typedef enum {
    FTS_OK                  = 0,
    FTS_E_INVALID_ARG       = 1,
    FTS_E_CONNECT           = 3,
    FTS_E_CHECKSUM_FAILED   = 4,   /* 数据不可信 */
    FTS_E_INTERRUPTED       = 5,
    FTS_E_CANCELLED         = 6,
    FTS_E_AUTH              = 7,
    FTS_E_IO                = 8,
    FTS_E_BUFFER_TOO_SMALL  = 9,
    FTS_E_NOT_FOUND         = 10,
    FTS_E_INTERNAL          = 255
} fts_error_t;

/* ---- 不透明句柄 ---- */
typedef struct fts_task_t fts_task_t;   /* 不透明,不可解引用 */

/* ---- 任务配置 ---- */
typedef struct {
    const char* src_path;        /* UTF-8,发送时必填 */
    const char* dst_addr;        /* "100.64.0.5:4433" */
    const char* out_dir;         /* 接收时的输出目录 */
    int32_t     direction;       /* 0 = send, 1 = recv */
    int32_t     streams;         /* 0 = auto */
    uint64_t    chunk_size;      /* 0 = auto */
    int32_t     relay_policy;    /* 0=off 1=auto 2=prefer 3=require */
    const char* relay_addr;      /* 可空 */
    int32_t     transport;       /* 0=auto 1=quic 2=tcp */
} fts_config_t;

/* ---- 进度结构 ---- */
typedef struct {
    uint64_t bytes_total;
    uint64_t bytes_done;
    uint32_t chunks_total;
    uint32_t chunks_done;
    uint64_t throughput_bps;
    uint32_t eta_seconds;
    int32_t  path_link;          /* 0=direct 1=peer_relay 2=derp 3=tcp */
} fts_progress_t;

/* ---- 事件回调 ----
 * event_type: 0=state 1=progress 2=path 3=degradation 4=finished 5=error
 * user_data : 提交时传入的上下文指针
 * 返回非 0 表示请求取消本次传输
 */
typedef int32_t (*fts_event_cb)(int32_t event_type,
                                const void* payload,   /* 见下表 */
                                void* user_data);

/* ---- 生命周期 ---- */
const char* fts_version(void);                     /* 静态字符串,勿释放 */
void        fts_free(void* ptr);                   /* 释放 fts 分配的内存 */

/* ---- 任务 API ---- */
fts_error_t fts_task_submit(const fts_config_t* cfg,
                            fts_event_cb cb,
                            void* user_data,
                            fts_task_t** out_task);

fts_error_t fts_task_poll(fts_task_t* task, fts_progress_t* out);
fts_error_t fts_task_state(fts_task_t* task);      /* 返回状态枚举 */
fts_error_t fts_task_pause(fts_task_t* task);
fts_error_t fts_task_resume(fts_task_t* task);
fts_error_t fts_task_cancel(fts_task_t* task);
void        fts_task_release(fts_task_t* task);    /* 释放句柄 */

/* ---- 阻塞等待(便捷) ---- */
fts_error_t fts_task_wait(fts_task_t* task, uint32_t timeout_ms);

/* ---- 错误详情 ---- */
const char* fts_last_error_message(const fts_task_t* task);
void        fts_string_free(char* s);

#ifdef __cplusplus
}
#endif
#endif /* FTS_H */
```

### 5.3 调用模型(两条路径,任选)

**路径 1:回调驱动(推荐)**

```
fts_task_submit(cfg, on_event, user_data, &task)
        │
        ├─ 回调在 fts 内部线程被调用
        │  on_event(FTS_EVT_PROGRESS, &progress, user_data)
        │  on_event(FTS_EVT_PATH,     &path_info, user_data)
        │  on_event(FTS_EVT_FINISHED, &result,    user_data)
        │
        └─ 回调返回非 0 → 传输被取消
```

**路径 2:轮询驱动(简单)**

```
fts_task_submit(cfg, NULL, NULL, &task)
loop:
    fts_task_poll(task, &progress)
    sleep(...)
fts_task_wait(task, timeout)   // 或阻塞等待
fts_task_release(task)
```

### 5.4 内存所有权规则(务必遵守)

这是 C ABI 最容易出错的地方,定死规则:

| 谁分配 | 谁释放 | 规则 |
|---|---|---|
| fts | fts | `fts_version()` 返回**静态字符串**,**调用方不得释放** |
| fts | fts | `fts_last_error_message()` 返回值由 fts 管理,**下次调用即失效**;需保留时**必须拷贝** |
| fts | 调用方 | 标注为 `fts_string_free` 的返回值,调用方用该函数释放 |
| 调用方 | 调用方 | 传入的 `fts_config_t` 及其字符串指针,调用方负责其生命周期,**须覆盖整个传输期间** |
| fts | 调用方 | `fts_task_t*` 由 `fts_task_submit` 分配,调用方须用 `fts_task_release` 释放 |

> **红线**:**不要**跨 DLL 边界 free(在 fts 内部分配的内存不能由宿主的 `free()` 释放,反之亦然)。任何 fts 分配、宿主释放的内存,必须经由 `fts_free`。

### 5.5 线程安全

| 对象 | 线程安全 |
|---|---|
| `fts_task_t*` | ✅ 各控制函数可从任意线程调用 |
| 回调 | 同一任务的回调**串行**执行(不并发) |
| `Client` 配置 | 提交时拷贝,后续修改配置不影响已提交任务 |

### 5.6 C# 调用示例

```csharp
using System;
using System.Runtime.InteropServices;

public static class Fts {
    const string Dll = "fts.dll";

    public delegate int EventCb(int type, IntPtr payload, IntPtr userData);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    public static extern IntPtr fts_version();

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    public static extern int fts_task_submit(ref FtsConfig cfg, EventCb cb,
                                             IntPtr userData, out IntPtr task);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    public static extern int fts_task_wait(IntPtr task, uint timeoutMs);

    [DllImport(Dll, CallingConvention = CallingConvention.Cdecl)]
    public static extern void fts_task_release(IntPtr task);
}

// 使用
IntPtr task;
var cb = new Fts.EventCb((t, p, u) => {
    if (t == 1) { /* progress */ }
    return 0;                       // 0 = 继续
});
var rc = Fts.fts_task_submit(ref cfg, cb, IntPtr.Zero, out task);
if (rc != 0) { /* 处理错误码 */ }
Fts.fts_task_wait(task, 60000);
Fts.fts_task_release(task);
```

---

## 6. 接口 D:命名管道 IPC

### 6.1 用途与前提

- 同一台 Windows 机器上,**多个程序共享一个常驻 `fts daemon`**;
- 适用于:桌面应用、服务、脚本需要复用同一传输引擎;
- 管道名:`\\.\pipe\fts`(可配置)。

### 6.2 管道模式(关键设计)

采用**消息模式(message mode)**而非字节模式,因为这是请求/响应语义,消息模式天然保留消息边界([Windows Named Pipes 实践](https://comcomponent.com/en/blog/windows-named-pipes-practical-guide/)):

| 设置 | 值 | 原因 |
|---|---|---|
| 模式 | `PIPE_TYPE_MESSAGE \| PIPE_READMODE_MESSAGE` | 保留消息边界 |
| 方向 | 双向(`PIPE_ACCESS_DUPLEX`) | 请求 + 响应 + 事件推送 |
| 实例 | 多实例并发 | 支持多客户端 |
| I/O | overlapped | 异步,避免阻塞([Microsoft 示例](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-server-using-overlapped-i-o)) |

> **客户端也必须设置 `PIPE_READMODE_MESSAGE`**,否则消息模式不生效——这是常见坑。

### 6.3 帧格式

即使使用消息模式,仍采用**长度前缀**以便跨平台与健壮性:

```
┌────────────┬────────────┬──────────────────────────┐
│ magic (4B) │ len (4B)   │ payload (len bytes)      │
│ "FTS1"     │ u32 LE     │ protobuf / JSON          │
└────────────┴────────────┴──────────────────────────┘
```

| 字段 | 说明 |
|---|---|
| `magic` | 固定 `"FTS1"`,用于快速识别错帧 |
| `len` | payload 字节数,上限 **16 MiB**(防滥用) |
| `payload` | 序列化消息(protobuf 优先,JSON 可配置) |

### 6.4 消息类型

| 方向 | 消息 | 说明 |
|---|---|---|
| C→S | `Submit` | 提交传输任务 |
| C→S | `Subscribe` | 订阅某任务的事件 |
| C→S | `Pause` / `Resume` / `Cancel` | 控制 |
| C→S | `Status` | 查询状态 |
| C→S | `PathQuery` | 查询链路类型 |
| C→S | `Diagnose` | 请求诊断包 |
| S→C | `SubmitAck` | 返回 task_id |
| S→C | `Event` | 推送事件(progress / path / degradation / …) |
| S→C | `StatusReply` | 状态回复 |
| S→C | `Error` | 错误回复 |

### 6.5 请求 / 响应往返

```
Client                              Daemon
  │                                   │
  │── Submit{cfg} ───────────────────►│
  │◄───────────── SubmitAck{task_id} ─│
  │── Subscribe{task_id} ────────────►│
  │◄───────────── Event{progress} ────│
  │◄───────────── Event{path} ────────│
  │◄───────────── Event{finished} ────│
  │                                   │
```

**契约**:每个请求消息带 `request_id`,响应/错误回带同一 `request_id`,便于异步匹配。

### 6.6 安全

| 措施 | 说明 |
|---|---|
| ACL | 管道仅允许本机指定用户/组访问 |
| 身份校验 | 服务端校验客户端 SID |
| 拒绝远程 | **禁止**跨机器访问管道(默认 `PIPE_REJECT_REMOTE_CLIENTS`) |
| 消息上限 | 16 MiB,防内存耗尽 |

---

## 7. 接口 E:本地 gRPC / HTTP

### 7.1 定位

- 监听 **localhost**(默认 `127.0.0.1:50051`),默认不对外暴露;
- 用于:**跨语言集成**、需要**进度事件流**的场景、远程管理(需显式开启);
- gRPC 用 **server-streaming** 推送长生命周期事件——这是 gRPC 处理长时流程的推荐做法([gRPC 性能最佳实践](https://grpc.io/docs/guides/performance/))。

### 7.2 Proto 定义

```protobuf
syntax = "proto3";
package fts.v1;

service Fts {
  // 任务生命周期
  rpc Submit  (TransferRequest)  returns (TransferHandle);
  rpc Observe (ObserveRequest)   returns (stream TransferEvent);
  rpc Status  (TaskRef)          returns (TransferStatus);
  rpc Pause   (TaskRef)          returns (Ack);
  rpc Resume  (TaskRef)          returns (Ack);
  rpc Cancel  (TaskRef)          returns (Ack);

  // 诊断与路径
  rpc Path    (TaskRef)          returns (PathInfo);
  rpc Diagnose(TaskRef)          returns (DiagnosticBundle);

  // 能力探测
  rpc Capabilities(Empty)        returns (CapabilitiesReply);
}

message TransferRequest {
  string task_name     = 1;
  Direction direction  = 2;
  string src_path      = 3;   // 发送
  string dst_addr      = 4;   // "100.64.0.5:4433"
  string out_dir       = 5;   // 接收
  int32  streams       = 6;   // 0 = auto
  uint64 chunk_size    = 7;   // 0 = auto
  RelayPolicy relay    = 8;
  string relay_addr    = 9;
  Transport transport  = 10;
  bool   resume        = 11;
}

enum Direction   { DIR_UNSPECIFIED = 0; DIR_SEND = 1; DIR_RECV = 2; }
enum RelayPolicy { RELAY_OFF = 0; RELAY_AUTO = 1; RELAY_PREFER = 2; RELAY_REQUIRE = 3; }
enum Transport   { TRANSPORT_AUTO = 0; TRANSPORT_QUIC = 1; TRANSPORT_TCP = 2; }

message TransferHandle { string task_id = 1; }

message TaskRef { string task_id = 1; }

message TransferStatus {
  string  task_id      = 1;
  State   state        = 2;
  Progress progress    = 3;
  PathInfo path        = 4;
  Error   error        = 5;   // 仅 FAILED 时
}

enum State { STATE_UNSPECIFIED=0; PENDING=1; RUNNING=2; PAUSED=3;
             COMPLETED=4; FAILED=5; CANCELLED=6; }

message Progress {
  uint64 bytes_total    = 1;
  uint64 bytes_done     = 2;
  uint32 chunks_total   = 3;
  uint32 chunks_done    = 4;
  uint64 throughput_bps = 5;
  uint32 eta_seconds    = 6;
}

message PathInfo {
  LinkType link      = 1;   // DIRECT / PEER_RELAY / DERP / TCP
  string   relay      = 2;   // 中继地址(若经中继)
  uint32   rtt_ms     = 3;
  string   previous   = 4;   // 上一个链路类型(若有变更)
}

enum LinkType { LINK_UNSPECIFIED=0; DIRECT=1; PEER_RELAY=2; DERP=3; TCP=4; }

message ObserveRequest { string task_id = 1; }

message TransferEvent {
  oneof event {
    Progress     progress     = 1;
    PathInfo     path         = 2;
    Degradation  degradation  = 3;
    Finished     finished     = 4;
    Error        error        = 5;
  }
}

message Degradation {
  string from_level = 1;   // "L1"
  string to_level   = 2;   // "L3"
  string reason     = 3;   // "loss_rate_6.4pct"
}

message Finished {
  State  final_state = 1;
  bool   checksum_ok = 2;
  string out_path    = 3;
  double duration_s  = 4;
}

message Error { int32 code = 1; string message = 2; }
message Ack   { bool ok = 1; }
message Empty {}

message DiagnosticBundle { string download_url = 1; string sha256 = 2; }

message CapabilitiesReply {
  string protocol_version = 1;
  uint32 capabilities     = 2;   // 能力位图
  string fts_version      = 3;
}
```

### 7.3 能力位图

| 位 | 能力 |
|---|---|
| 0 | `multi_source`(预留,T2) |
| 1 | `relay` |
| 2 | `fec` |
| 3 | `resume` |
| 4 | `compress` |
| 5 | `diagnose` |
| 6-31 | 保留 |

> 集成方**必须先调 `Capabilities`**,据此决定可用的功能——不要假设某能力存在。

### 7.4 典型调用流程

```
1. Capabilities() → 确认 protocol_version 与 capabilities
2. Submit(req)    → 得到 task_id
3. Observe({task_id}) → 持续接收事件流直到 Finished/Error
4. (可选) Status / Pause / Resume / Cancel
```

### 7.5 HTTP/REST 备选

对无法使用 gRPC 的前端,提供等价 REST + **SSE** 事件推送:

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/v1/capabilities` | 能力探测 |
| `POST` | `/v1/transfers` | 提交,返回 `task_id` |
| `GET` | `/v1/transfers/{id}` | 状态 |
| `GET` | `/v1/transfers/{id}/events` | **SSE 事件流** |
| `POST` | `/v1/transfers/{id}/pause` \| `/resume` \| `/cancel` | 控制 |
| `GET` | `/v1/transfers/{id}/path` | 链路信息 |
| `GET` | `/v1/transfers/{id}/diagnose` | 诊断包 |

### 7.6 安全

| 项 | 默认 |
|---|---|
| 监听地址 | **localhost only** |
| 认证 | 本地 token(写入用户配置目录,0600 权限)或 Windows 命名管道式 SID 校验 |
| TLS | 本地默认关闭;若暴露到网络则**必须**开启 |
| 远程暴露 | 需显式 `--allow-remote` + token |

---

## 8. 线协议规范(fts-protocol v1)

> **本节面向**:需要实现第三方兼容端点,或深度调试的高级用户。**普通集成方无需关心。**

### 8.1 分层

```
┌──────────────────────────────────────┐
│ fts-protocol v1(本节)                │  控制帧、manifest、分块
├──────────────────────────────────────┤
│ QUIC(quinn)                          │  可靠流 + TLS 1.3
├──────────────────────────────────────┤
│ UDP                                  │
└──────────────────────────────────────┘
可选:Tailscale / WireGuard(外部)
```

### 8.2 连接建立

```
1. QUIC 握手(TLS 1.3,ALPN = "fts/1")
2. 客户端开一条【控制流】(bidirectional)
3. 双方交换 Hello:
     Hello {
       protocol_version: 1,
       capabilities:     u32 位图,
       fts_version:      "1.0.0",
       role:             source | dest
     }
4. 协商:取双方 capabilities 交集;protocol_version 必须相同,否则拒绝
```

### 8.3 控制消息

控制流上为**长度前缀帧**:

```
┌────────────┬──────────────────────┐
│ len (u32)  │ payload(len bytes)   │  payload = 序列化控制消息
└────────────┴──────────────────────┘
```

控制消息类型:

| 类型 | 方向 | 说明 |
|---|---|---|
| `Hello` | ↔ | 版本与能力协商 |
| `Manifest` | S→D | 文件元数据与分块表 |
| `ManifestAck` | D→S | 确认,或请求续传 |
| `ResumeRequest` | D→S | 携带已持有块位图 |
| `ResumePlan` | S→D | 确定要传的块集合 |
| `ChunkComplete` | D→S | 某块校验通过 |
| `ChunkNack` | D→S | 某块校验失败,请求重传 |
| `PathUpdate` | ↔ | 链路变更通知 |
| `Degrade` | ↔ | 降级通知 |
| `Finish` | S→D | 数据发完 |
| `FinishAck` | D→S | 文件级校验结果 |
| `Error` | ↔ | 错误 |
| `Bye` | ↔ | 关闭 |

### 8.4 Manifest

```jsonc
{
  "file_name": "data.bin",
  "file_size": 10737418240,
  "chunk_size": 8388608,
  "chunk_count": 1280,
  "checksum_algo": "blake3",
  "merkle_root": "af1349b9f5f9a1a6...",     // 64 hex 字符
  "chunk_hashes": ["...", "..."],           // 可省略,按需随块发送
  "manifest_sig": "base64...",               // 源私钥签名(防篡改)
  "created_at": "2026-10-05T11:00:00Z",
  "resume_supported": true
}
```

| 字段 | 说明 |
|---|---|
| `chunk_size` | 分块字节数(降级时可变更,须重新协商) |
| `merkle_root` | 分块哈希构成的 Merkle 树根,用于整体校验 |
| `manifest_sig` | 源节点私钥对 manifest 的签名,防止切片投毒 |

### 8.5 数据流与分块帧

- 每条数据流上传输**分块帧**:

```
┌──────────┬──────────┬──────────┬─────────────┐
│ chunk_id │ offset   │ len      │ payload     │
│ u32      │ u64      │ u32      │ len bytes   │
└──────────┴──────────┴──────────┴─────────────┘
```

- **分块哈希**默认随块携带(置于控制流或前缀),或按 `chunk_hashes` 表校验;
- **校验在接收端边收边执行**,失败立即 `ChunkNack`;
- 只有**校验通过的块**才计入进度(见 2.4 节契约)。

### 8.6 续传协议

```
1. 接收端扫描本地临时文件与 checkpoint
2. 计算已完整持有且校验通过的块 → 位图 bitmap
3. ResumeRequest { received_bitmap }
4. 发送端计算差集 → ResumePlan { missing_chunk_ids }
5. 只传缺失块
```

> **优势**:Merkle 树使续传可**按块**进行,无需重传整个文件。

### 8.7 终结

```
1. 源发 Finish { chunk_count }
2. 接收端校验文件级 Merkle 根
3. FinishAck { checksum_ok: bool, merkle_root }
4. 若 ok:临时文件原子重命名为最终名(Windows MoveFileEx)
5. 双方 Bye
```

> **原子落盘契约**:在 `FinishAck` 确认校验通过**之前**,目标位置**不得**出现最终文件名。这保证任何时刻崩溃都不会留下"看似完成实则损坏"的文件。

### 8.8 错误与关闭

- `Error { code, message }` 后应立即 `Bye`;
- 断线不视为失败——若 `resume_supported`,接收端保存 checkpoint 并等待重连;
- 版本不匹配 → `Error{code=INCOMPATIBLE_VERSION}` + `Bye`。

---

## 9. 事件与错误模型

### 9.1 错误码(跨接口统一)

| 码 | 名称 | 含义 | CLI 退出码 |
|---|---|---|---|
| 0 | `OK` | 成功 | 0 |
| 1 | `INVALID_ARG` | 参数错误 | 2 |
| 3 | `CONNECT_FAILED` | 连接失败 | 3 |
| 4 | `CHECKSUM_FAILED` | **校验失败,数据不可信** | 4 |
| 5 | `INTERRUPTED` | 中断且不可续传 | 5 |
| 6 | `CANCELLED` | 被取消 | 6 |
| 7 | `AUTH_FAILED` | 认证/权限失败 | 7 |
| 8 | `IO_ERROR` | 本地 I/O 错误 | 1 |
| 9 | `BUFFER_TOO_SMALL` | 缓冲区不足 | 1 |
| 10 | `NOT_FOUND` | 任务/文件不存在 | 1 |
| 11 | `INCOMPATIBLE_VERSION` | 协议版本不匹配 | 3 |
| 12 | `RELAY_UNAVAILABLE` | 中继不可用 | 3 |
| 13 | `QUOTA_EXCEEDED` | 配额超限 | 1 |
| 255 | `INTERNAL` | 内部错误 | 1 |

> **契约**:错误码 v1.x 内不复用、不改变语义。新增错误使用新码。

### 9.2 事件类型(跨接口统一)

| 事件 | 触发时机 | 级别 |
|---|---|---|
| `task` | 任务创建/状态变更 | info |
| `progress` | 周期性进度 | debug |
| `path` | 链路类型变更 | **warn** |
| `degradation` | 降级触发 | **warn** |
| `relay` | 中继选择/切换/失败 | **warn** |
| `result` | 终态 | info |
| `error` | 错误 | error |

> **契约**:`path` / `degradation` / `relay` 三类事件**永远在 warn 级别**输出——它们回答"为什么慢",绝不能被默认日志级别过滤掉(与父文档第 10 节一致)。

### 9.3 未知字段与未知枚举

- **JSON**:未知字段必须忽略(前向兼容);
- **protobuf**:依赖 proto3 未知字段保留机制;
- **未知事件类型**:忽略,不得报错。

---

## 10. 多语言调用示例

### 10.1 Python(经 gRPC)

```python
import grpc, fts_pb2, fts_pb2_grpc

ch = grpc.insecure_channel("127.0.0.1:50051")
stub = fts_pb2_grpc.FtsStub(ch)

caps = stub.Capabilities(fts_pb2.Empty())
print("protocol:", caps.protocol_version)

h = stub.Submit(fts_pb2.TransferRequest(
    direction=fts_pb2.DIR_SEND,
    src_path="./data.bin",
    dst_addr="100.64.0.5:4433",
    relay=fts_pb2.RELAY_AUTO,
    streams=0, chunk_size=0))

for ev in stub.Observe(fts_pb2.ObserveRequest(task_id=h.task_id)):
    which = ev.WhichOneof("event")
    if which == "progress":
        print(f"{ev.progress.bytes_done}/{ev.progress.bytes_total}")
    elif which == "finished":
        assert ev.finished.checksum_ok, "校验失败!"
        break
```

### 10.2 Go(经 C ABI,cgo)

```go
/*
#cgo LDFLAGS: -L. -lfts
#include "fts.h"
#include <stdlib.h>
*/
import "C"
import "unsafe"

func Send(path, addr string) C.fts_error_t {
    p := C.CString(path); defer C.free(unsafe.Pointer(p))
    a := C.CString(addr); defer C.free(unsafe.Pointer(a))

    var cfg C.fts_config_t
    cfg.direction = 0
    cfg.src_path  = p
    cfg.dst_addr  = a
    cfg.streams   = 0
    cfg.relay_policy = 1

    var task *C.fts_task_t
    rc := C.fts_task_submit(&cfg, nil, nil, &task)
    if rc != 0 { return rc }
    defer C.fts_task_release(task)

    return C.fts_task_wait(task, 600000)
}
```

### 10.3 Node.js(经 gRPC)

```javascript
const grpc = require('@grpc/grpc-js');
const { FtsClient } = require('./fts_grpc_pb');

const client = new FtsClient('127.0.0.1:50051', grpc.credentials.createInsecure());

client.submit(req, (err, handle) => {
  const stream = client.observe({ taskId: handle.getTaskId() });
  stream.on('data', (ev) => {
    if (ev.hasProgress()) console.log(ev.getProgress().getBytesDone());
    if (ev.hasFinished()) {
      if (!ev.getFinished().getChecksumOk()) throw new Error('校验失败');
      stream.cancel();
    }
  });
});
```

### 10.4 PowerShell(经 CLI + JSON)

```powershell
$out = fts send .\data.bin --to 100.64.0.5:4433 --json
$out | ForEach-Object {
    $e = $_ | ConvertFrom-Json
    switch ($e.type) {
        'progress' { Write-Progress -PercentComplete ($e.bytes_done/$e.bytes_total*100) }
        'path'     { Write-Host "path: $($e.link)" }
        'result'   { if (-not $e.checksum_ok) { throw '校验失败' } }
    }
}
if ($LASTEXITCODE -ne 0) { throw "退出码 $LASTEXITCODE" }
```

---

## 11. 版本化与兼容性政策

### 11.1 三个版本号,含义不同

| 版本 | 位置 | 说明 |
|---|---|---|
| **fts 版本** | `fts_version()` / `--version` | 产品版本,语义化 |
| **协议版本** | `Hello.protocol_version` | **线协议**,不匹配即拒绝 |
| **控制 API 版本** | gRPC `package fts.v1` | 控制接口 |

### 11.2 兼容性规则

| 变更 | 是否兼容 | 处理 |
|---|---|---|
| 新增事件类型 | ✅ | 未知类型忽略 |
| 新增 JSON 字段 | ✅ | 未知字段忽略 |
| 新增 proto 字段 | ✅ | 用新 tag |
| 新增错误码 | ✅ | 用新码 |
| 改字段语义 | ❌ | 提升协议主版本 |
| 删字段 / 改 tag | ❌ | 提升主版本 |
| 改校验算法默认 | ❌ | 提升主版本 |

### 11.3 握手失败处理

```
协议版本不匹配
    → Error{ code: INCOMPATIBLE_VERSION, message: "peer=2, local=1" }
    → Bye
    → 记录到日志,提示用户升级(不要静默重试)
```

### 11.4 能力协商

- 集成方**必须先查能力**,不得假设某能力存在;
- 请求不支持的能力 → `Error{ INVALID_ARG }`,并说明哪项不支持;
- 新增能力只占新位,**不复用旧位**。

---

## 12. 附录

### 12.1 接口选择决策树

```
你的宿主是?
├─ 命令行/脚本 ──────────────────────► A. CLI(--json)
├─ Rust 程序 ────────────────────────► B. Rust 库
├─ C / C++ / C# / Go(Windows) ──────► C. C ABI DLL
├─ 同机多个进程共享一个引擎 ─────────► D. 命名管道
└─ 其他语言 / 需事件流 / 远程管理 ───► E. gRPC / HTTP(SSE)
```

### 12.2 集成方必做清单

| # | 事项 |
|---|---|
| 1 | **检查退出码 / 错误码**,尤其 `CHECKSUM_FAILED(4)` |
| 2 | 先调 **Capabilities**,按能力位图决策 |
| 3 | 订阅并处理 **path / degradation / relay** 事件(否则无法解释性能) |
| 4 | **长传输必须支持取消与续传** |
| 5 | 遵守 **C ABI 内存所有权规则**(勿跨边界 free) |
| 6 | 处理**协议版本不匹配**并提示升级 |
| 7 | 忽略未知事件类型与未知字段(前向兼容) |
| 8 | 记录 **task_id**,便于用 `fts diagnose` 排障 |

### 12.3 最小可用集成(30 秒版)

```csharp
// C#:提交 → 等待 → 校验退出码
var rc = Fts.fts_task_submit(ref cfg, null, IntPtr.Zero, out var task);
if (rc != 0) throw new Exception($"submit failed: {rc}");
Fts.fts_task_wait(task, 600000);
var state = Fts.fts_task_state(task);      // 4 == COMPLETED
var err   = Fts.fts_last_error_message(task);
Fts.fts_task_release(task);
```

### 12.4 术语

| 术语 | 含义 |
|---|---|
| task_id | 传输任务唯一标识(UUID) |
| wire protocol | fts 端点之间的线协议 |
| control API | 宿主程序调用 fts 的接口 |
| capability bitmap | 能力位图,用于版本协商 |
| chunk | 分块,传输与校验的基本单位 |
| manifest | 文件元数据与分块表 |
| Merkle root | 分块哈希构成的树根,用于整体校验 |
| checkpoint | 断点续传的持久化状态 |

### 12.5 相关文档

| 文档 | 内容 |
|---|---|
| `fts-design.md` v1.1 | 总体架构、性能工程、降级、日志 |
| `fts-t1-relay-design.md` | T1 单中继详细设计 |
| 本文档 | 接口与协议规范(契约) |

---

*文档结束 — 契约冻结版 v1.0*
