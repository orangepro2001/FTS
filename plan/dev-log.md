# fts 开发日志与进度记录

> **项目**:高速文件传输工具 `fts`(File Transfer System)
> **本文件职责**:按 AGENTS.md 要求,持续记录**开发思路、文件结构、进度、遇到的问题、解决方案**。
> **更新纪律**:每完成一个阶段/每解决一个问题,立即更新本文件。最新记录在最上方。

---

## 文档地图(先读哪个)

| 文件 | 内容 | 何时读 |
|---|---|---|
| `plan/fts-design.md` v1.1 | 总体设计:架构、Windows 平台约束、性能工程、降级阶梯 L0~L6、日志方案、路线图 P0~P4 | 了解全貌 |
| `plan/fts-t1-relay-design.md` v1.0 | T1 单中继设计:Tailscale Peer Relay(T1.0 首选)/ 自建中继(T1.1 备选)、探测、选路 | P2 做降级时 |
| `plan/fts-api-spec.md` v1.0 | **契约冻结**:五种接口(CLI/Rust 库/C ABI/命名管道/gRPC)、线协议 v1、错误码、事件模型 | 写任何对外接口前 |
| `plan/dev-log.md`(本文件) | 开发过程记录:进度、坑、决策 | 接手开发时 |
| `plan/fts-server-demo.md` v1.0 | 编排 server 案例:gRPC 控制两端、多任务并行、Web UI、真多机部署、排查手册 | 做编排/多机 demo 时 |
| `plan/fts-updater-design.md` v1.0 | W9 远程更新器:BeginUpdate 自替换、push-update、GUI、更新确认机制 | 做远程更新/部署时 |
| `../skills/ponytail.md` | Ponytail 开发纪律:最简可行方案,YAGNI | 每次写代码前 |

### 关键决策回顾(为什么是现在这个样子)

1. **QUIC 作为默认传输层**(quinn):四条硬约束(不许裸 UDP、校验不可关、要加密、跨国降级)共同指向 QUIC —— 自带 TLS 1.3、逐包完整性、可靠传输、多流。不自研裸 UDP 协议。
2. **Windows 唯一平台**:Linux 快路径(io_uring/sendfile/splice)全部不可用;tokio 在 Windows 上走 IOCP。性能上限低于 Linux 实现,P0 的意义就是把**真实数字**跑出来。
3. **Tailscale 负责组网**:NAT 穿透/中继交给 Tailscale,fts 只连 `100.64.0.0/10` 地址并探测链路类型(直连/DERP/Peer Relay)。不自建 P2P。
4. **校验不可关闭**:BLAKE3 分块哈希 + 文件级哈希,编码进类型系统(校验枚举无 `None` 变体)。
5. **Ponytail 纪律**:P0 不建 workspace、不拆 crate,单二进制 `src/main.rs` 跑通最小闭环;P1 有了真实需求再按 `fts-design.md` 第 8 节拆 crate。

---

## 2026-10-06(5):W9 远程更新器 —— BeginUpdate 自替换 + push-update + Tauri GUI(填 IP 一键换版)

设计文档:**plan/fts-updater-design.md**(架构图、决策表、踩坑明细都在那里,此处只记要点)。

### 完成内容

- proto:`BeginUpdate(UpdateRequest) -> Ack` + `CapabilitiesReply.started_at`(tag 6,进程启动 unix 秒)。
- `src/update.rs`(新模块):
  - `apply_update`:等旧进程退出(超时 taskkill 兜底)→ 备份→覆盖 → 拉起**安装目录**的 `fts start` → 失败回滚 → result.json/apply.log。
  - `push_update`:Capabilities → ReserveInbox(暂存)→ fts 传输推送 → BeginUpdate → 轮询确认,GUI/CLI 共用。
- CLI:`fts push-update --to <host[:50051]> --dist <dir>`(运维可用);隐藏 `apply-update` 子命令。
- GUI:`updater/`(Tauri 2 + Svelte 5):主机列表持久化、检查/更新单个与批量、实时日志面板;构建 `build.ps1 -Updater`。
- e2e:`tests/update_e2e.py` 全链路 PASS(真实传输 + 自替换 + 重启确认);单测 26 全过;orch stub 已重新生成。

### 遇到的问题与解决方案(全部实测踩到)

1. **`output()` 永久卡死**(重要):apply-update 用 `Command::output()` 捕获 `fts start` 输出会死锁 ——
   daemon 孙进程经 `bInheritHandles=TRUE` 继承了管道写句柄,EOF 永不到来。修法:stdio 直写 apply.log +
   `status()`。Python 侧同理:`subprocess` 起 `fts start` 不能 `capture_output`(管道句柄同样被 daemon 继承)。
   推论:**fts start 之后的进程树不适合被任何"等 EOF"的调用捕获输出**。
2. **本地回环更新全程 <2s,靠连接空窗判断重启是竞态**(重要):1s 轮询完全可能错过整个"退出→换文件→
   重启"窗口,客户端误报失败(目标端其实成功)。修法:CapabilitiesReply 加 `started_at`,重启判定无竞态。
3. **ctl_start 的 current_exe 陷阱**:apply-update 运行的是暂存副本,直接调 ctl_start 会把暂存区副本启动
   成 daemon(暂存目录被清理就连坐)。修法:apply-update spawn 安装目录的 `fts.exe start`。
4. **`fts start` 的 config 只有 `-c`**:apply-update 传 `--config` 触发参数错误 → 回滚路径同样失败 →
   daemon 没起来。修法:传 `-c`。
5. **图标 RC2176 "old DIB"**:手写 ICO 只给了像素数据,Windows 资源编译器要求每项以 BITMAPINFOHEADER
   (biHeight = 2×高)开头。`updater/tools/make_icon.py` 已修。
6. **PowerShell `-replace` 管道改写源码再次毁编码**(第 2 次踩同款坑,见 2026-10-05 问题 8):GBK/UTF-8
   往返把 update.rs 全文毁掉,整文件重写。结论不变:**源码只能用编辑器工具(Read/Write/Edit)改**。

### 遗留

- 回滚结果(result.json)只在目标机本地可见,GUI 拿不到精确回滚信息(靠版本判定 + apply.log 人工排查)。
- 首次引导:现网旧 daemon 无 BeginUpdate,需手工部署一次 W9 版 dist(之后才可远程更新)。
- gRPC 无鉴权,BeginUpdate = 远程换二进制,仅限可信内网(与现状同级风险,文档已标注)。

## 2026-10-06(4):编排 server 增强 —— 文件夹 + purge(force 清空目标)+ 接收端 Finish 竞态修复(重要)

### 功能:传输前强制清空目标目录(purge)

需求:force-clean 部署 —— 传整个文件夹前把目标目录清干净。复用 W6 `PurgeInbox`(三重防护:仅目录
模式 token / 显式 force=true / 只清目录内部),编排流程变为:
`ReserveInbox → PurgeInbox(token, force=true) → Submit → Observe`。任务级选项 `purge`(Web UI 勾选 /
CLI `--purge` / config `"purge": true`),删除统计进任务 warnings 并在 UI 展示。

文件夹传输零新增:编排器把目录路径直接当 `src_path` 透传,W8 的递归展开(以 basename 为根、相对路径
结构落盘,如 `srcdir/sub/f.txt` → `<dst>\srcdir\sub\f.txt`;空目录不传;符号链接跳过)原生生效。

### 核心 bug 修复:接收端 Finish 与 accept 数据流的竞态(实测触发)

**现象**:编排 server 跑多文件任务,6 字节的 a.txt 偶发失败,发送端报 `IO_ERROR: early eof`,接收端
报 `校验失败: Finish 后仍有 1 块未收到`(B1 daemon 日志实锤)。4 MiB 文件从不复现。

**根因**:`handle_one_file` 的收流循环里,`select!` 在 `data_src.next_stream()`(accept 新流)与
`finish_rx.recv()`(Finish)之间随机选择。发送端虽然**先**开满 N 条流、写完数据并 shutdown、**然后**
才发 Finish,但接收端若在队列里还有流未 accept 时先命中 Finish,就直接 break 去 join —— join 只等
**已 spawn** 的流任务,队列里未 accept 的流被遗弃,位图缺块。小文件整个传输在微秒级完成,Finish 与
流开启几乎同时到达,竞态窗口最大;大文件发送端哈希耗时,接收端总能先 accept 完。

**修复**(recv.rs):收到 Finish 后、join 之前,用 `timeout(Duration::ZERO, next_stream())` 循环把
accept 队列中**立即就绪**的流补收干净再 join。零延迟、无新依赖;真正在途的流本就等不到,位图校验兜底。

**教训**:与 P1 帧撕裂同族但不同型 —— 这次不是取消不安全,而是**状态机收到终态信号时必须先清空在途项**。
QUIC 跨流无顺序保证,任何"Finish 到了就当数据都到了"的假设都不成立,哪怕发送端确实先写完数据。

### 观测噪声(已文档化,未改)

编排器探活对数据端口做 TCP 探测,daemon 会记一条「接收会话失败: early eof」—— 探测连接被当成会话
接受后立即 EOF 所致,无害。已在 `fts-server-demo.md` §7 排查手册注明,避免误判。

### 验证(实测)

| 用例 | 结果 |
|---|---|
| 文件夹(3 文件 + 1 子目录)+ purge,单路 ×3 轮 | 全 PASS;purge 统计逐轮精确(2文件1目录 → 0文件1目录)✅ |
| 3 路并行文件夹 + purge(B1/B2/B3 各预埋残留物) | 全 PASS;`srcdir` 子树(含 sub\nested.txt)完整落盘,残留物精确清除 ✅ |
| 7 字节小文件 ×10 连传(竞态回归) | 10/10 PASS ✅ |

---

## 2026-10-06(3):W8 —— 文件夹递归传输

集成方需求:传整个文件夹 = 递归,子文件夹全部一起传。P5 当时是"仅直接文件"(规划明确简化),本次升级。

### 实现

| 侧 | 变更 |
|---|---|
| 发送端 | `expand_paths` 重写为递归 `walk_dir`:目录参数以其 **basename 为相对前缀**整棵展开(`fts send tree` → `tree/a.bin`、`tree/sub/deeper/c.bin`);协议内相对路径统一 `/` 分隔;重复判定从 basename 改为**相对路径**;目录符号链接不递归(防环,`DirEntry::file_type` 不跟随);空目录不产生条目(不重建,文档写明) |
| 接收端 | `protocol::sanitize_rel_path`(信任边界):manifest.file_name 现为相对路径,拒绝 `..`/`.`/空组件/反斜杠/盘符/绝对形态(**防路径穿越**);按相对路径 `create_dir_all` 重建父目录,checkpoint/tmp/rename 全部派生自校验后的路径 |
| 死锁修复(重要) | **零字节文件 / 续传时已完整文件**会导致发送端跳过 Finish→FinishAck 握手,接收端永远卡在 `finish_rx.recv()` —— 递归场景必踩(空文件常见)。修复:todo=0 时仍走完整握手,仅不发数据块(空流 + Finish,接收端 bitmap 空 = complete → 落盘 → Ack) |

### 验证

| 用例 | 结果 |
|---|---|
| 三层树(tree/a.bin、sub/b.bin、sub/deeper/c.bin、zero.bin 0 字节)一次传输 | ✅ 结构原样重建(tree /f 对比),4 文件校验全过,zero.bin 0 字节落盘,退出码 0 |
| 递归续传:树+512MB 传输中杀发送端 → 重发 | ✅ a.bin/b.bin/c.bin/zero 秒过,big2.bin 只补缺失块,SHA 一致 |
| 单测:rel_path_sanitizing(合法 2 例 + 穿越/逃逸 11 例全拒)、递归展开(经 e2e 覆盖) | ✅ 22/22 |

### 已知边界

- 空目录不重建(fs 语义里空目录无内容可校验;需要空目录语义时再加占位文件协议)。
- 目录符号链接不递归(防环);文件符号链接按普通文件处理(复制目标内容)。
- 接收端大文件句柄在 daemon 存活期间可能被占用(Windows 文件共享语义),校验/杀毒软件读取时同理 —— 传输完成即 rename,句柄随会话关闭释放。

---

## 2026-10-06(2):部署包修正 —— exe 原生 start/stop/status + dist 全量

集成方实测(截图):目标机 PowerShell 执行策略挡掉 `fts-daemon.ps1`(PSSecurityException);用户直觉尝试 `fts.exe start` 失败(无此子命令)。

### 解决(两层)

1. **exe 原生控制子命令**(主方案,用户直觉就是对的):
   - `fts start`:spawn 自身(daemon,DETACHED_PROCESS|CREATE_NO_WINDOW)+ pid 文件(.fts\daemon.pid,与 ps1 共用)+ TCP 健康检查(10s 内 rest 端口可连才返回成功,失败中止并清理 pid);自动采用 cwd 的 config.json(与 ps1 行为一致);启动前预检接收目录(坏目录 spawn 前暴露)。
   - `fts stop`:pid 文件 → taskkill /F;残留 pid 清理。
   - `fts status`:pid 存活(OpenProcess)+ REST 端口连通 → 四态输出(running healthy / running pid 未知 / degraded / stopped)。
   - 无需任何脚本,不受执行策略限制。
2. **cmd 包装**:`fts-daemon.cmd` = `powershell -NoProfile -ExecutionPolicy Bypass -File fts-daemon.ps1 %*` —— 逐进程绕过执行策略,无需管理员。

### dist 全量(build.ps1)

dist\ 现在含:fts.exe、fts.dll、fts.h、config.json、fts-daemon.ps1、fts-daemon.cmd、DEPLOY.txt —— **整个文件夹拷走即用**。DEPLOY.txt 面向部署机使用者(启动/发文件/常见问题,含执行策略问题的答案)。

### 验证与坑

- dist 拷到 C:\Temp 模拟部署:`fts start/status/stop` 三态全过(capabilities HTTP 200、stop 后 stopped、防重入由 pid 存活检查覆盖)。
- **.cmd 包装在本沙箱无法端到端验证**:沙箱对 cmd→powershell 嵌套链挂起/回显(嵌套 echo 都被原样返回)—— 测试环境限制。等价性已证:.cmd 唯一实质命令 `powershell -NoProfile -ExecutionPolicy Bypass -File fts-daemon.ps1 status` 在沙箱直接运行返回 "stopped"。真实 cmd 窗口预期可用,待集成方反馈。
- 单测 21/21;工作区出现外部产物(orch\ 编排 server、.zcode)—— 未触碰。
- TODO 残留:bin/lib 同名 pdb 冲突 warning(bin 改名 fts-cli 可除)。

---

## 2026-10-06:编排 server —— gRPC 控制两端 + 多任务并行 + 最小 Web UI

实际场景 demo:输入两端 IP:端口与路径,server 经 gRPC 分别指挥两端完成传输,带进度条/错误回报/
最小 Web UI,并并行跑 A1→B1 / A2→B2 / A3→B3。详细案例见 `plan/fts-server-demo.md`。

### 核心认知:fts 是纯 push,server 只能是"两端的 gRPC 客户端"

没有任何"命令接收端去拉取"的 RPC,所以 server 不搬数据,映射固定为:
`Capabilities(A/B)` 探活 → `ReserveInbox(B)` 拿 ticket → `Submit(A, dst_addr=B数据口, fingerprint=B指纹, ticket)`
→ `Observe(A)` 读进度/结果;之后 **A 自己直连 B**。这条决定了全部实现。

### 原型扩展(唯一改 Rust 的地方)

`CapabilitiesReply` 加 `fingerprint`(tag 4)+ `listen_addr`(tag 5) —— 只追加字段,proto3 向后兼容。
`daemon.rs` 用 `tls::load_or_generate_cert(cert_cache)` 取证书算 BLAKE3 指纹(与数据监听同一张证书,
即发送端 pin 所需值),存入共享 `Registry`,`grpc.rs`/REST 的 capabilities 一并返回。收益:server
只凭 gRPC 地址即可全自动编排,免人工抄指纹。

### 实现(orch/,Python)

`core.py`(编排引擎:探活/ReserveInbox/Submit/Observe/取消/事件总线)+ `cli.py`(--check/--pairs/
--run-config-tasks/--serve)+ `app.py`(FastAPI REST+SSE)+ `static/index.html`(原生 JS:EventSource 进度条)。
选 FastAPI+grpc.aio 是因为 Observe 是流式、SSE 是长连接,asyncio 下最自然。

### 两个必须处理的时序陷阱(否则卡死/误判)

1. **极快传输时订阅晚于 result**:Observe 的 broadcast 是环形缓冲,晚订阅者丢早期事件;若传输在订阅前
   就完成,**流不会自动关闭 → 会永久挂住**。解决:Observe 读到的事件走独立队列,主循环 `wait_for(timeout=2s)`
   超时就拉一次 `Status`,发现终态即断;并且**每次收尾必拉一次 Status** 补齐 `error_message`/兜底终态。
2. **Cancel 不回发 result**:daemon 的 `Cancel` 只 abort 发送任务,不推 result 事件 —— server 收到 `ok=true`
   后自行置 CANCELLED(error_code 6),不能等事件。

### Python 3.14 环境踩坑

- `uvicorn.run()` 内部自建事件循环,若在 `asyncio.run()` 里调用 → `RuntimeError: asyncio.run() cannot be
  called from a running event loop`。修:`--serve` 在 `asyncio.run` **之外**直接调用,不放进协程入口。
- grpcio-tools 生成的 `fts_pb2_grpc.py` 用绝对 `import fts_pb2`,所以必须从 `orch/` 目录运行或把 `orch/` 加进 path。

### 验证(实测)

| 用例 | 结果 |
|---|---|
| 单机 6-daemon 并行 A1→B1/A2→B2/A3→B3(64 MiB × 3) | 三条全 COMPLETED + checksum_ok,落盘 67108864 B,~0.34 s ✅ |
| 探活(含伪造死端点) | 死端点标 XX 且 `--check` 退出码非 0 ✅ |
| 错误回报(源路径不存在) | FAILED,`IO_ERROR(1)` + 精确 message,退出码 1 ✅ |
| Web UI(浏览器实测) | 主机表/指纹自动发现正确;表单提交 → SSE 实时进度 → COMPLETED ✅ |

### 已知边界

控制面无鉴权 + 需绑 `0.0.0.0` 才能跨主机 → 仅限可信网络(文档已声明);gRPC 的 `ReserveInbox` 无目录
预检(REST 有),坏目标路径会在传输阶段以 IO 错误暴露;一份任务单源路径(引擎按 paths 列表传,易扩多文件)。

---

## 2026-10-06:W6 —— gRPC 凭据代建立 + PurgeInbox 危险删除指令

集成方需求:①SMB 目标凭据经 gRPC 传输,fts 代建会话;②gRPC 可下发删除指令,force 级干净复制 —— 危险控制由集成端做,fts 负责**准确删除正确的文件夹**。

### 实现

| 项 | 内容 |
|---|---|
| 凭据代建立 | `ReserveInbox` 加 `credential{user_name, password, domain}`;`netcred.rs` 调 `WNetAddConnection2W`(CONNECT_TEMPORARY 会话级,不写 profile);仅 UNC(`\\host\share` 根提取);**密码不进任何日志/错误/事件**(`describe()` 脱敏);REST body 同步支持 |
| PurgeInbox | 新 RPC + `POST /v1/inbox/{token}/purge`;清空 token 绑定目录**直接内容**(子目录整删/文件删,目录本身保留);返回删除统计;每次操作 `[warn] PURGE` 审计日志 |
| 授权边界 | `InboxRegistry::peek_purgeable`:**仅目录模式 token 可 purge**(文件绑定授权范围只是单文件,越权 → 403/PERMISSION_DENIED);token 无效/过期 → 404/NOT_FOUND;**必须显式 force=true**(缺省 → 400/INVALID_ARGUMENT) |

### 危险操作的边界设计(核心决策)

"删除正确的文件夹"由三重机制保证,而不是靠调用方小心:
1. **删除范围由 token 授权决定**,不接受裸路径 —— 能注册 out_dir 的进程才能删该目录,授权与破坏范围严格对齐;
2. **文件绑定 token 禁止 purge** —— 它的授权只是一个文件,不许触发整目录删除;
3. **force 显式确认** —— 语义上强迫调用方意识到破坏性。

标准 force 部署流程(API.md 附录 A):ReserveInbox → PurgeInbox(force=true)→ Submit → Observe。

### 验证(T10 PASS)

| 用例 | 结果 |
|---|---|
| REST purge:force=false → 400 提示;force=true → 统计(2 文件 1 目录);目录本身保留 | ✅ |
| 文件绑定 token purge → 403 + 精确原因 | ✅ |
| gRPC 五断言(Python):本地路径+凭据 → INVALID_ARG 且密码不泄漏;force=false → INVALID_ARG;文件绑定 → PERMISSION_DENIED;正常 purge 统计;陌生 token → NOT_FOUND | ✅ |
| 单测:UNC 根提取、describe 脱敏、establish 拒本地路径(密码不泄漏)、peek_purgeable 三态 | ✅ 共 21/21 |

### 已知边界(诚实)

- WNetAddConnection2 对**真实需凭据的 SMB 服务器**未实测(本机管理共享隐式有凭据);1219/1326 错误映射按 OS 文档写,待集成方真实环境反馈。
- purge 原子性:逐项删除,中途失败即返回(已删部分不回滚)—— 目录清理语义可接受;集成端可重试(幂等)。
- purge 不处理符号链接逃逸以外的情况:remove_dir_all 不跟随目录 symlink(Rust 标准库加固);文件 symlink 按普通文件删链接本身。

---

## 2026-10-05(末4):配置文件 + SMB/UNC 网络路径支持

### 配置文件(P6,按集成方"一键启动"需求)

- `fts daemon -c config.json`:全部 daemon 参数 JSON 化(`rest_addr/grpc_addr/listen_addr/out_dir/cert_cache/max_sessions`),全字段可选。
- 合并优先级:**CLI 显式参数 > 配置文件 > 内置默认**(CLI 参数全部改为 Option,merge 后回填)。
- **BOM 容错**:PowerShell 5.1 / 记事本写 JSON 带 UTF-8 BOM,serde_json 拒绝 —— 加载前剥 BOM(Windows 编码坑第 5 次)。
- `fts-daemon.ps1`:config.json 存在时自动 `--config` 启动(REST 健康检查端口同步从配置读);`-Config ""` 强制 CLI 参数模式。
- 启动即预检接收目录可写(临时文件探测),坏配置/坏目录在启动时暴露而非传输中途。

### SMB/UNC 网络路径(集成方"目标需要身份认证"问题)

**结论:能处理,凭据由 Windows 会话负责,fts 不弹凭据框**(CLI/服务程序无 UI,弹框是 Explorer 行为)。

| 场景 | 行为 |
|---|---|
| 凭据已建立(cmdkey/net use/域自动) | UNC 路径直接作为 `--out` / inbox `out_dir`,标准文件 API 透明工作。**实测**:`\\localhost\c$\Temp\fts-unc` 接收 30MB,SHA 与源一致,284 MB/s |
| 凭据缺失 | 传输/启动失败,错误自动翻译为人话 + 解决命令(OS 1326 → 提示 `cmdkey /add:<主机> /user:<用户> /pass:<密码>`;5 → 权限;53/67 → 路径;1219 → `net use * /delete`) |
| 预检 | daemon 启动 + inbox 注册两处目录可写探测(写临时文件),凭据问题在提交任务前暴露 |

安全立场:fts **不接管密码管理**(明文凭据进 JSON 是反面),凭据走 Windows 凭据管理器(cmdkey)是标准做法 —— API.md §11.1 已写明。

### 验证

- config 启动(端口 5060)✅;CLI 覆盖(5070)✅;坏 JSON 优雅退出 1 ✅;BOM JSON 正常加载 ✅
- UNC 传输 e2e ✅;io_with_hint 单测 ✅;全单测 16/16 ✅
- 文档:API.md §11.1(网络路径)、README"一键启动"节

---

## 2026-10-05(末3):标准化接口文档

- 新增 **`API.md`**(根目录):面向集成方的实际交付接口文档 —— 通用约定、任务/事件模型、inbox 语义、gRPC 六 RPC 与 REST 八端点逐个字段表、CLI/C ABI、错误码总表(跨接口统一)、安全模型、兼容性承诺、典型集成流程与已知限制。
- 与 `plan/fts-api-spec.md`(设计契约,冻结于实现前)分工明确:API.md 描述**现状**,冲突时以 API.md 为准;README 指向它。
- 内容核对方式:从 daemon.rs 路由表与 proto/fts.proto 实际定义反查,非凭记忆。

---

## 2026-10-05(末2):P5 集成能力完成 ✅

按 `plan/fts-p5-integration-design.md` 执行,四个工作项全部落地,T1~T9 验收全过。

### 交付明细

| 项 | 内容 |
|---|---|
| W1 inbox 预注册 | `POST /v1/inbox {out_dir, file_name?}` → 一次性/目录 token(TTL 10min);Manifest 新增 `ticket` 字段(serde default 向后兼容);接收端查表定落盘目录;文件绑定模式防挪用,**错误使用也焚毁 token(防枚举)**;gRPC `ReserveInbox` 同语义 |
| W2 多文件 | 发送端 `<PATH>...` 多路径/目录展开(不递归)+ 重名检测(退出码 2,`FtsError::InvalidArg`);**单连接逐文件** Manifest→chunks→Finish→FinishAck;事件新增 `file`/`file_done`;**接收端 session_logic 改多文件循环**;REST submit 支持 `paths` 数组 |
| W3 并发会话 | `recv.rs` 拆 `bind/accept_one/handle_incoming/serve`;daemon `--max-sessions`(默认 3,Semaphore);**TCP 会话全局互斥**(数据流与新会话共用 listener,OwnedMutexGuard 锁会话期)—— 刻意简化,文档写明 |
| W4 gRPC | tonic 0.13 + prost 0.13 + **protox(纯 Rust protoc 替代,零系统依赖)**;proto/fts.proto 六个 RPC;daemon `--grpc 127.0.0.1:50051`;与 REST 共享任务表;`tests/grpc_smoke.py` Python grpcio 端到端 PASS |
| 脚本 | `build.ps1`(构建+测试+dist 汇总)、`fts-daemon.ps1`(start/stop/restart/status,PID 文件+REST 健康检查) |

### W2 关键设计修正(执行中发现)

1. **目录模式 token 必须可复用**:初版 token 一次性,批量 12 文件第二个就被拒。改为:文件绑定模式一次性、目录模式 TTL 内可复用(集成主路径 = 一个目录 token 传一批)。
2. **接收端必须也多文件循环**:初版发送端循环、接收端单文件 —— f1 完成后 ctl_reader abort,**abort drop 掉任务内 RecvStream → STOP_SENDING 打断 f2 控制流**(与"read_exact 取消不安全"同族问题的新形态)。重构:常驻 ctl_reader 按帧类型分发(Manifest/Finish 双通道),接收端循环逐文件处理。
3. **错误路径的 gate**:ticket 拒绝后立即 `conn.close(1)` 会把刚发的 deny Ack 一起丢掉 → 发送端只见 connection lost,退出码 1 而非 7。修:错误路径 close 前等对端 2s(读走最后一条消息)。同族问题第三次出现 —— **规则:QUIC 上任何"我刚说完话就挂电话"都是 bug**。

### 构建链备忘(tonic + protox)

- tonic 0.14 大改架构(prost 功能拆到 tonic-prost-build),**锁 0.13**(API:configure/skip_protoc_run/file_descriptor_set_path)。
- protox 0.9 内部 prost 0.14 与 tonic-build 0.13 链的 prost 0.13 类型不兼容 → build-dep 双版本共存(0.13 给 tonic-build,0.14 给 protox 编码),用 `protox::prost::Message::encode_to_vec` 显式选择;protobuf 线格式版本无关, descriptor bin 兼容。
- `compile_protos(&[...], &[])` 的 `AsRef<Path>` 二义(tempfile 引入第 N 个实现)—— 数组显式标注类型。
- Windows PowerShell 5.1:`$pid` 是只读自动变量;`> nul` 在 PS 里不是设备(用 `> $null` / cmd 才是 nul)。

### 验收结果(T1~T9 全过)

| 用例 | 结果 |
|---|---|
| T1 inbox 目录模式 + 5 文件(3 多路径 + 目录展开) | ✅ 全落指定目录,退出码 0 |
| T2 文件绑定:错名拒绝(退出码 7,token 焚毁)/ 对名成功(0) | ✅ |
| T4 无 ticket 回归 → 默认目录 | ✅ |
| T6 多文件中断:f4 完整落盘、f5 半成品 checkpoint → 重发 f5 只补 1 块 | ✅ |
| T7 重名检测 | ✅ 退出码 2 + 明确错误消息 |
| T8 双发送端并发(不同目录 token) | ✅ 双 0,各自落盘 |
| T9 gRPC:Capabilities→ReserveInbox→Submit(2×200MB)→Observe(progress+file_done+result)→Status | ✅ PASS |
| 单元测试 | ✅ 15/15(inbox 5 例) |

### 遗留(诚实)

- fts-daemon.ps1 未加 `-Grpc` 参数(daemon 默认 50051 已开;改端口需手动命令行)。
- REST/gRPC 仍无鉴权(README 已声明);inbox token 是唯一授权面。
- `--keep-going`(部分失败继续)未做(规划明确不做)。
- Observe 流:任务结束后事件 channel 关闭,迟订阅者只收 result 前的缓存事件(broadcast 容量 256,lagged 标记)。

---

## 2026-10-05(末):集成确认答疑 + 运维脚本

### 新增脚本

| 脚本 | 用途 |
|---|---|
| `build.ps1`(`-Test` 跑单测) | 一键 release 构建 + 产物汇总到 `dist\`(fts.exe / fts.dll / fts.h) |
| `fts-daemon.ps1` `start\|stop\|restart\|status` | daemon 生命周期:PID 文件(`.fts\daemon.pid`)+ REST 健康检查轮询;防重入;restart 一键切换 |

脚本全英文输出 —— PowerShell 5.1 无 BOM UTF-8 会按 ANSI 解析,中文注释/字符串有乱码风险(编码教训第 3 次)。
坑:`$pid` 是 PowerShell 只读自动变量,脚本内赋值直接报错 —— 已改用 `$daemonPid`。

### 集成方(Q&A)确认结论

1. **接收目录当前是启动参数固定**(`recv --out` / `daemon --out`),运行中不可变。gRPC/REST 集成若需 per-task 目录,方案已定:**inbox 预注册** —— 调用方先 `POST /v1/inbox {file_name, out_dir}` 拿一次性 token → 发送端携带 → 接收端按 token 落盘到对应目录。未实现,列为下一步。
2. **daemon 退出**:当前前台 Ctrl+C,或 `fts-daemon.ps1 stop`;重启 `restart`。Windows 服务化(NSSM/sc)未做。
3. **并发调度结论**(A/B/C 同时向 Z 传):当前实现实际是**顺序队列**(daemon 一次只 accept 一个会话,后来者在 QUIC/TCP accept 队列排队,不失败只等待)。评估:LAN 下总带宽守恒,顺序 ≈ 并发总耗时;顺序写盘对 HDD 友好;缺点是慢客户端(如 DERP 2Mbps)队头阻塞快客户端。**目标策略:接收端并发度 2~4 + 会话队列**(暴露 `--max-sessions`),既隔离慢链路又不抖动磁盘。
4. **每客户端十几个文件**:正确解 = 发送端**多文件支持**(单连接复用,握手一次,逐文件 manifest/续传);当前调用方循环逐文件调用也可用,但跨国高 RTT 下每次握手+manifest 往返开销被放大。

### 下一步(更新,按集成优先级)

1. **inbox 预注册**(per-task out_dir)—— gRPC/REST 集成的前置
2. 发送端多文件/目录传输(单连接多 manifest)
3. 接收端并发会话(`--max-sessions`,2~4)+ 会话队列
4. 两台物理机实测吞吐(需第二台机器)
5. gRPC(tonic)对齐 api-spec 第 7 节 —— REST 已覆盖功能,gRPC 按调用方语言需求决定

---

## 2026-10-05(深夜):P2 + P3 完成 ✅(P4 部分完成)

### P2:TCP 回退 + 路径探测

**TCP 回退(design 9.5 方案 A)**
- 收端 `--transport auto`:QUIC(UDP)与 TCP **同端口双栈监听**,谁先到来用谁;`quic`/`tcp` 强制单栈。
- 发端 `--transport auto`:QUIC 握手限时 4s,超时/失败自动落 TCP 并打降级事件(`degradation`,`L0→L4 reason=quic_unreachable_tcp_fallback`,契约 warn 级别);**认证/版本错误不降级**(降级无意义)直接报错。
- 数据流抽象:`DataOpener`(发送)/`DataInSource`(接收)双实现,QUIC=流复用,TCP=每流一条连接(写半部/读半部拆分)。
- 实测:QUIC 4s 超时→自动 TCP 传输成功;`--transport tcp` 强制 1GB 1964 MB/s(回环)。

**路径探测(probe.rs,观测级)**
- `tailscale ping --c 1 --timeout 2s <ip>`,解析 `via`(直连)/`via DERP`(中继)/`peer-relay`;输出 `path` 事件(Human/Json 双模式)。未知环境(Tailscale 缺失)静默。**仅观测,未接入降级决策**。
- 单元测试 4 例(直连/DERP/PeerRelay/失败解析)。

### P3:daemon + REST + C DLL

**基础设施**
- 证书持久化(`tls::load_or_generate_cert`,默认 `.fts\` 目录):指纹跨运行稳定 —— daemon 模式的前提,发送端可长期固定同一指纹。
- `Output::Silent` + `SessionOut` 事件通道(`tokio::sync::broadcast<String>`,事件即 NDJSON 字符串):同一套进度/结果事件同时喂 stdout(JSON 模式)与 REST SSE。

**daemon(`fts daemon`)**
- 后台任务:`run_recv` 循环(单会话失败不中断 daemon,200ms 防端口竞争空转)。
- REST(axum,仅 localhost):
  - `GET /v1/capabilities` — 协议版本 + 能力位图(bit3=resume)
  - `POST /v1/transfers` — 提交发送任务(异步 spawn,返回 task_id=UUID)
  - `GET /v1/transfers` / `GET /v1/transfers/{id}` — 列表/状态(state、checksum_ok、error_code/name/message)
  - `POST /v1/transfers/{id}/cancel` — `AbortHandle::abort`
  - `GET /v1/transfers/{id}/events` — **SSE 实时事件流**(BroadcastStream)
- 实测:REST 提交 → COMPLETED(checksum_ok=true)→ SSE 传输中事件(throughput/eta)→ cancel 生效(state=CANCELLED)。

**C ABI DLL(`fts.dll`,ffi.rs + include/fts.h)**
- 手写头文件(不引 cbindgen,4 个函数手写更快 —— Ponytail)。
- 导出:`fts_version` / `fts_abi_version` / `fts_last_error` / `fts_send_file`(阻塞式一体)。
- 内存策略:返回值全部静态或全局托管,**宿主永不释放**,`fts_last_error` 遵循"下次调用前有效"契约 —— 从根上消除跨边界 free。
- `tests/ffi_smoke.py`:Python ctypes 实测 rc=0(传输成功)、互斥参数 rc=2、错误消息可读。

**P4(部分)**
- 单元测试 10 例全绿:帧编解码往返/帧上限、Bitmap 往返/短输入、Merkle 根一致性、Hello 版本拒绝、probe 解析 4 例。
- README(快速开始/CLI/REST/FFI/已知限制)。

### 回归测试矩阵(P3 后全量)

| 用例 | 结果 |
|---|---|
| 1GB × 4 流 QUIC | ✅ 339.8 MB/s,退出码 0 |
| 1GB × 4 流 TCP-only | ✅ 1964 MB/s(回环无加密开销,真实网络不适用),退出码 0 |
| 2GB 传输中强杀 → checkpoint | ✅ meta 落盘(596/2048 块) |
| 重发自动续传 | ✅ 只补 1452 块,477.4 MB/s,校验通过 |
| REST 提交 → COMPLETED | ✅ checksum_ok=true |
| SSE 事件流 | ✅ progress(吞吐/ETA)实时 |
| REST cancel | ✅ state=CANCELLED |
| daemon capabilities | ✅ HTTP 200 |
| FFI(ctypes)发送 | ✅ rc=0;互斥参数 rc=2 |
| cargo test | ✅ 10/10 |

### 本阶段新问题与解决

#### 问题 7:P2 重构弄丢 QUIC 关闭门闩(P0 问题 2 回归)

- **现象**:daemon REST 提交的传输,数据收完、校验通过,但发送端报 `connection lost: closed by peer: 0` → 任务 FAILED(文件实际正确)。
- **根因**:重写 recv.rs 时,P0 教训的 `conn.closed().await`(等发送端读走 FinishAck 再由发送端关闭)被弄丢 —— 接收端 session 结束即 drop endpoint,QUIC 连接关闭时丢弃了未 ACK 的 FinishAck。
- **解决**:统一会话句柄时带上 `gate: Option<quinn::Connection>`,成功路径 `conn.closed().await`,失败路径 `conn.close(1)`;TCP 路径无此问题(优雅 FIN)。
- **教训**:**修过的竞态必须有测试或注释锚点**,重构时最容易丢的就是这类"看起来多余的等待"。

#### 问题 8:PowerShell 修改源码再次损坏编码(daemon.rs 被清空)

- **现象**:`$c=[IO.File]::ReadAllText(...)` 的 `$c` 在 cmd 转义中丢失,命令变成 `=[IO.File]...`,daemon.rs 内容被清空。
- **解决**:删文件 + Write 工具重写(与问题 4 同一个教训第二次发生)。**已固化规则:本项目中含中文的源码一律只经 Edit/Write 工具修改**。

#### 问题 9:edition 2024 下 `#[no_mangle]` 需写 `#[unsafe(no_mangle)]`

- Rust 2024 edition 将 no_mangle/export_name 标记为 unsafe attribute。static 的 `CString::new` 不可 const —— 换 `OnceLock<CString>`。

#### 问题 10:bin 与 lib 同名导致 pdb 冲突警告

- `fts.exe`(bin)与 lib target 同名 → `fts.pdb` 冲突 warning(当前仅警告)。后续若变 hard error,bin 改名 `fts-cli`。

### 与契约的差异(P3 后)

1. Nack 级块重传、FEC(RaptorQ)、BBR/CUBIC 切换 — 未实现(design 9.7/9.6)
2. pause/resume(REST 只有 cancel;续传靠"重发同文件") — 部分
3. tracing 结构化日志/journal 回放、`fts diagnose` — 未实现
4. 命名管道 IPC、gRPC — 未做(REST 已覆盖同机集成需求,ponytail:按需再加)
5. 多机 relay(T1)、多会话并发接收 — 未实现
6. REST 无鉴权 — 已在 README 声明

### 下一步候选(按价值)

1. 两台物理机实测(1GbE / Tailscale 跨国),校准真实吞吐数字 —— **P0 遗留的最高优先验证**
2. Nack 级块重传(坏块只重传该块)
3. tracing 日志 + `fts diagnose` 诊断包
4. 并发接收会话(daemon 多客户端)
5. pause/resume 控制面

---

## 2026-10-05(晚):P1 MVP 完成 ✅

### 本次完成内容(对照 design 第 13 节 P1 目标)

| P1 目标 | 状态 | 实现方式 |
|---|---|---|
| 多流并发 | ✅ | 发送端 N 条 unidirectional 流(默认 4,`--streams`),共享待传块栈(`Arc<Mutex<Vec<usize>>>`)条纹分配;接收端 `accept_uni` + JoinSet 逐流 spawn 任务 |
| BLAKE3 校验 | ✅ 升级 | 逐块哈希(manifest 携带全表)+ 单层 Merkle 根 `root = blake3(全部分块哈希拼接)`;接收端**边收边验**,manifest 到达时先自检 root 与哈希表一致(防篡改/截断) |
| 断点续传 | ✅ | checkpoint = `<name>.fts-part` 预分配半成品 + `.fts-part.meta`(manifest 关键字段 + 已验证位图 hex);**每秒周期落盘**(崩溃/断电/被杀均可续),传输成功后删除;重发时接收端把位图塞进 `ManifestAck.resume_bitmap_hex`,发送端只补缺失块 |
| 探测/降级骨架 | ⏳ P2 | 见下节 |
| 结构化日志 | ⏳ P2(先用 stderr 人类日志 + NDJSON 事件) | |

**其他 P1 新增**:
- **Hello 能力协商**(spec 8.2):`Hello{protocol_version, fts_version, role}`,版本不符报 `INCOMPATIBLE_VERSION`(退出码 3)。
- **退出码契约**(spec 3.3):0=成功 / 1=通用 / 2=参数(clap 默认)/ 3=连接与版本 / 4=校验 / 5=中断 / 6=取消(预留)/ 7=认证。`FtsError` 枚举 + `exit_code()`,main 统一映射。
- **`--json` NDJSON**(spec 3.4):stdout 只输出事件(`task`/`progress`/`result`,path/degradation P2 接入),人类日志走 stderr,任务 id 为 UUID v4。
- **证书固定(cert pinning)替换 P0 的跳过校验**:接收端生成自签证书并打印 **BLAKE3 指纹**;发送端 `--fingerprint <hex>`(TOFU 模型)或显式 `--insecure`;两者互斥,`--fingerprint` 与指纹不符 → `AUTH_FAILED`(退出码 7)。
- **模块化**(Ponytail:仍单 crate,但拆模块):`src/{main,error,protocol,tls,ui,send,recv}.rs`。
- 接收端预分配目标文件(`set_len`),块乱序到达按 offset 写 —— 多流的前提。

### 文件结构(P1)

```
src\
├── main.rs       # CLI 定义、参数校验、退出码映射、控制台代码页
├── error.rs      # FtsError 枚举(Connect/Version/Checksum/Auth/Interrupted/Cancelled/Io)+ 退出码/错误名
├── protocol.rs   # fts-protocol v1 P1 子集:Hello/Manifest/ManifestAck/Finish/FinishAck、帧编解码、Bitmap、merkle_root
├── tls.rs        # 自签证书生成、服务器/客户端 rustls 配置、指纹验证器、连接错误分类
├── ui.rs         # SessionOut:Human/Json 双模式输出(task/progress/path/result 事件)
├── send.rs       # 发送端:哈希预计算 → 控制流握手 → N 流并行发送 → Finish/FinishAck
└── recv.rs       # 接收端:握手 → manifest 自检 → 续传位图 → 并行收块验写 → checkpoint → 原子落盘
```

### 测试结果(P1,回环 release)

| 用例 | 结果 |
|---|---|
| 20 MB × 1 流 | ✅ 退出码 0,校验通过 |
| **1 GB × 4 流** | ✅ 退出码 0,校验通过,发送口径 **329.9 MB/s(≈2.6 Gb/s)** |
| **2 GB 中断续传** | ✅ 传至 547/2048 块强杀双方 → checkpoint 落盘 → 重发自动只补 1501 块 → 校验通过,403 MB/s |
| 指纹不匹配 | ✅ 退出码 7,`AUTH_FAILED` |
| 连接超时 | ✅ 退出码 3,`CONNECT_FAILED` |
| `--fingerprint` + `--insecure` 互斥 | ✅ 退出码 2 |
| `--json` 模式 | ✅ stdout 纯 NDJSON,`result` 事件含 `checksum_ok`/`duration_seconds` |

> 回环数字仅供参考:不经网卡、无 RTT;真实 LAN/跨国数字需两台物理机(design P1 验收 LAN ≥ 5Gb/s 待实测)。

### 遇到的问题与解决方案(P1,重点)

#### 问题 1(重要):`tokio::select!` 里的 `read_exact` 取消不安全 → 控制流帧被撕碎

- **现象**:接收端随机报 `数据流接受失败: closed by peer: 0` 或 `控制流中断: connection lost`;发送端报 `sending stopped by peer: error 0`。
- **根因**:我把 `read_frame(&mut ctl_rx)`(内部是 `read_exact`,**非取消安全**)与 `conn.accept_uni()` 放进同一个 `select!` 循环。每次 accept 分支就绪都会**丢弃**未完成的 read future —— 若它已消费了帧头的前几个字节,这些字节就永久丢失,控制流帧边界错乱,连锁导致双方互相报错。
- **解决**:控制流读取移入**独立任务**,用 `mpsc::channel<Result<Finish>>` 把 Finish 事件送回主循环;`accept_uni`(取消安全)留在 select 里。
- **教训**:tokio 文档明确 `read_exact` 不保证取消安全;select 中只能放取消安全操作,或把非安全操作隔离到专属任务 + channel。

#### 问题 2(重要):`f.read()` 短读导致分块边界错位 → 末块哈希不匹配

- **现象**:20MB 文件传输,块 2(末块)哈希不匹配,`expect=8ac83f... actual=04e52c...`。
- **根因**:发送端哈希第一遍用 `f.read(&mut buf)`,按"返回 0 即 EOF"切分块;但 **tokio `read()` 允许短读**(返回 n < 请求量不等于 EOF),块边界与第二遍发送时的 `seek + read_exact`(严格 `idx × chunk_size`)错位。
- **解决**:第一遍改为**读满一块(或 EOF)才切一个分块**:内层循环累积 `filled` 直到 `buf.len()` 或 `read() == 0`。
- **教训**:任何"按缓冲区大小切块"的逻辑都必须假设 `read()` 短读;`read_exact` 语义(读满或 EOF)才可依赖。

#### 问题 3:checkpoint 时机的设计缺陷(自查发现)

- **问题**:初版 checkpoint 只在**优雅失败**路径落盘;若进程被强杀/断电,meta 不存在,续传失效 —— 与"断点续传"目标矛盾。
- **解决**:checkpoint 并入每秒进度线程,有验证进展即落盘(`fs::write` 全量覆盖位图;位图只增不减,覆盖写安全)。
- **取舍**:1 秒窗口内最多重复传 1 秒的数据;每秒一次小文件写盘不构成瓶颈(对比 GB 级传输)。

#### 问题 4(流程):PowerShell 管道改写源码 → 编码损坏 → rustc ICE

- **现象**:用 `Get-Content | -replace | Set-Content` 批量改 recv.rs 后,rustc 直接 ICE(`assertion failed: bpos.to_u32() >= mbc.pos.to_u32()`);检查发现文件被以 GBK 读入再存盘,UTF-8 中文变乱码字节。
- **解决**:删除文件用 Write 工具整体重写;**今后批量修改源码一律走 Edit/Write 工具,不走 shell 文本管道**(尤其含中文的文件)。
- **教训**:Windows PowerShell 5.1 默认编码不是 UTF-8;`-Encoding` 参数只保护 `Get-Content`,`Set-Content` 需显式指定。

#### 问题 5:接收端计时起点含对端准备时间

- **现象**:recv 平均吞吐(214.7 MB/s)远低于 send(329.9 MB/s),但两者传的是同一份数据。
- **根因**:recv 的计时从进程启动开始,包含 sender 哈希预计算的空闲等待。
- **解决**:`SessionOut::reset_started()` 在收到 manifest(真正开始收块)时重置计时。数字口径:两边均从"握手完成"起算。

#### 问题 6:quinn/rustls API 备忘(新增)

| 事项 | 结论 |
|---|---|
| `PrivatePkcs8KeyDer` 克隆 | 用 `clone_key()`;`with_single_cert` 需要 `PrivateKeyDer::Pkcs8(..)` 包装 |
| quinn 原生流读写 | `SendStream::write_all` / `RecvStream::read_exact` 是 quinn 自有 API,**不**需要 tokio 的 AsyncWriteExt/AsyncReadExt trait(但在 select 中混用时注意取消安全) |
| quinn `RecvStream` 读到 FIN | 原生 `read_exact` 返回 `ReadExactError::Finished`,Display 为 "early eof" —— 程序里靠字符串匹配识别流正常结束 |

### 与契约的差异(P1 后仍缺,按优先级)

1. 逐块 Nack/重传(块损坏即整体失败)—— P2
2. 能力位图细化、`fts status/cancel/pause`、daemon —— P2/P3
3. tracing 结构化日志 + journal 回放 —— P2
4. TCP 回退、FEC、BBR/CUBIC 切换、Tailscale 链路识别 —— P2(BBR 注意:quinn 未内置 BBR 实现,评估自研成本或维持 CUBIC,见 design 9.6)
5. C ABI / IPC / gRPC —— P3
6. fuzz、CI 基准 —— P4

---

## 2026-10-05:P0 可行性原型完成 ✅

### 本次完成内容

**目标**(对应 `fts-design.md` 第 13 节 P0):最小原型 `send`/`recv` 单文件,验证 Windows + QUIC 路线可行。

**交付**:
- 单文件传输闭环:QUIC(quinn 0.11)双向流、逐块传输、BLAKE3 逐块哈希累积 = 文件级哈希、接收端全量校验。
- 线协议 P0 子集(对齐 `fts-api-spec.md` 第 8 节):`Manifest → ManifestAck → [chunk 帧]… → Finish → FinishAck`。帧格式:u32 LE 长度前缀 + JSON(manifest/finish 类)或裸字节(分块帧:`[chunk_id:u32][offset:u64][len:u32][payload]`)。
- CLI:`fts send <path> --to <addr> [--chunk-size N]` / `fts recv --listen <addr> --out <dir>`(clap)。
- 原子落盘:先写 `<name>.fts-part`,BLAKE3 校验通过后才 rename 为最终名(spec 8.7 契约)。
- TLS:自签证书(rcgen)+ 客户端跳过校验(SkipVerify verifier)。**P0 专用,P1 必须换**(见 TODO)。

**当前文件结构**(Ponytail:单 crate,P1 再拆 workspace):

```
C:\develop\File-Transfer\
├── AGENTS.md              # 工作区纪律:在 /plan 记录规划
├── Cargo.toml / Cargo.lock
├── src\
│   └── main.rs            # 全部 P0 代码(~440 行):CLI、线协议帧、TLS、send/recv
├── plan\                  # 本规划目录(见上方文档地图)
│   ├── fts-design.md
│   ├── fts-t1-relay-design.md
│   ├── fts-api-spec.md
│   └── dev-log.md         # 本文件
├── skills\
│   └── ponytail.md        # 开发纪律 skill
└── target\                # 构建产物(gitignore)
```

**依赖清单**(Cargo.toml):

| crate | 版本 | 用途 | 备注 |
|---|---|---|---|
| quinn | 0.11.12 | QUIC 实现 | 默认 ring provider |
| tokio | 1.53.2 (full) | 异步运行时 | Windows 上基于 IOCP |
| clap | 4.6.7 (derive) | CLI | |
| serde / serde_json | 1.0.229 / 1.0.151 | 帧序列化 | P0 用 JSON,后续控制帧可换 bincode |
| blake3 | 1.8.7 | 校验 | SIMD 多线程 |
| rcgen | 0.14.10 | 自签证书 | |
| rustls | 0.23.45 | TLS 配置(直接依赖只为构建 quinn crypto 配置) | |
| anyhow | 1.0.104 | 错误 | |
| windows-sys | 0.61.2 (Win32_System_Console) | 设控制台代码页 65001 | 修中文乱码 |

### 测试结果(P0 真实数字)

环境:本机回环(loopback),release 构建,NTFS,块大小 8 MiB,单 QUIC 双向流。

| 用例 | 结果 |
|---|---|
| 100 MB 文件传输 | ✅ 校验通过,接收端平均 ~294 MB/s(≈2.4 Gb/s) |
| **1 GB 文件传输** | ✅ 校验通过,双向平均 **343.7 MB/s(≈2.75 Gb/s)**,耗时 2.98s,退出码 0 |
| 接收端落盘后文件大小 | 1,073,741,824 字节,精确一致 |
| 连接不存在端口 | ✅ 优雅报错 `QUIC 握手失败: timed out`,退出码 1(非 panic) |

**P0 验收标准对照**(design 第 13 节:LAN ≥ 3Gb/s):环回单流 2.75 Gb/s,略低于 3Gb/s 目标。回环测试的瓶颈在**接收端单线程 读→seek→写→哈希 串行**与单流窗口,不是 QUIC 本身。**多数据流(N 条 unidirectional)+ 收发并行化是 P1 第一个优化点**,真实 LAN(1/10GbE)数字要在两台物理机之间测才算数 —— 回环不经过网卡,数字仅供参考上限。

### 遇到的问题与解决方案(详细记录,便于复现排查)

#### 问题 1:传输完成后发送端挂 30 秒才退出

- **现象**:1GB 实际传输 ~3s,但发送端总耗时 33.6s,平均吞吐被稀释到 30.4 MB/s。
- **定位**:发送端最后的 `endpoint.wait_idle().await` 在等连接关闭;双方都没有显式 close,QUIC 连接要等**空闲超时(默认 30s)**才断。
- **解决**:传输完成后显式 `conn.close(0u32.into(), b"")`,删除两处 `wait_idle()`。
- **教训**:quinn 不会因为你调了 `tx.finish()` 就关连接;`finish()` 只关发送方向流。

#### 问题 2:显式 close 后引入新竞态 —— FinishAck 丢失

- **现象**:接收端发完 `FinishAck` 立即 `conn.close(0)`,发送端报 `connection lost: closed by peer: 0`,退出码 1。
- **根因**:`SendStream::finish()` 只标记 FIN,`conn.close()` 发 CONNECTION_CLOSE 时,**尚未被 ACK 的流数据可能直接丢弃**。接收端 close 太快,FinishAck 没送达。
- **解决**:确立"**谁最后完成谁关连接**"规则 —— 接收端发完 FinishAck 后只 `tx.finish()` 并 `conn.closed().await`(等发送端来关);发送端读到 FinishAck 后才 `conn.close(0)`。
- **教训**:QUIC 应用层"最后一条消息"交付需要等对端读走;关闭方向要设计好,不能两边都随手 close。

#### 问题 3:quinn / rustls / rcgen 新版 API 陷阱(编译期)

这三个是本次编译错误最多的来源,记录 API 实测结论(以 quinn 0.11.12 / rustls 0.23.45 / rcgen 0.14.10 为准):

| 陷阱 | 正确写法 |
|---|---|
| `quinn::WindowSize` 类型不存在 | `TransportConfig::receive_window(VarInt)` / `stream_receive_window(VarInt)`,都用 `quinn::VarInt::from_u32(..)` |
| `quinn::ServerConfig::with_crypto(Arc<rustls::ServerConfig>)` 类型不匹配 | rustls 配置需先转 `quinn::crypto::rustls::QuicServerConfig::try_from(crypto)?`;客户端同理 `QuicClientConfig` |
| rustls 服务端配置顺序 | `builder_with_provider(p).with_protocol_versions(..)?.with_no_client_auth().with_single_cert(cert, key)?` —— **先 no_client_auth 再 single_cert**,顺序反了类型不通过 |
| rcgen 0.14 字段改名 | `CertifiedKey { cert, signing_key }`,取私钥用 `signing_key.serialize_der()`,**没有 `key_pair` 字段** |
| rustls 危险验证器方法名 | 是 `with_custom_certificate_verifier`,不是 `with_certificate_verifier` |
| rustls crypto provider | quinn 默认 ring;自定义配置时用 `rustls::crypto::ring::default_provider()`,避免 aws-lc-rs/ring 双 provider 冲突 |
| rustls ALPN | 不会自动设置;`crypto.alpn_protocols = vec![ALPN.to_vec()]`(两端都要),ALPN 固定 `fts/1` |

#### 问题 4:控制台中文乱码

- **现象**:Rust 输出 UTF-8,Windows 控制台默认 GBK(936),`recv: 完成` 变乱码。
- **解决**:main 开头 `windows_sys::Win32::System::Console::SetConsoleOutputCP(65001)`。
- **注意**:该函数在 `Win32::System::Console` 模块,**不在** `Win32::Globalization`(容易猜错)。
- **注意**:输出重定向到文件时是纯 UTF-8 字节,用 `Get-Content -Encoding UTF8` 读。

#### 问题 5:cmd 下 `%ERRORLEVEL%` 测不准

- **现象**:`fts send ... & echo EXITCODE=%ERRORLEVEL%` 总显示 0。
- **根因**:cmd 对同一行的 `%ERRORLEVEL%` 在**解析期**展开,此时前一个命令还没跑。
- **解决**:用 PowerShell `& fts.exe ...; $LASTEXITCODE`,或 cmd `call echo`。CI 脚本注意同样问题。

#### 问题 6(流程):Ponytail 与设计文档的 workspace 结构冲突

- `fts-design.md` 建议拆 8 个 crate,但 P0 阶段单 crate 闭环更快、更符合 Ponytail(" Fewest files possible")。
- **决策**:P0 单 crate `fts`;P1 引入多流/续传/探测时按 design 第 8 节拆 `fts-core`/`fts-transport`/`fts-cli`。拆分时机 = 第二个人要并行改核心库,或 gRPC/IPC 外壳开始出现。

### 当前与契约的差异(P0 已知简化,P1 必须补)

| 契约要求(api-spec) | P0 现状 | 归属 |
|---|---|---|
| Hello 帧(版本 + 能力位图协商) | 无,直接 Manifest | P1 |
| 多条 unidirectional 数据流 | 单双向流 | P1(吞吐关键) |
| Merkle 树根 + 分块哈希表 | 逐块哈希累积成文件哈希 | P1 |
| 断点续传(ResumeRequest/位图) | 无 | P1 |
| 退出码契约(3=连接失败,4=校验失败…) | 只有 0/1 | P1 |
| `--json` NDJSON 输出 | 无 | P1 |
| 证书校验(预共享密钥/证书固定) | **跳过校验(危险)** | **P1 安全项,最高优先** |
| 分块哈希失败 ChunkNack 重传 | 校验失败即整体失败 | P2 |
| tracing 结构化日志 / journal | println | P1 |
| TCP 回退、FEC、BBR、探测降级 | 无 | P2 |
| `fts path`、daemon、IPC/gRPC/DLL | 无 | P2/P3 |

### 下一步(P1 MVP 候选顺序,按价值排序)

1. **多数据流 + 收发端并行化**(吞吐 → 3Gb/s+):N 条 unidirectional 流条纹化;接收端 写盘/哈希 与读流并行。
2. **连接安全**:预共享密钥(PSK)或证书固定,替换 SkipVerify。
3. **Hello 能力协商 + 退出码契约 + `--json`**(对齐 api-spec 第 3、9、11 节)。
4. **断点续传**:checkpoint + 已收块位图。
5. **tracing 结构化日志**(warn 级别必须包含降级/路径事件 —— 契约)。
6. 两台物理机 1GbE/10GbE 实测,校准 P0 目标(3Gb/s 是否现实)。
