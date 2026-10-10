//! 发送端(P5 W2):多文件单连接 —— 握手一次,逐文件 Manifest→chunks→Finish→FinishAck。
//! QUIC 优先,UDP 不可达自动降级 TCP(方案 A,design 9.5;`--transport` 可强制)。
//! W1:每个 Manifest 携带可选 ticket(inbox 预注册)。
//! v2(fts-more-fast):QUIC + 对端 MULTIFILE → 文件级并发流水线:
//!   hash-ahead 后台预读哈希(藏进前一文件的传输窗口)+ K 个文件管道任务(槽位化)。

use crate::error::FtsError;
use crate::protocol::*;
use crate::tls::PinMode;
use crate::ui::{Output, Progress, SessionOut};
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;
pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;

pub struct SendCfg {
    pub paths: Vec<PathBuf>,
    pub to: String,
    pub chunk_size: u32,
    pub streams: u32,
    pub pin: PinMode,
    pub transport: String, // auto | quic | tcp
    /// W1 inbox 票据(目录模式或单文件绑定)
    pub ticket: Option<String>,
    pub out: Output,
    /// daemon 模式事件通道(REST/gRPC SSE)
    pub events: Option<tokio::sync::broadcast::Sender<String>>,
    /// v2 文件级并发:0 = auto(4);生效值 = min(请求, 文件数),仅 Concurrent 模式 >1
    pub file_concurrency: u32,
}

/// 作业级计数(progress 事件的 job 附加字段)
struct JobCounters {
    bytes_total: u64,
    files_total: usize,
    bytes_done: Arc<AtomicU64>,
    files_done: Arc<AtomicU64>,
}

/// hash-ahead 预读产物:文件元数据 + 全部分块哈希(已含 root)
struct Prepared {
    index: usize,
    path: PathBuf,
    name: String,
    file_size: u64,
    hashes: Vec<[u8; 32]>,
    root: String,
}

/// 数据流来源:QUIC 端点上再开流,或 TCP 新连接(与控制连接同地址)。
pub enum DataOpener {
    Quic(quinn::Connection),
    Tcp(SocketAddr),
}

impl DataOpener {
    async fn open(&self) -> Result<BoxWrite> {
        match self {
            DataOpener::Quic(conn) => {
                let s = conn.open_uni().await.map_err(|e| {
                    anyhow::anyhow!(FtsError::Interrupted(format!("打开数据流失败: {e}")))
                })?;
                Ok(Box::new(s))
            }
            DataOpener::Tcp(addr) => {
                let s = tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::net::TcpStream::connect(*addr),
                )
                .await
                .map_err(|_| anyhow::anyhow!(FtsError::Connect("TCP 数据流连接超时".into())))?
                .map_err(FtsError::io)?;
                let (_, w) = s.into_split();
                Ok(Box::new(w))
            }
        }
    }
}

fn clone_opener(opener: &DataOpener) -> DataOpener {
    match opener {
        DataOpener::Quic(c) => DataOpener::Quic(c.clone()),
        DataOpener::Tcp(a) => DataOpener::Tcp(*a),
    }
}

/// 建立会话;返回 (数据流来源, 控制流读写端, 是否走了 TCP 回退)
async fn connect_session(
    cfg: &SendCfg,
    addr: SocketAddr,
    out: &SessionOut,
) -> Result<(DataOpener, BoxWrite, BoxRead, bool)> {
    let want_tcp = cfg.transport == "tcp";
    if !want_tcp {
        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
        endpoint.set_default_client_config(crate::tls::client_config(cfg.pin.clone())?);
        let connect = async {
            let conn = endpoint
                .connect(addr, "fts.local")
                .map_err(|e| FtsError::Connect(e.to_string()))?
                .await
                .map_err(crate::tls::map_conn_err)?;
            Ok::<_, anyhow::Error>(conn)
        };
        match tokio::time::timeout(Duration::from_secs(4), connect).await {
            Ok(Ok(conn)) => {
                let (tx, rx) = conn.open_bi().await.map_err(|e| {
                    anyhow::anyhow!(FtsError::Connect(format!("打开控制流失败: {e}")))
                })?;
                return Ok((DataOpener::Quic(conn), Box::new(tx), Box::new(rx), false));
            }
            Ok(Err(e))
                if matches!(
                    e.downcast_ref::<FtsError>(),
                    Some(FtsError::Auth(_)) | Some(FtsError::Version(_))
                ) =>
            {
                // 认证/版本错误降级无意义,直接失败
                return Err(e);
            }
            Ok(Err(_)) | Err(_) => {
                if cfg.transport == "quic" {
                    return Err(anyhow::anyhow!(FtsError::Connect(
                        "QUIC 连接失败(transport=quic,不回退)".into()
                    )));
                }
                out.out.log_degradation("L0", "L4", "quic_unreachable_tcp_fallback");
                // 继续走 TCP
            }
        }
    }
    let s = tokio::time::timeout(Duration::from_secs(10), tokio::net::TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow::anyhow!(FtsError::Connect("TCP 连接超时".into())))?
        .map_err(FtsError::io)?;
    let (rx, tx) = s.into_split();
    Ok((DataOpener::Tcp(addr), Box::new(tx), Box::new(rx), true))
}

/// 多路径展开(W8):目录**递归**展开,保留相对路径结构('/' 分隔,协议内统一用 '/');
/// 目录参数的相对前缀 = 其 basename(即"整个文件夹"作为一棵子树落盘)。
/// 重复的相对路径即错;空目录不产生条目(接收端不会重建空目录);目录符号链接不递归(防环)。
fn expand_paths(paths: &[PathBuf]) -> Result<Vec<(PathBuf, String)>> {
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    for p in paths {
        if p.is_dir() {
            let root_name = p
                .file_name()
                .context("目录路径缺少末段名(不能是盘根)")?
                .to_string_lossy()
                .to_string();
            walk_dir(p, &root_name, &mut files)
                .with_context(|| format!("遍历目录失败: {}", p.display()))?;
        } else {
            let name = p
                .file_name()
                .context("路径缺少文件名")?
                .to_string_lossy()
                .to_string();
            files.push((p.clone(), name));
        }
    }
    let mut seen = HashSet::new();
    for (_, n) in &files {
        if !seen.insert(n.clone()) {
            return Err(anyhow::anyhow!(FtsError::InvalidArg(format!(
                "重名文件: {n}(多路径展开后相对路径不得重复,接收端将按此路径落盘)"
            ))));
        }
    }
    if files.is_empty() {
        return Err(anyhow::anyhow!(FtsError::InvalidArg(
            "没有可发送的文件(目录为空,或只含空子目录;空目录不会被传输)".into()
        )));
    }
    Ok(files)
}

fn walk_dir(dir: &Path, rel_prefix: &str, out: &mut Vec<(PathBuf, String)>) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let ft = e.file_type()?;
        let name = e.file_name().to_string_lossy().to_string();
        let child_rel = format!("{rel_prefix}/{name}");
        if ft.is_dir() {
            walk_dir(&e.path(), &child_rel, out)?;
        } else if ft.is_file() {
            out.push((e.path(), child_rel));
        }
        // 符号链接(is_dir/is_file 均为 false):跳过,防环
    }
    Ok(())
}

pub async fn run_send(cfg: SendCfg) -> Result<(), FtsError> {
    run_send_inner(cfg)
        .await
        .map_err(|e| match e.downcast::<FtsError>() {
            Ok(fts_err) => fts_err,
            Err(other) => FtsError::Io(format!("{other:#}")),
        })
}

async fn run_send_inner(cfg: SendCfg) -> Result<()> {
    let out = SessionOut::new(cfg.out);
    let mut out = if let Some(tx) = &cfg.events {
        out.clone_with_events(tx.clone())
    } else {
        out
    };
    out.task_created();

    let result = send_all(&cfg, &mut out).await;
    match &result {
        Ok(()) => out.result("COMPLETED", true, None),
        Err(e) => {
            let fts_err = e
                .downcast_ref::<FtsError>()
                .cloned()
                .unwrap_or_else(|| FtsError::Io(format!("{e:#}")));
            out.result("FAILED", false, Some(&fts_err));
        }
    }
    result
}

async fn send_all(cfg: &SendCfg, out: &mut SessionOut) -> Result<()> {
    let files = expand_paths(&cfg.paths)?;
    let total = files.len();

    // 作业级总量(每文件一次 metadata,远小于后续哈希读)
    let mut bytes_total = 0u64;
    for (p, name) in &files {
        let len = tokio::fs::metadata(p)
            .await
            .with_context(|| format!("读取文件失败: {} ({name})", p.display()))?
            .len();
        bytes_total += len;
    }
    let job = Arc::new(JobCounters {
        bytes_total,
        files_total: total,
        bytes_done: Arc::new(AtomicU64::new(0)),
        files_done: Arc::new(AtomicU64::new(0)),
    });

    let addr: SocketAddr = cfg
        .to
        .to_socket_addrs()
        .map_err(|e| FtsError::Io(e.to_string()))?
        .next()
        .ok_or_else(|| FtsError::Io("目标地址解析失败".into()))?;

    let (opener, mut ctl_tx, mut ctl_rx, degraded) = connect_session(cfg, addr, out).await?;
    out.log(&format!(
        "send: 已连接 {addr},{} 个文件{}",
        total,
        if degraded { " [TCP 回退]" } else { "" }
    ));

    // 路径探测(观测):异步执行不阻塞传输
    {
        let out_probe = cfg.out;
        let ip = addr.ip().to_string();
        tokio::spawn(async move {
            let (link, rtt) = crate::probe::probe_peer(&ip).await;
            if link != crate::probe::LinkType::Unknown {
                out_probe.path(link.as_str(), rtt);
            }
        });
    }

    // 控制流握手一次(本端宣告 MULTIFILE;模式由两端各自按 transport+caps 推导)
    write_frame(
        &mut ctl_tx,
        &serde_json::to_vec(&Hello {
            protocol_version: PROTOCOL_VERSION,
            fts_version: env!("CARGO_PKG_VERSION").into(),
            role: "source".into(),
            caps: CAP_MULTIFILE,
        })?,
    )
    .await?;
    let peer: Hello = parse_json(&read_frame(&mut ctl_rx, 4096).await?)?;
    check_hello(&peer)?;

    let mode = negotiate_mode(!degraded, peer.caps);
    let want = if cfg.file_concurrency == 0 {
        DEFAULT_FILE_CONCURRENCY
    } else {
        cfg.file_concurrency.clamp(1, 16)
    };
    let k = match mode {
        TransferMode::Concurrent => want.min(total as u32).max(1) as usize,
        TransferMode::Sequential => 1,
    };
    if k > 1 {
        out.log(&format!("send: 文件级并发 K={k}"));
    }
    let slot_of = |s: usize| match mode {
        TransferMode::Concurrent => Some(s as u32),
        TransferMode::Sequential => None,
    };

    // hash-ahead 生产者:顺序预读+哈希,藏进前一文件的传输窗口;容量 K+1 保持领先一步
    let (prep_tx, prep_rx) = tokio::sync::mpsc::channel::<Result<Prepared>>(k + 1);
    let producer = {
        let chunk_size = cfg.chunk_size;
        tokio::spawn(async move {
            for (i, (path, name)) in files.into_iter().enumerate() {
                let item = prepare_file(&path, &name, i, chunk_size).await;
                let is_err = item.is_err();
                if prep_tx.send(item).await.is_err() {
                    break; // worker 全部退出
                }
                if is_err {
                    break; // 预读失败:会话将快失败,无需继续
                }
            }
        })
    };

    // 控制读 router:ManifestAck/FinishAck 按 slot 路由到各 worker(None → 0,Sequential)
    let mut ack_txs = Vec::new();
    let mut finish_txs = Vec::new();
    let mut ack_rxs = Vec::new();
    let mut finish_rxs = Vec::new();
    for _ in 0..k {
        let (a, ar) = tokio::sync::mpsc::unbounded_channel::<ManifestAck>();
        let (f, fr) = tokio::sync::mpsc::unbounded_channel::<FinishAck>();
        ack_txs.push(a);
        finish_txs.push(f);
        ack_rxs.push(ar);
        finish_rxs.push(fr);
    }
    let router = {
        tokio::spawn(async move {
            let mut ctl_rx = ctl_rx;
            loop {
                match read_frame(&mut ctl_rx, 1024 * 1024).await {
                    Ok(bytes) => {
                        if let Ok(a) = serde_json::from_slice::<ManifestAck>(&bytes) {
                            let idx = a.slot.map(|s| s as usize).unwrap_or(0);
                            let Some(tx) = ack_txs.get(idx) else {
                                break; // 未知槽位:协议错,worker 将因通道关闭失败
                            };
                            if tx.send(a).is_err() {
                                break;
                            }
                        } else if let Ok(f) = serde_json::from_slice::<FinishAck>(&bytes) {
                            let idx = f.slot.map(|s| s as usize).unwrap_or(0);
                            let Some(tx) = finish_txs.get(idx) else {
                                break;
                            };
                            if tx.send(f).is_err() {
                                break;
                            }
                        } else {
                            break; // 无法识别的帧:会话失败(worker 通道关闭即报错)
                        }
                    }
                    Err(_) => break, // EOF/断开:所有 worker 的通道随之关闭
                }
            }
        })
    };

    // K 个文件管道 worker(共享 prep 队列;worker s 固定占槽 s)
    let ctl = Arc::new(Mutex::new(ctl_tx));
    let prep_rx = Arc::new(Mutex::new(prep_rx));
    let mut set: JoinSet<Result<()>> = JoinSet::new();
    for s in 0..k {
        let cfg_fields = CfgRef {
            chunk_size: cfg.chunk_size,
            streams: cfg.streams,
            ticket: cfg.ticket.clone(),
            out: cfg.out,
            events: cfg.events.clone(),
        };
        let opener = clone_opener(&opener);
        let ctl = ctl.clone();
        let prep_rx = prep_rx.clone();
        let ack_rx = ack_rxs.remove(0);
        let finish_rx = finish_rxs.remove(0);
        let job = job.clone();
        let worker_out = out.clone();
        let slot = slot_of(s);
        set.spawn(async move {
            worker(
                cfg_fields,
                opener,
                ctl,
                prep_rx,
                ack_rx,
                finish_rx,
                slot,
                job,
                worker_out,
            )
            .await
        });
    }

    // 快失败收割:任一 worker 失败即中止其余(与接收端快失败对齐)
    let mut first_err: Option<anyhow::Error> = None;
    while let Some(r) = set.join_next().await {
        match r.context("管道任务 panic")? {
            Ok(()) => {}
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                    set.abort_all();
                }
            }
        }
    }
    producer.abort();
    router.abort();
    if let Some(e) = first_err {
        return Err(e);
    }
    // 全部完成:显式关 QUIC 连接 —— 接收端 gate 在等这条 close;
    // 仅靠 drop 在自传(daemon 提交给自己)场景不保证 close 帧发出,会拖死会话 permit。
    if let DataOpener::Quic(conn) = &opener {
        conn.close(0u32.into(), b"done");
    }
    Ok(())
}

/// worker 用到的 cfg 只读字段(SendCfg 不可 Clone,收窄传参)
struct CfgRef {
    chunk_size: u32,
    streams: u32,
    ticket: Option<String>,
    out: Output,
    events: Option<tokio::sync::broadcast::Sender<String>>,
}

/// 管道 worker:取预读产物 → Manifest{slot} → Ack(续传位图)→ N 流 → Finish{slot} → FinishAck{slot}
#[allow(clippy::too_many_arguments)]
async fn worker(
    cfg: CfgRef,
    opener: DataOpener,
    ctl: Arc<Mutex<BoxWrite>>,
    prep_rx: Arc<Mutex<tokio::sync::mpsc::Receiver<Result<Prepared>>>>,
    ack_rx: tokio::sync::mpsc::UnboundedReceiver<ManifestAck>,
    finish_rx: tokio::sync::mpsc::UnboundedReceiver<FinishAck>,
    slot: Option<u32>,
    job: Arc<JobCounters>,
    mut out: SessionOut,
) -> Result<()> {
    let total = job.files_total;
    let mut ack_rx = ack_rx;
    let mut finish_rx = finish_rx;
    loop {
        let prep = {
            let mut rx = prep_rx.lock().await;
            rx.recv().await
        };
        let Some(prep) = prep else { break Ok(()) }; // 队列关闭 = 全部文件已分发
        let prep = match prep {
            Ok(p) => p,
            Err(e) => break Err(e), // 预读失败:快失败
        };
        out.file(&prep.name, prep.index + 1, total);
        let started = std::time::Instant::now();
        match transfer_prepared(&cfg, &opener, &ctl, &mut ack_rx, &mut finish_rx, &prep, slot, &job, &mut out)
            .await
        {
            Ok(bytes) => {
                job.files_done.fetch_add(1, Ordering::Relaxed);
                out.file_done(&prep.name, true, started.elapsed().as_secs_f64(), bytes);
            }
            Err(e) => {
                out.file_done(&prep.name, false, started.elapsed().as_secs_f64(), 0);
                break Err(e.context(format!("文件 {} 传输失败", prep.name)));
            }
        }
    }
}

/// 预读+哈希一个文件(hash-ahead 生产者主体;必须读满一块才切分块,tokio read 允许短读)
async fn prepare_file(path: &Path, name: &str, index: usize, chunk_size: u32) -> Result<Prepared> {
    let file_size = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("读取文件失败: {}", path.display()))?
        .len();
    let chunk_count = file_size.div_ceil(chunk_size as u64) as usize;
    let mut hashes = Vec::with_capacity(chunk_count);
    let mut f = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; chunk_size as usize];
    loop {
        let mut filled = 0usize;
        loop {
            match f.read(&mut buf[filled..]).await? {
                0 => break,
                n => filled += n,
            }
            if filled == buf.len() {
                break;
            }
        }
        if filled == 0 {
            break;
        }
        hashes.push(*blake3::hash(&buf[..filled]).as_bytes());
        if filled < buf.len() {
            break;
        }
    }
    hashes.truncate(chunk_count);
    Ok(Prepared {
        index,
        path: path.to_path_buf(),
        name: name.to_string(),
        file_size,
        root: merkle_root(&hashes),
        hashes,
    })
}

/// 传一个已预读的文件:Manifest{slot} → Ack(可含续传位图)→ N 流并行(各写槽位前导)→ Finish{slot} → FinishAck{slot}
#[allow(clippy::too_many_arguments)]
async fn transfer_prepared(
    cfg: &CfgRef,
    opener: &DataOpener,
    ctl: &Arc<Mutex<BoxWrite>>,
    ack_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ManifestAck>,
    finish_rx: &mut tokio::sync::mpsc::UnboundedReceiver<FinishAck>,
    prep: &Prepared,
    slot: Option<u32>,
    job: &Arc<JobCounters>,
    out: &mut SessionOut,
) -> Result<u64> {
    let file_size = prep.file_size;
    let chunk_count = prep.hashes.len();

    let manifest = Manifest {
        file_name: prep.name.clone(),
        file_size,
        chunk_size: cfg.chunk_size,
        checksum_algo: "blake3".into(),
        chunk_hashes: prep
            .hashes
            .iter()
            .map(|h| blake3::Hash::from(*h).to_hex().to_string())
            .collect(),
        merkle_root: prep.root.clone(),
        ticket: cfg.ticket.clone(),
        slot,
    };

    write_frame(&mut *ctl.lock().await, &serde_json::to_vec(&manifest.clone())?).await?;
    let ack = ack_rx
        .recv()
        .await
        .ok_or_else(|| anyhow::anyhow!(FtsError::Interrupted("等待 ManifestAck 失败: 控制流已关闭".into())))?;
    if !ack.ok {
        let reason = ack.reason.unwrap_or_else(|| "对端拒绝 manifest".into());
        // 接收端以 "AUTH: " 前缀标记授权类拒绝(ticket 无效等)→ 退出码 7
        return Err(anyhow::anyhow!(if let Some(r) = reason.strip_prefix("AUTH: ") {
            FtsError::Auth(r.to_string())
        } else {
            FtsError::Connect(reason)
        }));
    }
    out.reset_started();

    let resume: Vec<bool> = ack
        .resume_bitmap_hex
        .map(|hex| Bitmap::from_hex(&hex, chunk_count).bits)
        .unwrap_or_else(|| vec![false; chunk_count]);
    let todo: Vec<usize> = (0..chunk_count).filter(|&i| !resume[i]).collect();
    let todo_count = todo.len();
    if resume.iter().any(|b| *b) {
        out.log(&format!(
            "send: {} 续传,已完成 {} 块,需传 {} 块",
            prep.name,
            chunk_count - todo_count,
            todo_count
        ));
    }
    if todo_count == 0 {
        // 文件已完整(零字节文件天然无块 / 续传时全部块已验证):
        // 仍必须走 Finish→FinishAck 握手 —— 接收端在等 Finish 才会做落盘收尾,
        // 跳过会让对端永远卡在 finish_rx.recv()(死锁)。
        out.log(&format!("send: {} 已完整(无需传输数据块)", prep.name));
    }

    // N 条流并行发送,共享待传块栈;job.bytes_done 同步累计(作业级进度)
    let todo = Arc::new(Mutex::new(todo));
    let sent_bytes = Arc::new(AtomicU64::new(0));
    let done_flag = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();
    let n_streams = cfg.streams.clamp(1, 64);
    for _ in 0..n_streams {
        let opener = clone_opener(opener);
        let todo = todo.clone();
        let path = prep.path.clone();
        let chunk_size = cfg.chunk_size;
        let sent_bytes = sent_bytes.clone();
        let job_bytes = job.bytes_done.clone();
        let preamble = slot.map(slot_preamble);
        tasks.push(tokio::spawn(async move {
            let mut stream: BoxWrite = opener.open().await?;
            if let Some(pre) = preamble {
                stream.write_all(&pre).await?;
            }
            let mut file = tokio::fs::File::open(&path).await?;
            let mut buf = vec![0u8; chunk_size as usize];
            loop {
                let idx = { todo.lock().await.pop() };
                let Some(idx) = idx else { break };
                let offset = idx as u64 * chunk_size as u64;
                let expect = (file_size - offset).min(chunk_size as u64) as usize;
                file.seek(std::io::SeekFrom::Start(offset)).await?;
                file.read_exact(&mut buf[..expect]).await?;
                stream.write_all(&(idx as u32).to_le_bytes()).await?;
                stream.write_all(&offset.to_le_bytes()).await?;
                stream.write_all(&(expect as u32).to_le_bytes()).await?;
                stream.write_all(&buf[..expect]).await?;
                sent_bytes.fetch_add(expect as u64, Ordering::Relaxed);
                job_bytes.fetch_add(expect as u64, Ordering::Relaxed);
            }
            stream.shutdown().await?;
            Ok::<(), anyhow::Error>(())
        }));
    }

    // 进度线程:每秒报当前文件进度 + 作业级聚合(job 附加字段)
    {
        let sent_bytes = sent_bytes.clone();
        let done_flag = done_flag.clone();
        let total = file_size;
        let mut out2 = SessionOut::new(cfg.out);
        if let Some(tx) = &cfg.events {
            out2 = out2.with_events(tx.clone());
        }
        out2.task_id = out.task_id.clone();
        out2.started = out.started;
        let job = job.clone();
        tokio::spawn(async move {
            let mut last: u64 = 0;
            while done_flag.load(Ordering::Relaxed) == 0 {
                let now = sent_bytes.load(Ordering::Relaxed);
                if now != last {
                    out2.progress(Progress {
                        bytes_done: now,
                        bytes_total: total,
                        job: Some(crate::ui::JobProgress {
                            bytes_done: job.bytes_done.load(Ordering::Relaxed),
                            bytes_total: job.bytes_total,
                            files_done: job.files_done.load(Ordering::Relaxed),
                            files_total: job.files_total as u64,
                        }),
                    });
                }
                last = now;
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
            let now = sent_bytes.load(Ordering::Relaxed);
            if now != last {
                out2.progress(Progress {
                    bytes_done: now,
                    bytes_total: total,
                    job: Some(crate::ui::JobProgress {
                        bytes_done: job.bytes_done.load(Ordering::Relaxed),
                        bytes_total: job.bytes_total,
                        files_done: job.files_done.load(Ordering::Relaxed),
                        files_total: job.files_total as u64,
                    }),
                });
            }
        });
    }

    let mut first_err: Option<anyhow::Error> = None;
    for t in tasks {
        if let Err(e) = t.await.context("发送任务 panic")? {
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }

    // Finish → FinishAck
    write_frame(
        &mut *ctl.lock().await,
        &serde_json::to_vec(&Finish { slot })?,
    )
    .await?;
    let finish_ack = finish_rx.recv().await.ok_or_else(|| {
        anyhow::anyhow!(FtsError::Interrupted("等待对端校验结果失败: 控制流已关闭".into()))
    })?;
    done_flag.store(1, Ordering::Relaxed);

    if !finish_ack.checksum_ok {
        return Err(anyhow::anyhow!(FtsError::Checksum(
            finish_ack.error.unwrap_or_else(|| "对端校验失败".into())
        )));
    }

    let elapsed = out.started.elapsed().as_secs_f64();
    out.log(&format!(
        "send: {} 完成,{} 块,耗时 {elapsed:.2}s,平均 {:.1} MB/s",
        prep.name,
        chunk_count,
        file_size as f64 / elapsed / 1_048_576.0
    ));
    Ok(file_size)
}
