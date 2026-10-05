# fts 编排 server(orch)

一个独立的 Python 编排服务:输入两端主机和路径,由它通过 **gRPC** 分别指挥两端完成传输,
带进度条、错误回报和最小化 Web UI。多任务可并行(如 A1→B1 / A2→B2 / A3→B3)。

> 完整案例(背景、部署、真多机实操、排查手册)见 [../plan/fts-server-demo.md](../plan/fts-server-demo.md)。

## 它是怎么工作的(push 模型)

fts 是**纯 push** 模型:数据连接永远由**源端**发起去连**目标端**,没有任何"命令接收端去拉取"的 RPC。
所以 server 不搬数据,它是两端的 gRPC 客户端:

```
Capabilities(A)  探活 + 读 A 的身份(指纹/监听端口)
Capabilities(B)  探活 + 读 B 的身份
ReserveInbox(B)  在 B 上预注册落盘目录 → ticket
Submit(A)        paths=源路径, dst_addr=B:数据端口, fingerprint=B指纹, ticket → task_id
Observe(A)       订阅 NDJSON 事件流 → 进度 / 结果
                 (之后 A 自己直连 B 传数据,字节不经过 server)
```

前置条件:daemon 的 `Capabilities` 需返回指纹与监听地址(见 `proto/fts.proto` 的
`CapabilitiesReply.fingerprint/listen_addr`),这样 server 只凭 gRPC 地址即可全自动编排,
无需人工抄指纹。

## 安装

```powershell
python -m pip install -r requirements.txt
python gen_proto.py            # 从 ../proto/fts.proto 生成 fts_pb2*.py
```

要求 Python 3.10+;daemon 侧需用支持自描述的构建(`cargo build --release`)。

## 配置

复制 `config.example.json` 为 `config.json`。每台主机只需 `name` + `grpc_addr`,
指纹与数据端口由 `Capabilities` 自动发现:

```json
{
  "timeout_sec": 3,
  "defaults": { "transport": "auto", "streams": 0, "chunk_size": 0 },
  "endpoints": [
    { "name": "A1", "grpc_addr": "10.0.0.11:50051" },
    { "name": "B1", "grpc_addr": "10.0.0.21:50051" }
  ],
  "tasks": [
    { "name": "A1->B1", "src": "A1", "src_path": "C:\\data\\payload.bin",
      "dst": "B1", "dst_path": "D:\\recv" },
    { "name": "folder", "src": "A1", "src_path": "C:\\data\\srcdir",
      "dst": "B1", "dst_path": "D:\\recv", "purge": true }
  ]
}
```

## 用法

```powershell
# 1) 探活:确认两端存在且活着(控制面 gRPC + 数据面 TCP 双重)
python cli.py --config config.json --check

# 2) 并行编排三个任务
python cli.py --config config.json --pairs "A1:B1,A2:B2,A3:B3" `
    --src-path C:\data\payload.bin --dst-path D:\recv

# 2b) 传整个文件夹(递归,保留相对路径结构)+ 传输前清空目标目录(危险,不可恢复)
python cli.py --config config.json --pairs "A1:B1" `
    --src-path D:\data\srcdir --dst-path D:\recv --purge
# 目录按相对路径落盘:D:\recv\srcdir\...(子目录结构保留;空目录不传)

# 3) 运行 config.json 里的 tasks(每任务可有不同目标目录)
python cli.py --config config.json --run-config-tasks

# 4) Web UI(浏览器操作,带实时进度条)
python cli.py --config config.json --serve --host 127.0.0.1 --port 8080
```

退出码:0 = 全部 COMPLETED 且校验通过;1 = 有任务失败;2 = 参数/配置错误。

## 单机复现(无需多台主机)

```powershell
powershell -ExecutionPolicy Bypass -File run_local_demo.ps1
```

起 6 个 daemon(各独立端口/证书目录 → 6 个不同指纹),生成测试文件,并行跑
A1→B1 / A2→B2 / A3→B3,校验落盘文件大小,然后清理。

## server 自身的 API(供 Web UI / 脚本)

| 方法/路径 | 作用 |
|---|---|
| `GET /` | 静态页 |
| `GET /api/hosts` | 主机列表(含探活结果) |
| `POST /api/hosts/check` | 重新探活 |
| `GET /api/tasks` | 任务列表 |
| `POST /api/tasks` | 新建并启动任务 |
| `POST /api/tasks/{id}/cancel` | 取消 |
| `GET /api/events` | SSE 增量(任务/主机) |

## 文件

```
core.py              编排引擎(探活/ReserveInbox/Submit/Observe/取消/事件总线)
cli.py               命令行入口(--check / --pairs / --run-config-tasks / --serve)
app.py               FastAPI:REST + SSE(Web UI 后端)
static/index.html    Web UI(原生 JS + EventSource)
gen_proto.py         生成 gRPC stub
config.example.json  配置样例
run_local_demo.ps1   单机 6-daemon 并行演示
```
