# fts — 高速文件传输工具(Windows + QUIC)

GB 级大文件端到端传输:QUIC 多流并行 + 逐块 BLAKE3 校验 + 断点续传 + TCP 自动回退 + inbox 预注册 + 多文件单连接 + 并发会话 + daemon/REST/gRPC/C-DLL 集成。

> **集成方请读 [API.md](API.md)** —— 标准化接口文档(gRPC/REST/CLI/C ABI 全量字段表、错误码、事件模型、典型集成流程)。

## 当前状态(P0–P6 完成,含 W6/W8/W9)

| 能力 | 状态 |
|---|---|
| 单/多文件 send/recv(QUIC,N 流并行,单连接多文件) | ✅ |
| BLAKE3 逐块校验 + 单层 Merkle 根(不可关闭) | ✅ |
| 断点续传(周期 checkpoint,崩溃可恢复,per-file) | ✅ |
| TCP 回退(QUIC 不可达自动降级 / `--transport` 强制) | ✅ |
| 证书固定(TOFU:指纹校验) | ✅ |
| 退出码契约 + `--json` NDJSON 事件 | ✅ |
| daemon + REST API + SSE 事件流 | ✅ |
| **inbox 预注册**(per-task 接收目录,文件/目录两种绑定) | ✅ |
| **接收端并发会话**(`--max-sessions`,QUIC 并发/TCP 互斥) | ✅ |
| **gRPC(tonic)**:Capabilities/ReserveInbox/PurgeInbox/Submit/Observe/Status/Cancel/BeginUpdate | ✅ |
| **UNC/SMB 凭据代建立**(W6,netcred)+ **PurgeInbox** 危险删除(三重防护) | ✅ |
| **文件夹递归传输**(W8:目录整传,以 basename 为根原样重建结构) | ✅ |
| **编排 server**(Python,`orch/`):gRPC 控制两端 + 多任务并行 + 进度条 + 最小 Web UI | ✅ 见 [plan/fts-server-demo.md](plan/fts-server-demo.md) |
| **远程自更新**(W9:push-update + BeginUpdate 自替换/失败回滚) | ✅ 见 [plan/fts-updater-design.md](plan/fts-updater-design.md) |
| C ABI DLL(Python ctypes 已验证) | ✅ |
| Tailscale 链路探测(观测) | ✅ 仅观测 |
| FEC / BBR / Nack 重传 / 多机 relay | ❌ 见 plan/fts-design.md 第 9 节 |

## 构建

```
cargo build --release
# 产物:target\release\fts.exe(CLI/daemon)、target\release\fts.dll(C ABI)
```

## 一键启动(配置文件)

`config.json`(仓库已带默认值,直接改):

```json
{
  "rest_addr": "127.0.0.1:5050",
  "grpc_addr": "127.0.0.1:50051",
  "listen_addr": "0.0.0.0:4433",
  "out_dir": "downloads",
  "cert_cache": ".fts",
  "max_sessions": 3
}
```

```powershell
.\build.ps1                  # 构建 + dist\ 收齐全部运行件(exe/dll/配置/脚本/说明)
# 把 dist\ 整个文件夹拷到目标机即可(无需脚本、不受 PowerShell 执行策略限制):
fts start                    # 后台启动(自动读 config.json,健康检查就绪才返回)
fts status                   # running (pid=..., healthy)
fts stop                     # 停止
fts daemon                   # 前台运行(调试用)
fts daemon -c config.json --rest 127.0.0.1:6000   # CLI 参数覆盖配置文件
```

偏好脚本的用户:`fts-daemon.cmd`(cmd 包装,内含 `-ExecutionPolicy Bypass`,不受执行策略限制)
或 `fts-daemon.ps1`(需 `Set-ExecutionPolicy` 或 -Bypass)。

优先级:**CLI 显式参数 > 配置文件 > 内置默认**。接收目录支持 UNC/SMB 网络路径(凭据要求见 API.md §11.1),daemon 启动与 inbox 注册时自动预检可写性。

## 快速开始

```powershell
# 1. 接收端(打印证书指纹)
.\target\release\fts.exe recv --listen 0.0.0.0:4433 --out .\downloads

# 2. 发送端(TOFU:首次抄下指纹)
.\target\release\fts.exe send .\bigfile.bin --to <对端IP>:4433 --fingerprint <指纹> --streams 4

# 整个文件夹(递归:子目录全部一起传,接收端按原结构重建)
.\target\release\fts.exe send .\myfolder --to <对端IP>:4433 --fingerprint <指纹>

# UDP 被封的环境:自动回退 TCP(或强制 --transport tcp)
.\target\release\fts.exe send .\bigfile.bin --to <对端IP>:4433 --fingerprint <指纹> --transport tcp

# 脚本集成:NDJSON 事件 + 退出码
.\target\release\fts.exe send .\bigfile.bin --to <IP>:4433 --fingerprint <指纹> --json
if ($LASTEXITCODE -ne 0) { throw "传输失败: $LASTEXITCODE" }  # 4 = 校验失败,数据不可信
```

### CLI 一览

```
fts send <PATH>... --to <ADDR> [--fingerprint <HEX> | --insecure] [--streams N]
              [--chunk-size B] [--transport auto|quic|tcp] [--ticket <T>] [--json]
              # 多路径/目录(目录递归展开,子目录结构在接收端原样重建;
              # 相对路径重复 → 退出码 2;空目录不重建;目录符号链接不递归)
fts recv --listen <ADDR> [--out DIR] [--transport auto|quic|tcp] [--cert-cache DIR] [--json]
fts daemon [--rest 127.0.0.1:5050] [--listen 0.0.0.0:4433] [--out DIR]
           [--cert-cache DIR] [--max-sessions 3] [--grpc 127.0.0.1:50051]
fts start|stop|status [-c CONFIG]     # 后台 daemon 生命周期(pid 文件 + REST 健康检查)
fts push-update --to <ADDR> --dist <DIR> [--overwrite-config] [--force]
                                      # 远程推送新版 dist,目标 daemon 自替换重启(W9)
```

退出码:0 成功 / 1 通用 / 2 参数 / 3 连接·版本 / **4 校验失败(数据不可信)** / 5 中断(可续传)/ 6 取消 / 7 认证。

## daemon + REST + gRPC(集成主路径)

```powershell
.\fts-daemon.ps1 start          # 或手动 fts daemon ...(见上文参数)
```

典型集成流程(per-client 目录 + 批量多文件):

```
1. POST /v1/inbox {out_dir: "D:\\fromA"}            → {token}     (目录模式,TTL 10min 内可复用)
2. POST /v1/transfers {paths: [...12 个文件], dst_addr, ticket: token} → {task_id}
3. GET  /v1/transfers/{id}/events                    → SSE 事件流(file/progress/file_done/result)
4. (可选) POST /v1/transfers/{id}/cancel
```

gRPC 等价端点(proto/fts.proto):`Capabilities / ReserveInbox / PurgeInbox / Submit / Observe(stream) / Status / Cancel`,默认 `127.0.0.1:50051`。
Python 端到端示例:`python tests\grpc_smoke.py <指纹>`。

`Capabilities` 自描述(本版新增):返回 `fingerprint`(证书 BLAKE3 hex)与 `listen_addr`(数据监听地址),
使**编排方只凭 gRPC 地址即可全自动编排**,无需人工抄指纹。

### REST 端点

```
GET  /v1/capabilities
POST /v1/inbox {out_dir, file_name?}     → {token, expires_in}   # file_name 空=目录模式
DELETE /v1/inbox/{token}
POST /v1/transfers {paths|src_path, dst_addr, fingerprint, ticket?, ...}
GET  /v1/transfers   |  GET /v1/transfers/{id}
POST /v1/transfers/{id}/cancel
GET  /v1/transfers/{id}/events           → SSE
```

### C ABI(Python 示例,见 tests/ffi_smoke.py)

```python
import ctypes
lib = ctypes.CDLL("fts.dll")
lib.fts_send_file.argtypes = [ctypes.c_char_p]*2 + [ctypes.c_char_p, ctypes.c_int32, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_char_p]
rc = lib.fts_send_file(b"C:\\data\\big.bin", b"100.64.0.5:4433", b"<64hex指纹>", 0, 4, 0, b"auto")
if rc: print(lib.fts_last_error().decode())   # rc: 3 连接 / 4 校验 / 7 认证 …
```

## 编排 server(多机编排 + 最小 Web UI)

`orch/` 是一个独立的 Python 编排服务:输入两端主机与路径,由它经 **gRPC** 分别指挥两端完成传输
(源端 push,server 不搬数据),支持多任务并行、进度条、错误回报与浏览器 UI;任务级 `--purge`
可在传输前清空目标目录(force-clean 部署)。详细案例见
[plan/fts-server-demo.md](plan/fts-server-demo.md),使用说明见 [orch/README.md](orch/README.md)。

```powershell
python -m pip install -r orch\requirements.txt
python orch\gen_proto.py                                        # 生成 gRPC stub
python orch\cli.py --config orch\config.json --check            # 探活两端
python orch\cli.py --config orch\config.json --pairs "A1:B1,A2:B2,A3:B3" `
    --src-path C:\data\payload.bin --dst-path D:\recv           # 并行编排
python orch\cli.py --config orch\config.json --serve            # Web UI: http://127.0.0.1:8080
powershell -ExecutionPolicy Bypass -File orch\run_local_demo.ps1  # 单机 6-daemon 模拟,一键复现
```

跨主机编排时 daemon 需 `--grpc 0.0.0.0:50051` 并放行防火墙;server 只填每台的 gRPC 地址即可。

## 远程更新器(填 IP 一键换版)

几十台机器的版本升级不再手工"杀进程 → 拷文件 → 重启":daemon 支持 W9 远程自更新
(新版文件经 fts 传输推到目标机暂存目录 → 目标 daemon 自动备份/覆盖/重启/失败回滚)。

```powershell
# 命令行
target\release\fts.exe push-update --to 192.168.1.60 --dist dist\

# GUI(Tauri 2 + Svelte 5,开发机构建,填 IP 批量更新)
.\build.ps1 -Updater
updater\src-tauri\target\release\fts-updater.exe
```

首次引导:目标机需部署过一次带 W9 的 dist(旧版无 BeginUpdate RPC),之后全程远程。
默认不覆盖目标机 `config.json`;更新日志在目标机 `.fts\update\apply.log`。设计与踩坑见
[plan/fts-updater-design.md](plan/fts-updater-design.md)。

## 线协议(fts-protocol v1)

```
控制流(双向): Hello ↔ Hello → Manifest(分块哈希表+Merkle根) → ManifestAck(可携带续传位图)
              → [数据流 N 条并行, 分块帧 = id:u32|offset:u64|len:u32|payload]
              → Finish → FinishAck → (接收端等发送端读走 Ack 后,由发送端关连接)
```

契约细节见 [plan/fts-api-spec.md](plan/fts-api-spec.md)(v1.0 冻结);当前实现覆盖 P0–P6 与 W 系列(W6/W8/W9),差异清单见 [plan/dev-log.md](plan/dev-log.md)。

## 安全模型

- 传输:QUIC = TLS 1.3(逐包加密+完整性)+ 逐块 BLAKE3(端到端)。
- 身份:自签证书 + 指纹固定(TOFU)。`--insecure` 只应出现在测试环境。
- REST 默认仅监听 localhost;控制面(REST/gRPC)**未实现远程鉴权**。跨主机编排需将 gRPC 绑 `0.0.0.0`
  并用防火墙/ACL 限制访问 —— **仅限可信网络(LAN/Tailscale)**。
- `Capabilities` 会返回证书指纹(自描述所需);这不影响传输安全模型(数据面仍逐块 BLAKE3 + 指纹 pin)。

## 已知限制(诚实清单)

- 回环基准 ≠ 真实网络:1GB 回环 QUIC ~340 MB/s、TCP ~1900 MB/s(回环无 RTT/丢包);**两台物理机/跨国数字待测**。
- 块损坏即整体失败(无 Nack 级重传);FEC/BBR 未实现(设计见 plan/fts-design.md 第 9 节)。
- daemon 的 REST/gRPC 无鉴权,取消接口任何本地进程可调。
- Tailscale 探测仅观测(打 `path` 事件),未接入降级决策。
- TCP 回退会话全局互斥(数据流与新会话共用 listener,无法区分);QUIC 会话可并发(默认 3)。
- 多文件任一文件失败即中止整个任务(无 --keep-going)。

## 目录

```
src\          main(CLI) lib(入口) send recv protocol tls ui error probe daemon grpc inbox netcred ffi update
include\      fts.h(C 头文件)
proto\        fts.proto(gRPC 契约)
orch\         编排 server(Python):gRPC 控制两端 + Web UI + 单机演示脚本
updater\      远程更新器 GUI(Tauri 2 + Svelte 5):填 IP 推送新版 dist 并自替换重启
tests\        ffi_smoke.py(ctypes)、grpc_smoke.py(gRPC 端到端)、update_e2e.py(远程更新端到端)
plan\         设计文档、API 契约、开发日志、编排 server 案例(AGENTS.md 纪律)
```

开发过程(问题→根因→解决)全部记录在 [plan/dev-log.md](plan/dev-log.md)。
