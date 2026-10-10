# TODO — fts-more-fast(文件级并发传输,协议 v2)

设计真相:plan/fts-more-fast.md(决策已定,不再复议)。
地图:repomix-output.md(2026-10-08 生成;git 状态显示其后仅 daemon/error/netcred 有未提交改动,与本任务文件无交集)。

## 关键实现决策(执行中细化)

- 模式判定统一:QUIC 且对端 Hello.caps bit0(MULTIFILE)→ Concurrent,否则 Sequential。两端同规则,靠 transport+caps 对齐,无需额外协商帧。
- 单文件任务在 QUIC 下也走 Concurrent 帧(slot=Some(0)),保证"QUIC+caps ⇒ v2 帧"规则无例外;K(同时 in-flight 文件数)= min(请求值, 文件数),与帧模式解耦。
- Sequential 模式字节级等于 v1:控制帧 slot=None(skip 序列化不输出)、数据流无前导。
- sender 侧:控制写 = Arc<Mutex<BoxWrite>>(整帧原子);控制读 = 单一 router 任务按 slot 路由到各 worker 的 ack/finish 通道(None → 通道 0)。
- sender 快失败 = JoinSet::join_next 首个 Err → abort_all + producer.abort。
- recv 侧:ctl reader 按 slot 路由(HashMap<u32,SlotHandle>,get_or_insert 由会话循环与数据流 router 共用);数据流 router 读 4 字节前导路由到槽的 unbounded 通道。
- recv Finish 后补收(原 ZERO-timeout drain 的并发版):drain 通道至空 + 等 slot inflight==0(计数在 router 入队前 inc,recv_stream 结束 dec)。
- JobProgress:progress 事件追加 "job":{bytes_done,bytes_total,files_done,files_total}(additive;job 字节在流任务 fetch_add,files_done 在 FinishAck 后 +1)。
- Hello 测试 hello_version_rejected 用字面量 2,版本升 2 后改为 PROTOCOL_VERSION+1。

## 阶段

1. [done] protocol.rs v2:VERSION=2、Hello.caps、四结构 slot:Option<u32>、SLOT_PREAMBLE 编解码、TransferMode+negotiate_mode、单测(协商矩阵/v1 帧向后反序列化/前导往返/修 hello_version_rejected)
2. [done] recv.rs 槽位路由:模式判定、session_concurrent(reader 路由 + 数据流 router + SlotHandle + slot 任务循环)、handle_one_file 改 StreamSource(Direct/Piped)、Sequential 原路径保留(slot:None 补字段)
3. [done] send.rs 管道化:hash-ahead 生产者(有界 mpsc K+1)、K worker(固定 slot=i)、ack router、JobCounters、SendCfg.file_concurrency
4. [done] 接口插线:main.rs --file-concurrency + main.rs:19 过期文案修复 + SendCfg/RecvCfg 字面量;daemon.rs SubmitReq.file_concurrency + FileConfig.quic_window_mib(默认 32)贯通 bind;grpc.rs TransferRequest.file_concurrency(clamp 1..=16,0=auto);ffi.rs/update.rs 字面量补 0;proto TransferRequest file_concurrency = 15
5. [done] orch:core.py defaults 透传 + TransferRequest 字段;config.example.json defaults.file_concurrency=0
6. [done] 测试:cargo test 全绿(30);tests/multi_file_concurrency.py T1/T2/T3 PASS(20 文件、并发断言、续传、取消);grpc_smoke 300MB T9 PASS;ffi_smoke PASS;daemon 自传 5 连发(含同名 600MB)全 COMPLETED
7. [done] 文档:plan/dev-log.md 条目、plan/fts-api-spec.md 第 8.9 节 v2 修订、README --file-concurrency

## 完成标准

- cargo build + cargo test 全绿
- python tests/multi_file_concurrency.py PASS(含并发生效断言)
- 现有 smoke 不回归:python tests/ffi_smoke.py、grpc_smoke.py(需 daemon)

## 错误记录表

| 错误 | 处置 |
|------|------|
| unbounded mpsc `send` 是同步方法,19 处 `.await` 编译错 | 去掉 await |
| sender worker 通道:router 先 spawn 会捕获空 Vec,后建通道无效 | 通道对先建满,router 拿发送端,worker remove(0) 按序拿接收端 |
| recv CtrlClosed 处理:slot_loop 持 `Arc<SlotShared>` → drop 表后引用计数不归零,槽永远等不到通道关闭,permit 泄漏 | slot_loop 只拿接收半部 + 独立 inflight Arc;收尾先 router.abort() 再 drop 表 |
| 既有缺陷(W2):接收端 gate `conn.closed().await` 无界等;daemon 自传时发送端 drop 不保证发出 close 帧 → 3 个会话后 max_sessions 信号量耗尽,后续任务全部饿死(QUIC 握手无人受理 → 降级 TCP → TCP 停在 backlog,全停摆) | 发送端 Ok 路径显式 `conn.close(0)`;gate 加 30s 上界 |
| e2e:TCP 探测等就绪会被 CLI recv 当成会话消费掉 | 等 stderr "监听" 行 |
| e2e:loopback 太快(<1s)没有 progress tick,job 字段断言空转 | 有 progress 事件才断言 job |
| 用 PowerShell 重写 py 源码 → GBK 重编码毁中文(记忆坑重蹈) | Write 工具整文件重写;今后源码一律不走 PS 改写 |
| T3 QUIC 下杀 sender 不退出:QUIC 静默掉线无 idle timeout(与 v1 一致,非本轮回归) | T3 改 --transport tcp(RST 即时暴露);QUIC idle timeout 记为后续项 |

## 发布 0.2.0(2026-10-10,运维操作)

1. [done] 版本 0.1.0 → 0.2.0(Cargo.toml),build.ps1 构建 dist(VERSION=0.2.0)
2. [done] 探活 orch/config.json 8 台 endpoint:8/8 存活,均 v0.1.0
3. [done] 逐台 push-update:8/8 "更新成功: 0.1.0 → 0.2.0(重启确认)",started_at 全部刷新
4. [done] 复验 8/8 fts_version=0.2.0;本机 0.2.0 sender → 192.168.1.60 真实内网 QUIC v2 传输 COMPLETED;dev-log 已记录

完成标准达成:所有存活 daemon 的 fts_version = 0.2.0;不可达主机:无。

### 错误记录表(发布轮)

| 错误 | 处置 |
|------|------|
| 0.2.0 发送端直推 0.1.0 目标:8 台全部 0.02s FAILED "early eof" —— 推送传输本身要过协议握手,v2 Hello 被 v1 check_hello 拒绝(先有鸡先有蛋) | 用 git 根提交(13aa3fc)worktree 构建 0.1.0 引导 exe,以 v1 发送端推 v2 dist(v1→v1 传输兼容,BeginUpdate 换装 0.2.0);此手法适用于今后任何跨协议版本的首跳升级 |
