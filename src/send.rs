//! 发送端(P5 W2):多文件单连接 —— 握手一次,逐文件 Manifest→chunks→Finish→FinishAck。
//! QUIC 优先,UDP 不可达自动降级 TCP(方案 A,design 9.5;`--transport` 可强制)。
//! W1:每个 Manifest 携带可选 ticket(inbox 预注册)。

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

    // 控制流握手一次
    write_frame(
        &mut ctl_tx,
        &serde_json::to_vec(&Hello {
            protocol_version: PROTOCOL_VERSION,
            fts_version: env!("CARGO_PKG_VERSION").into(),
            role: "source".into(),
        })?,
    )
    .await?;
    let peer: Hello = parse_json(&read_frame(&mut ctl_rx, 4096).await?)?;
    check_hello(&peer)?;

    // 逐文件传输;任一文件失败即中止(快失败)
    for (i, (path, name)) in files.iter().enumerate() {
        out.file(name, i + 1, total);
        let started = std::time::Instant::now();
        match transfer_one_file(cfg, &opener, &mut ctl_tx, &mut ctl_rx, path, name, out).await {
            Ok(bytes) => {
                out.file_done(name, true, started.elapsed().as_secs_f64(), bytes);
            }
            Err(e) => {
                out.file_done(name, false, started.elapsed().as_secs_f64(), 0);
                return Err(e.context(format!("文件 {name} 传输失败")));
            }
        }
    }
    Ok(())
}

/// 单文件:哈希预计算 → Manifest(含 ticket)→ Ack(可含续传位图)→ N 流并行 → Finish → FinishAck。
/// 返回文件字节数。
async fn transfer_one_file(
    cfg: &SendCfg,
    opener: &DataOpener,
    ctl_tx: &mut BoxWrite,
    ctl_rx: &mut BoxRead,
    path: &Path,
    file_name: &str,
    out: &mut SessionOut,
) -> Result<u64> {
    let file_size = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("读取文件失败: {}", path.display()))?
        .len();
    let chunk_size = cfg.chunk_size as u64;
    let chunk_count = file_size.div_ceil(chunk_size) as usize;

    // 哈希预计算:必须读满一块(或 EOF)才切一个分块 —— tokio read() 允许短读
    let mut hashes = Vec::with_capacity(chunk_count);
    {
        let mut f = tokio::fs::File::open(path).await?;
        let mut buf = vec![0u8; cfg.chunk_size as usize];
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
    }
    hashes.truncate(chunk_count);
    let root = merkle_root(&hashes);
    let manifest = Manifest {
        file_name: file_name.to_string(),
        file_size,
        chunk_size: cfg.chunk_size,
        checksum_algo: "blake3".into(),
        chunk_hashes: hashes
            .iter()
            .map(|h| blake3::Hash::from(*h).to_hex().to_string())
            .collect(),
        merkle_root: root,
        ticket: cfg.ticket.clone(),
    };

    write_frame(ctl_tx, &serde_json::to_vec(&manifest.clone())?).await?;
    let ack: ManifestAck = parse_json(&read_frame(ctl_rx, 1024 * 1024).await?)?;
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
            "send: {file_name} 续传,已完成 {} 块,需传 {} 块",
            chunk_count - todo_count,
            todo_count
        ));
    }
    if todo_count == 0 {
        // 文件已完整(零字节文件天然无块 / 续传时全部块已验证):
        // 仍必须走 Finish→FinishAck 握手 —— 接收端在等 Finish 才会做落盘收尾,
        // 跳过会让对端永远卡在 finish_rx.recv()(死锁)。
        out.log(&format!("send: {file_name} 已完整(无需传输数据块)"));
    }

    // N 条流并行发送,共享待传块栈
    let todo = Arc::new(Mutex::new(todo));
    let sent_bytes = Arc::new(AtomicU64::new(0));
    let done_flag = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();
    let n_streams = cfg.streams.clamp(1, 64);
    for _ in 0..n_streams {
        let opener = clone_opener(opener);
        let todo = todo.clone();
        let path = path.to_path_buf();
        let chunk_size = cfg.chunk_size;
        let sent_bytes = sent_bytes.clone();
        tasks.push(tokio::spawn(async move {
            let mut stream: BoxWrite = opener.open().await?;
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
            }
            stream.shutdown().await?;
            Ok::<(), anyhow::Error>(())
        }));
    }

    // 进度线程:每秒报当前文件进度
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
        tokio::spawn(async move {
            let mut last: u64 = 0;
            while done_flag.load(Ordering::Relaxed) == 0 {
                let now = sent_bytes.load(Ordering::Relaxed);
                if now != last {
                    out2.progress(Progress { bytes_done: now, bytes_total: total });
                }
                last = now;
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
            let now = sent_bytes.load(Ordering::Relaxed);
            if now != last {
                out2.progress(Progress { bytes_done: now, bytes_total: total });
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
    write_frame(ctl_tx, &serde_json::to_vec(&Finish {})?).await?;
    let finish_ack: FinishAck = parse_json(&read_frame(ctl_rx, 4096).await?).map_err(|e| {
        anyhow::anyhow!(FtsError::Interrupted(format!("等待对端校验结果失败: {e:#}")))
    })?;
    done_flag.store(1, Ordering::Relaxed);

    if !finish_ack.checksum_ok {
        return Err(anyhow::anyhow!(FtsError::Checksum(
            finish_ack.error.unwrap_or_else(|| "对端校验失败".into())
        )));
    }

    let elapsed = out.started.elapsed().as_secs_f64();
    out.log(&format!(
        "send: {file_name} 完成,{} 块,耗时 {elapsed:.2}s,平均 {:.1} MB/s",
        chunk_count,
        file_size as f64 / elapsed / 1_048_576.0
    ));
    Ok(file_size)
}
