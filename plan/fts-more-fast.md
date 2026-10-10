fts 文件级并发传输(协议 v2:槽位化多文件流水线)

预期收益(诚实版)

文件并发不提高链路吞吐(单文件 4 流已打满万兆),收益 = 消除文件间串行阶段:



hash-ahead(预读哈希隐藏):目前每文件完整读两遍盘,预读期间网线闲置。此项独立生效,即使 K=1 也有效,是保底收益。预期万兆+NVMe 下消掉 \~27% 每文件耗时。

文件流水线(K>1):消除每文件 2 轮握手与文件尾部排空的间隙;小文件越多收益越大。

量化:NVMe+万兆 ≈ 1.3\~1.4×;NVMe+千兆 ≈ 1.05×;HDD ≈ 1.1\~1.2×;大量小文件 ≈ 1.5\~3×。

e2e 在 loopback 上主要验证正确性与并发生效;真实倍数需物理内网测,dev-log 记录测法。

后续方向(本轮不做):单遍哈希(边发边算消双读)——与逐块到达校验/续传位图冲突,需协议级重构。

决策(已定)

仅 QUIC 启用并发;TCP 回退与协商失败自动退回串行(行为与今天一致)。

默认自动开启:file\_concurrency 默认 4(单文件任务强制 1),两端 W9 一起升级,无需用户感知。

现状根因

任务内文件严格串行(src/send.rs:257-269、src/recv.rs:311-319):每文件 2 轮握手 + 全文预读哈希 + 文件间零重叠。数据流帧头无文件标识(靠"最近一个 Manifest"隐式归属),check\_hello 是 v1 严格相等(src/protocol.rs:145-154)→ 并发化必须动线协议。



改动清单

1\. src/protocol.rs — 线协议 v2

PROTOCOL\_VERSION = 2;Hello 加 #\[serde(default)] caps: u32(bit0 = MULTIFILE)。

Manifest/ManifestAck/Finish/FinishAck 加 slot: Option<u32>(skip\_serializing\_if none——串行模式字节级与 v1 相同)。

数据流 v2 前导 \[slot: u32 LE] 4 字节 + 编解码函数;TransferMode { Sequential, Concurrent }。

单测:协商矩阵、v1 帧向后反序列化、前导编解码。

2\. src/send.rs — 管道化(核心)

hash-ahead 生产者:后台顺序预读+哈希文件队列,送有界 mpsc(容量 K+1),把预读藏进前一文件的传输窗口。

K 个文件管道任务:取活儿→占 slot→Manifest{slot}→Ack(续传位图)→N 条流任务(各写 slot 前导,复用现有 chunk 栈模式)→Finish{slot}→FinishAck{slot}。任一失败全管道中止(快失败不变)。

SendCfg.file\_concurrency,生效值 = min(请求, 协商能力);单文件/TCP/Sequential 强制 1。

作业级进度 JobProgress{bytes\_done/total, files\_done/total},progress 事件加附加字段(additive 兼容)。

3\. src/recv.rs — 槽位路由

session\_logic 串行循环 → HashMap<u32, SlotHandle>;数据流读 4 字节前导路由到对应槽 mpsc;Sequential 模式保留原路径。

每槽 = 现 handle\_one\_file 改造;JoinSet/每秒检查点/原子 rename 不变(已 per-file 隔离)。快失败保持:任一槽失败关会话。

4\. 接口插线

CLI --file-concurrency(0=自动 4);顺手修 main.rs:19 过期帮助文案。

REST SubmitReq.file\_concurrency;proto TransferRequest 加 uint32 file\_concurrency = 15(grpc.rs clamp 1..=16);FFI 不动。

FileConfig 加可选 quic\_window\_mib(默认 32)。

orch core.py defaults 透传 + gen\_proto.py 重生成 stub + config.example.json 更新。

5\. 测试与文档

新增 tests/multi\_file\_concurrency.py:20 文件混合大小 → COMPLETED+checksum\_ok+树逐字节一致 + 并发生效断言(第 2 个 file 事件先于第 1 个 file\_done)+ 中断续传 + 取消;输出吞吐数字供记录。

现有 Python smoke + cargo test 全绿。

plan/dev-log.md 新条目;plan/fts-api-spec.md 第 8 节追加 v2 修订;README。

非目标

keep-going、TCP 并发、单遍哈希、任务级 per-file 失败状态上报。



实施顺序

protocol.rs v2 → recv.rs 槽位路由 → send.rs 管道化(hash-ahead 优先)→ 接口插线 → e2e + 文档。

