//! 接收端(P5):QUIC 与 TCP 监听同一端口;W1 ticket 决定落盘目录;W2 多文件会话;W3 并发会话。
//! 会话 = Hello ↔ Hello → N × (Manifest → 并行收流逐块校验落盘 → Finish → 校验 → 原子落盘 → FinishAck)。
//! 续传:checkpoint(<name>.fts-part.meta)每秒落盘,重发同 manifest 自动只补缺失块。
//!
//! 并发说明(规划 W3):QUIC 会话天然可并发;**TCP 会话全局互斥** —— TCP 回退模式下
//! 数据流与新控制连接共享同一 listener,无法区分,故同一时间只允许一个 TCP 会话
//! (其余排队)。这是刻意简化,见 plan/fts-p5-integration-design.md 第 4 节。

use crate::error::FtsError;
use crate::protocol::*;
use crate::send::{BoxRead, BoxWrite};
use crate::ui::{Output, Progress, SessionOut};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore};
use tokio::task::JoinSet;

#[derive(Serialize, Deserialize, Clone)]
struct Checkpoint {
    file_name: String,
    file_size: u64,
    chunk_size: u32,
    merkle_root: String,
    bitmap_hex: String,
}

#[derive(Clone)]
pub struct RecvCfg {
    pub listen: String,
    pub out_dir: PathBuf,
    pub out: Output,
    pub transport: String, // auto | quic | tcp
    /// 证书缓存目录(指纹跨运行稳定;daemon 必需)
    pub cert_cache: PathBuf,
    /// daemon 模式事件通道(REST SSE)
    pub events: Option<tokio::sync::broadcast::Sender<String>>,
    /// W1 inbox 预注册表;None = 不支持 ticket(收到带 ticket 的传输即拒绝)
    pub inbox: Option<Arc<crate::inbox::InboxRegistry>>,
}

/// 监听器集合(绑定一次,反复 accept)
pub struct Listeners {
    endpoint: Option<quinn::Endpoint>,
    /// TCP listener 用互斥锁保护:TCP 会话独占(见模块注释)
    tcp: Option<Arc<Mutex<tokio::net::TcpListener>>>,
}

pub enum Incoming {
    Quic(quinn::Connection),
    Tcp(tokio::net::TcpStream),
}

/// 数据流来源:QUIC 连接上继续 accept_uni;TCP 会话独占 listener 接收数据流连接。
pub enum DataInSource {
    Quic(quinn::Connection),
    Tcp(OwnedMutexGuard<tokio::net::TcpListener>),
}

impl DataInSource {
    async fn next_stream(&self) -> std::io::Result<BoxRead> {
        match self {
            DataInSource::Quic(conn) => {
                let s = conn.accept_uni().await?;
                Ok(Box::new(s))
            }
            DataInSource::Tcp(l) => {
                let (s, _) = l.accept().await?;
                let (r, w) = s.into_split();
                drop(w); // 写端立即丢弃:发送端持有对应写半部
                Ok(Box::new(r))
            }
        }
    }
}

/// CLI 入口:绑定 → 处理一个会话 → 退出(面向人)
pub async fn run_recv(cfg: RecvCfg) -> Result<(), FtsError> {
    run_recv_inner(cfg)
        .await
        .map_err(|e| match e.downcast::<FtsError>() {
            Ok(fts_err) => fts_err,
            Err(other) => FtsError::Io(format!("{other:#}")),
        })
}

async fn run_recv_inner(cfg: RecvCfg) -> Result<()> {
    let mut out = SessionOut::new(cfg.out);
    if let Some(tx) = &cfg.events {
        out = out.with_events(tx.clone());
    }
    out.task_created();
    let listeners = bind(&cfg, &out).await.map_err(downcast_fts)?;
    let incoming = accept_one(&listeners).await?;
    handle_incoming(&cfg, &mut out, incoming, listeners.tcp.clone()).await
}

fn downcast_fts(e: anyhow::Error) -> FtsError {
    match e.downcast::<FtsError>() {
        Ok(fts_err) => fts_err,
        Err(other) => FtsError::Io(format!("{other:#}")),
    }
}

/// daemon 入口(W3):循环 accept,最多 max_sessions 个并发会话;单个会话失败不影响服务。
pub async fn serve(cfg: RecvCfg, max_sessions: usize) -> Result<(), FtsError> {
    let out = SessionOut::new(cfg.out);
    let listeners = bind(&cfg, &out).await.map_err(downcast_fts)?;
    let sem = Arc::new(Semaphore::new(max_sessions.max(1)));
    eprintln!("fts recv 服务循环启动(最大并发会话 {max_sessions})");
    loop {
        let permit = sem.clone().acquire_owned().await.expect("semaphore");
        let incoming = match accept_one(&listeners).await {
            Ok(i) => i,
            Err(e) => {
                eprintln!("[warn] accept 失败: {e:#}");
                drop(permit);
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let cfg = cfg.clone();
        let tcp_listener = listeners.tcp.clone();
        tokio::spawn(async move {
            let mut out = SessionOut::new(cfg.out);
            if let Err(e) = handle_incoming(&cfg, &mut out, incoming, tcp_listener).await {
                eprintln!("[warn] 接收会话失败: {e}");
            }
            drop(permit);
        });
    }
}

/// 绑定监听(按 transport 配置:QUIC(UDP) 与/或 TCP,同一端口号)
/// 运维信息直接走 stderr(SessionOut 可能是 Silent —— daemon 模式也要能看到指纹)
async fn bind(cfg: &RecvCfg, _out: &SessionOut) -> Result<Listeners> {
    let addr: SocketAddr = cfg
        .listen
        .parse()
        .map_err(|e| FtsError::Io(format!("监听地址无效 {}: {e}", cfg.listen)))?;

    let cert = crate::tls::load_or_generate_cert(&cfg.cert_cache)?;
    eprintln!("fts recv 证书指纹(blake3): {}", cert.fingerprint);
    eprintln!("      发送端请用 --fingerprint <上述指纹> 连接(或 --insecure,仅测试)");

    let mut endpoint: Option<quinn::Endpoint> = None;
    let mut tcp: Option<Arc<Mutex<tokio::net::TcpListener>>> = None;
    if cfg.transport != "tcp" {
        let mut server_cfg = crate::tls::server_config(&cert)?;
        server_cfg.transport_config(Arc::new({
            let mut tc = quinn::TransportConfig::default();
            let win = quinn::VarInt::from_u32(32 * 1024 * 1024);
            tc.stream_receive_window(win);
            tc.receive_window(win);
            tc
        }));
        let ep = quinn::Endpoint::server(server_cfg, addr)
            .map_err(|e| FtsError::Io(format!("QUIC 监听失败: {e}")))?;
        eprintln!("fts recv QUIC(UDP)监听 {}", ep.local_addr()?);
        endpoint = Some(ep);
    }
    if cfg.transport != "quic" {
        let l = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| FtsError::Io(format!("TCP 监听失败: {e}")))?;
        eprintln!("fts recv TCP 监听 {}", l.local_addr()?);
        tcp = Some(Arc::new(Mutex::new(l)));
    }
    Ok(Listeners { endpoint, tcp })
}

async fn accept_one(l: &Listeners) -> Result<Incoming> {
    match (&l.endpoint, &l.tcp) {
        (Some(ep), Some(tcp)) => {
            tokio::select! {
                inc = ep.accept() => {
                    let conn = inc.context("endpoint 已关闭")?.await
                        .map_err(|e| FtsError::Connect(format!("QUIC 连接失败: {e}")))?;
                    Ok(Incoming::Quic(conn))
                }
                acc = async { tcp.lock().await.accept().await } => {
                    Ok(Incoming::Tcp(acc.map_err(FtsError::io)?.0))
                }
            }
        }
        (Some(ep), None) => {
            let conn = ep.accept().await.context("endpoint 已关闭")?.await
                .map_err(|e| FtsError::Connect(format!("QUIC 连接失败: {e}")))?;
            Ok(Incoming::Quic(conn))
        }
        (None, Some(tcp)) => {
            let (s, _) = tcp.lock().await.accept().await.map_err(FtsError::io)?;
            Ok(Incoming::Tcp(s))
        }
        (None, None) => unreachable!("transport 配置至少启用一种传输"),
    }
}

/// 处理一个会话(QUIC 或 TCP)。
/// `tcp_listener`:TCP 会话需独占 listener(OwnedMutexGuard 锁到会话结束);
/// QUIC 会话不使用它。listener 本体保留在 Listeners 里供后续会话复用。
async fn handle_incoming(
    cfg: &RecvCfg,
    out: &mut SessionOut,
    incoming: Incoming,
    tcp_listener: Option<Arc<Mutex<tokio::net::TcpListener>>>,
) -> Result<()> {
    // 统一会话句柄;gate = QUIC 连接句柄,会话成功后等发送端读走最后一个 FinishAck 再关
    let (ctrl_tx, ctrl_rx, data_src, gate): (BoxWrite, BoxRead, DataInSource, Option<quinn::Connection>) =
        match incoming {
            Incoming::Quic(conn) => {
                let (tx, rx) = conn.accept_bi().await.map_err(|e| {
                    FtsError::Connect(format!("接受控制流失败: {e}"))
                })?;
                (Box::new(tx), Box::new(rx), DataInSource::Quic(conn.clone()), Some(conn))
            }
            Incoming::Tcp(stream) => {
                let listener = tcp_listener.expect("TCP 会话必有监听器");
                // 独占整个 TCP 会话期间(锁到 guard drop):数据流与新会话共用 listener
                let guard = listener.lock_owned().await;
                let (r, w) = stream.into_split();
                (Box::new(w), Box::new(r), DataInSource::Tcp(guard), None)
            }
        };

    let result = session_logic(cfg, out, ctrl_tx, ctrl_rx, &data_src).await;
    match (&result, gate) {
        // QUIC:close 会丢弃未 ACK 的流数据。
        // 成功路径:等发送端读完 FinishAck 后由它关闭;
        // 错误路径:session 可能刚发完 deny/错误 Ack(如 ticket 拒绝)——给对端
        // 2s 读走最后一条消息的机会,超时才强断(否则退出码语义丢失,只剩 connection lost)。
        (Ok(()), Some(conn)) => {
            conn.closed().await;
        }
        (Err(_), Some(conn)) => {
            let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
            conn.close(1u32.into(), b"error");
        }
        _ => {}
    }
    result
}

async fn session_logic(
    cfg: &RecvCfg,
    out: &mut SessionOut,
    mut ctrl_tx: BoxWrite,
    mut ctrl_rx: BoxRead,
    data_src: &DataInSource,
) -> Result<()> {
    let peer: Hello = parse_json(&read_frame(&mut ctrl_rx, 4096).await?)?;
    check_hello(&peer)?;
    write_frame(
        &mut ctrl_tx,
        &serde_json::to_vec(&Hello {
            protocol_version: PROTOCOL_VERSION,
            fts_version: env!("CARGO_PKG_VERSION").into(),
            role: "dest".into(),
        })?,
    )
    .await?;

    // 常驻控制流读取任务:按帧类型分发(Manifest → manifest 通道;Finish → finish 通道)。
    // read_exact 取消不安全,不能直接放进 select!(否则帧被撕碎 —— P1 教训);
    // 也不能 per-file abort —— abort 会 drop 掉任务内的 RecvStream,向发送端发
    // STOP_SENDING,打断多文件会话的后续控制流(W2 教训)。
    let (manifest_tx, mut manifest_rx) = tokio::sync::mpsc::channel::<Manifest>(1);
    let (finish_tx, mut finish_rx) = tokio::sync::mpsc::channel::<Result<Finish>>(1);
    let ctl_reader = tokio::spawn(async move {
        loop {
            match read_frame(&mut ctrl_rx, MAX_CTRL_FRAME).await {
                Ok(bytes) => {
                    if let Ok(m) = serde_json::from_slice::<Manifest>(&bytes) {
                        if manifest_tx.send(m).await.is_err() {
                            break; // 接收端已离开会话循环
                        }
                    } else {
                        match serde_json::from_slice::<Finish>(&bytes) {
                            Ok(f) => {
                                if finish_tx.send(Ok(f)).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                let _ = finish_tx.send(Err(e.into())).await;
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    // EOF(发送端全部传完并关闭)或错误 → 通道关闭 → 会话循环自然退出
                    let _ = finish_tx
                        .send(Err(anyhow::anyhow!(FtsError::Interrupted(format!(
                            "控制流中断: {e}"
                        )))))
                        .await;
                    break;
                }
            }
        }
    });

    // W2:多文件循环;任一文件失败即中止会话(与发送端快失败对齐)
    let mut files_done = 0u32;
    let result = loop {
        let Some(manifest) = manifest_rx.recv().await else {
            break Ok(()); // 控制流结束 = 会话正常结束
        };
        match handle_one_file(cfg, out, manifest, data_src, &mut ctrl_tx, &mut finish_rx).await {
            Ok(()) => files_done += 1,
            Err(e) => break Err(e),
        }
    };
    ctl_reader.abort();
    if result.is_ok() && files_done == 0 {
        return Err(anyhow::anyhow!(FtsError::Interrupted(
            "会话未传输任何文件".into()
        )));
    }
    result
}

/// 单文件:manifest 自检 → ticket → 续传 → Ack → 并行收流 → Finish → 校验 → 原子落盘 → FinishAck
async fn handle_one_file(
    cfg: &RecvCfg,
    out: &mut SessionOut,
    manifest: Manifest,
    data_src: &DataInSource,
    ctrl_tx: &mut BoxWrite,
    finish_rx: &mut tokio::sync::mpsc::Receiver<Result<Finish>>,
) -> Result<()> {
    if manifest.checksum_algo != "blake3" {
        return Err(anyhow::anyhow!(FtsError::Version(format!(
            "不支持的校验算法: {}",
            manifest.checksum_algo
        ))));
    }
    let chunk_count = manifest.chunk_hashes.len();

    // 自检:manifest 自带的 root 必须与其分块哈希表一致(防篡改/截断)
    let hashes: Vec<[u8; 32]> = manifest
        .chunk_hashes
        .iter()
        .map(|h| blake3::Hash::from_hex(h).map(|x| *x.as_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!(FtsError::Checksum(format!("manifest 分块哈希解析失败: {e}"))))?;
    if merkle_root(&hashes) != manifest.merkle_root {
        return Err(anyhow::anyhow!(FtsError::Checksum(
            "manifest merkle_root 与分块哈希表不一致".into()
        )));
    }

    // W1:ticket → 落盘目录;拒绝时回 ManifestAck{ok:false, reason="AUTH: ..."}(发送端转退出码 7)
    async fn deny_ack(ctrl_tx: &mut BoxWrite, reason: String) -> anyhow::Error {
        // 尽力通知对端(发送失败不影响本端错误上报)
        let payload = serde_json::to_vec(&ManifestAck {
            ok: false,
            reason: Some(format!("AUTH: {reason}")),
            resume_bitmap_hex: None,
        })
        .unwrap_or_default();
        let _ = write_frame(ctrl_tx, &payload).await;
        anyhow::anyhow!(FtsError::Auth(reason))
    }
    let base_dir: PathBuf = match &manifest.ticket {
        Some(ticket) => {
            let Some(reg) = &cfg.inbox else {
                return Err(deny_ack(ctrl_tx, "本端未启用 inbox,ticket 无法兑换".into()).await);
            };
            match reg.redeem(ticket, &manifest.file_name) {
                Ok(dir) => dir,
                Err(r) => {
                    return Err(deny_ack(ctrl_tx, format!("ticket 拒绝: {r}")).await);
                }
            }
        }
        None => cfg.out_dir.clone(),
    };

    // W8:file_name 现在是 '/' 分隔的相对路径(递归传输);校验(信任边界:防路径穿越)
    // 并重建父目录。file_name 同时用作 checkpoint 匹配键与文件事件名。
    let rel = sanitize_rel_path(&manifest.file_name)
        .map_err(|e| anyhow::anyhow!(FtsError::Checksum(format!("manifest 路径非法: {e}"))))?;
    let final_path = base_dir.join(&rel);
    let parent = final_path
        .parent()
        .context("落盘路径缺少父目录")?
        .to_path_buf();
    std::fs::create_dir_all(&parent).map_err(crate::error::FtsError::io_with_hint)?;

    // 续传:检查已有 checkpoint 与 manifest 是否匹配
    let tmp_path = {
        let mut os = final_path.clone().into_os_string();
        os.push(".fts-part");
        PathBuf::from(os)
    };
    let meta_path = {
        let mut os = tmp_path.clone().into_os_string();
        os.push(".meta");
        PathBuf::from(os)
    };
    let mut bitmap = Bitmap::new(chunk_count);
    let mut resumed = false;
    if tmp_path.exists() {
        if let Some(cp) = std::fs::read(&meta_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Checkpoint>(&b).ok())
        {
            if cp.file_name == manifest.file_name
                && cp.file_size == manifest.file_size
                && cp.chunk_size == manifest.chunk_size
                && cp.merkle_root == manifest.merkle_root
            {
                bitmap = Bitmap::from_hex(&cp.bitmap_hex, chunk_count);
                resumed = true;
            }
        }
        if !resumed {
            let _ = tokio::fs::remove_file(&tmp_path).await; // manifest 变了,旧半成品作废
        }
    }
    tokio::fs::File::create(&tmp_path)
        .await?
        .set_len(manifest.file_size)
        .await?; // 预分配,块可乱序写
    let ack = ManifestAck {
        ok: true,
        reason: None,
        resume_bitmap_hex: if resumed { Some(bitmap.to_hex()) } else { None },
    };
    out.log(&format!(
        "recv: {} ({} 字节, {} 块 × {}){}",
        manifest.file_name,
        manifest.file_size,
        chunk_count,
        manifest.chunk_size,
        if resumed { " [续传]" } else { "" }
    ));
    write_frame(ctrl_tx, &serde_json::to_vec(&ack)?).await?;
    out.reset_started();

    let shared = Arc::new(Shared {
        manifest: manifest.clone(),
        bitmap: Mutex::new(bitmap),
        verified_bytes: AtomicU64::new(0),
        tmp_path: tmp_path.clone(),
        meta_path: meta_path.clone(),
    });
    let mut tasks: JoinSet<Result<()>> = JoinSet::new();
    let done_flag = Arc::new(AtomicU64::new(0));

    // 进度 + checkpoint 线程:每秒报验证进度并落盘 checkpoint
    // (checkpoint 周期性写而非仅失败时写 —— 崩溃/断电/被杀都能续传)
    {
        let shared = shared.clone();
        let done_flag = done_flag.clone();
        let total = shared.manifest.file_size;
        let mut out2 = SessionOut::new(cfg.out);
        if let Some(tx) = &cfg.events {
            out2 = out2.with_events(tx.clone());
        }
        out2.task_id = out.task_id.clone();
        out2.started = out.started;
        tokio::spawn(async move {
            let mut last: u64 = 0;
            while done_flag.load(Ordering::Relaxed) == 0 {
                let now = shared.verified_bytes.load(Ordering::Relaxed);
                if now != last {
                    out2.progress(Progress { bytes_done: now, bytes_total: total });
                    let bitmap = shared.bitmap.lock().await.clone();
                    let cp = Checkpoint {
                        file_name: shared.manifest.file_name.clone(),
                        file_size: shared.manifest.file_size,
                        chunk_size: shared.manifest.chunk_size,
                        merkle_root: shared.manifest.merkle_root.clone(),
                        bitmap_hex: bitmap.to_hex(),
                    };
                    let _ = tokio::fs::write(&shared.meta_path, serde_json::to_vec(&cp).unwrap_or_default())
                        .await;
                }
                last = now;
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
            let now = shared.verified_bytes.load(Ordering::Relaxed);
            if now != last {
                out2.progress(Progress { bytes_done: now, bytes_total: total });
            }
        });
    }

    let finish_received = loop {
        tokio::select! {
            stream = data_src.next_stream() => {
                match stream {
                    Ok(s) => {
                        let shared = shared.clone();
                        tasks.spawn(async move { recv_stream(s, shared).await });
                    }
                    Err(e) => {
                        done_flag.store(1, Ordering::Relaxed);
                        let _ = tasks.join_next().await;
                        return Err(anyhow::anyhow!(FtsError::Interrupted(format!(
                            "数据流接受失败: {e}"
                        ))));
                    }
                }
            }
            msg = finish_rx.recv() => {
                match msg {
                    Some(Ok(_)) => break true,
                    Some(Err(e)) => {
                        done_flag.store(1, Ordering::Relaxed);
                        return Err(e);
                    }
                    None => {
                        done_flag.store(1, Ordering::Relaxed);
                        return Err(anyhow::anyhow!(FtsError::Interrupted(
                            "控制流任务意外结束".into()
                        )));
                    }
                }
            }
        }
    };
    done_flag.store(1, Ordering::Relaxed);

    // Finish 后补收:发送端先开满 N 条流、写完才发 Finish,但跨流无到达顺序保证,
    // 上面的 select 可能先命中 Finish,把 accept 队列里尚未 accept 的流遗弃
    // (实测:极小文件整个传输微秒级完成时触发"Finish 后仍有 1 块未收到")。
    // 以"立即就绪"为准补收队列中的流,零延迟;真正在途的流本就等不到,交给下方位图校验兜底。
    while let Ok(stream) = tokio::time::timeout(Duration::ZERO, data_src.next_stream()).await {
        match stream {
            Ok(s) => {
                let shared = shared.clone();
                tasks.spawn(async move { recv_stream(s, shared).await });
            }
            Err(_) => break, // 连接已关闭:不再有新流
        }
    }

    // 等所有流任务收尾
    let mut first_err: Option<anyhow::Error> = None;
    while let Some(r) = tasks.join_next().await {
        match r.context("接收任务 panic")? {
            Ok(()) => {}
            Err(e) if first_err.is_none() => first_err = Some(e),
            Err(_) => {}
        }
    }

    let bitmap = shared.bitmap.lock().await.clone();
    let verified = shared.verified_bytes.load(Ordering::Relaxed);

    if !finish_received || first_err.is_some() {
        // 中断:保存 checkpoint 供续传
        let cp = Checkpoint {
            file_name: shared.manifest.file_name.clone(),
            file_size: shared.manifest.file_size,
            chunk_size: shared.manifest.chunk_size,
            merkle_root: shared.manifest.merkle_root.clone(),
            bitmap_hex: bitmap.to_hex(),
        };
        tokio::fs::write(&shared.meta_path, serde_json::to_vec(&cp)?).await?;
        if let Some(e) = first_err {
            return Err(e);
        }
        return Err(anyhow::anyhow!(FtsError::Interrupted(format!(
            "连接中断:已验证 {verified}/{} 字节;重发同一文件将自动续传",
            shared.manifest.file_size
        ))));
    }

    if !bitmap.is_complete() {
        return Err(anyhow::anyhow!(FtsError::Checksum(format!(
            "Finish 后仍有 {} 块未收到",
            chunk_count as u64 - bitmap.count()
        ))));
    }

    // 文件级校验:所有块哈希已在到达时逐一验证(含续传前已验证的块),
    // 文件级完整性 = manifest root 与分块哈希表的一致性(前面已自检)。
    let elapsed = out.started.elapsed().as_secs_f64();
    tokio::fs::rename(&tmp_path, &final_path).await?;
    let _ = tokio::fs::remove_file(&meta_path).await;
    out.log(&format!(
        "recv: 完成 {} — BLAKE3 逐块校验通过(root={}),耗时 {elapsed:.2}s,平均 {:.1} MB/s",
        shared.manifest.file_name,
        &shared.manifest.merkle_root[..16],
        shared.manifest.file_size as f64 / elapsed / 1_048_576.0
    ));

    write_frame(
        ctrl_tx,
        &serde_json::to_vec(&FinishAck {
            checksum_ok: true,
            error: None,
        })?,
    )
    .await?;
    Ok(())
}

struct Shared {
    manifest: Manifest,
    bitmap: Mutex<Bitmap>,
    verified_bytes: AtomicU64,
    tmp_path: PathBuf,
    meta_path: PathBuf,
}

async fn recv_stream(mut s: BoxRead, shared: Arc<Shared>) -> Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&shared.tmp_path)
        .await?;
    let chunk_size = shared.manifest.chunk_size as u64;
    let total_chunks = shared.manifest.chunk_hashes.len();
    loop {
        // 帧头 [chunk_id:u32][offset:u64][len:u32];读到 EOF(FIN)= 本流正常结束
        let mut hdr = [0u8; CHUNK_HEADER];
        if let Err(e) = s.read_exact(&mut hdr).await {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                return Ok(());
            }
            return Err(anyhow::anyhow!(FtsError::Interrupted(format!(
                "读分块帧头失败: {e}"
            ))));
        }
        let chunk_id = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let offset = u64::from_le_bytes(hdr[4..12].try_into().unwrap());
        let len = u32::from_le_bytes(hdr[12..16].try_into().unwrap());

        if len as u64 > chunk_size {
            return Err(anyhow::anyhow!(FtsError::Checksum(format!(
                "块 {chunk_id} 长度 {len} 超过协商块大小"
            ))));
        }
        if chunk_id as usize >= total_chunks {
            return Err(anyhow::anyhow!(FtsError::Checksum(format!(
                "块编号 {chunk_id} 超出 manifest"
            ))));
        }
        let mut payload = vec![0u8; len as usize];
        if let Err(e) = s.read_exact(&mut payload).await {
            return Err(anyhow::anyhow!(FtsError::Interrupted(format!(
                "读分块载荷失败: {e}"
            ))));
        }

        // 逐块校验(不可关闭 —— 硬约束)
        let expect = &shared.manifest.chunk_hashes[chunk_id as usize];
        let actual = blake3::hash(&payload).to_hex();
        if actual.as_str() != expect {
            return Err(anyhow::anyhow!(FtsError::Checksum(format!(
                "块 {chunk_id} 哈希不匹配 expect={} actual={actual}",
                &expect[..16]
            ))));
        }

        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.write_all(&payload).await?;
        {
            let mut bm = shared.bitmap.lock().await;
            if !bm.get(chunk_id as usize) {
                bm.set(chunk_id as usize);
                shared
                    .verified_bytes
                    .fetch_add(len as u64, Ordering::Relaxed);
            }
        }
    }
}
