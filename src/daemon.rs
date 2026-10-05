//! fts daemon:接收循环 + 本地 REST API(api-spec 第 7.5 节 HTTP/REST 的 P3 子集)。
//!
//! 端点:
//!   GET  /v1/capabilities              能力探测
//!   POST /v1/transfers                 提交发送任务 → {task_id}
//!   GET  /v1/transfers                 任务列表
//!   GET  /v1/transfers/{id}            状态
//!   POST /v1/transfers/{id}/cancel     取消
//!   GET  /v1/transfers/{id}/events     SSE 事件流(progress/path/degradation/result)
//!
//! 同时 daemon 以 QUIC+TCP 双栈在 --listen 上接受外部传入的传输。
//! 安全:REST 仅监听 localhost(契约);接收目录与监听地址由启动参数给定。

use crate::send::SendCfg;
use crate::tls::PinMode;
use crate::ui::Output;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{sse::Event as SseEvent, IntoResponse, Sse},
    routing::{get, post},
    Json, Router,
};
use crate::error::FtsError;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

#[derive(Clone)]
pub(crate) struct TaskEntry {
    pub(crate) state: Arc<Mutex<String>>, // RUNNING | COMPLETED | FAILED | CANCELLED
    pub(crate) error: Arc<Mutex<Option<(u8, String, String)>>>, // (exit_code, name, message)
    pub(crate) checksum_ok: Arc<AtomicBool>,
    pub(crate) abort: Arc<tokio::task::AbortHandle>,
    pub(crate) events: broadcast::Sender<String>,
}

#[derive(Clone)]
pub struct Registry {
    pub(crate) inner: Arc<Mutex<HashMap<String, TaskEntry>>>,
    pub(crate) inbox: Arc<crate::inbox::InboxRegistry>,
    /// 本机身份(自描述):数据监听证书指纹与监听地址,REST/gRPC 的 capabilities 共用。
    pub(crate) fingerprint: String,
    pub(crate) listen_addr: String,
    /// W9:本进程启动时刻(unix 秒);远程更新后凭 started_at 变化确认已重启。
    pub(crate) started_at: u64,
    /// W9:BeginUpdate 拉起 apply-update 时透传给 fts start 的启动配置路径
    pub(crate) config_path: PathBuf,
}

impl Registry {
    fn new(
        inbox: Arc<crate::inbox::InboxRegistry>,
        fingerprint: String,
        listen_addr: String,
        started_at: u64,
        config_path: PathBuf,
    ) -> Self {
        Registry {
            inner: Arc::new(Mutex::new(HashMap::new())),
            inbox,
            fingerprint,
            listen_addr,
            started_at,
            config_path,
        }
    }
}

pub struct DaemonCfg {
    pub rest: String,
    pub listen: String,
    pub out_dir: PathBuf,
    pub cert_cache: PathBuf,
    /// W3:最大并发接收会话(QUIC 并发;TCP 会话天然互斥,见 recv.rs 模块注释)
    pub max_sessions: usize,
    /// W4:gRPC 监听地址(None = 禁用)
    pub grpc: Option<std::net::SocketAddr>,
    /// W9:启动用的 config 路径(空 = 无;BeginUpdate 透传给 apply-update 重启用)
    pub config_path: PathBuf,
}

/// 配置文件(fts daemon --config <PATH>):全部字段可选,未提供的回落 CLI/默认值。
/// 字段与 CLI 一一对应;未知字段忽略(前向兼容)。
#[derive(Deserialize, Default, Clone)]
pub struct FileConfig {
    pub rest_addr: Option<String>,
    pub grpc_addr: Option<String>,
    pub listen_addr: Option<String>,
    pub out_dir: Option<PathBuf>,
    pub cert_cache: Option<PathBuf>,
    pub max_sessions: Option<usize>,
}

pub fn load_file_config(path: &PathBuf) -> Result<FileConfig, crate::error::FtsError> {
    let bytes = std::fs::read(path)
        .map_err(|e| crate::error::FtsError::Io(format!("读取配置失败 {}: {e}", path.display())))?;
    // Windows 编辑器(记事本/PowerShell 5.1)常写 UTF-8 BOM,serde_json 不认 —— 剥掉
    let bytes = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) { bytes[3..].to_vec() } else { bytes };
    serde_json::from_slice(&bytes)
        .map_err(|e| crate::error::FtsError::Io(format!("配置解析失败 {}: {e}", path.display())))
}

/// 目录可写预检:启动与 inbox 注册时快速失败,附网络路径提示。
pub fn precheck_dir(dir: &PathBuf) -> Result<(), crate::error::FtsError> {
    std::fs::create_dir_all(dir).map_err(crate::error::FtsError::io_with_hint)?;
    let probe = dir.join(".fts-write-probe");
    std::fs::write(&probe, b"ok").map_err(crate::error::FtsError::io_with_hint)?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

// ---------- W7:exe 原生 start / stop / status(复制即用,无需脚本) ----------

/// 配置生效值解析(config 文件可选,未提供的字段用内置默认)。
/// start/stop/status 与 daemon 共用;cwd 有 config.json 且未显式指定时自动采用。
/// W9:pub(crate) 供 update.rs(apply-update)复用同一解析规则。
pub(crate) fn resolve_config(
    config: Option<PathBuf>,
) -> Result<(FileConfig, PathBuf), crate::error::FtsError> {
    let path = match config {
        Some(p) => Some(p),
        None => {
            let auto = PathBuf::from("config.json");
            if auto.exists() { Some(auto) } else { None }
        }
    };
    let fc = match &path {
        Some(p) => load_file_config(p)?,
        None => FileConfig::default(),
    };
    Ok((fc, path.unwrap_or_default()))
}

pub(crate) fn cert_dir(fc: &FileConfig) -> PathBuf {
    fc.cert_cache.clone().unwrap_or_else(|| PathBuf::from(".fts"))
}

fn pid_file(fc: &FileConfig) -> PathBuf {
    cert_dir(fc).join("daemon.pid")
}

fn read_pid(fc: &FileConfig) -> Option<u32> {
    let s = std::fs::read_to_string(pid_file(fc)).ok()?;
    s.trim().parse().ok()
}

/// 进程是否存活(OpenProcess + STILL_ACTIVE);W9:pub(crate) 供 update.rs 等待旧进程退出。
pub(crate) fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE as u32
    }
    #[cfg(not(windows))]
    { true }
}

fn rest_addr_of(fc: &FileConfig) -> String {
    fc.rest_addr.clone().unwrap_or_else(|| "127.0.0.1:5050".into())
}

fn rest_alive(addr: &str) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    addr.to_socket_addrs()
        .map(|mut it| match it.next() {
            Some(sa) => TcpStream::connect_timeout(&sa, std::time::Duration::from_millis(500)).is_ok(),
            None => false,
        })
        .unwrap_or(false)
}

/// `fts start`:自后台化 —— spawn 自身(daemon)+ pid 文件 + TCP 健康检查
pub fn ctl_start(config: Option<PathBuf>) -> Result<(), crate::error::FtsError> {
    let (fc, cfg_path) = resolve_config(config)?;
    let cd = cert_dir(&fc);
    std::fs::create_dir_all(&cd).map_err(crate::error::FtsError::io_with_hint)?;

    if let Some(pid) = read_pid(&fc) {
        if process_alive(pid) {
            println!("daemon 已在运行(pid={pid}, rest={})", rest_addr_of(&fc));
            return Ok(());
        }
        let _ = std::fs::remove_file(pid_file(&fc)); // 残留 pid 文件,清掉
    }

    // 启动前预检(坏目录在 spawn 前暴露)
    let out_dir = fc.out_dir.clone().unwrap_or_else(|| PathBuf::from("."));
    precheck_dir(&out_dir)?;

    let exe = std::env::current_exe().map_err(FtsError::io)?;
    let log_path = cd.join("daemon.log");
    let err_path = cd.join("daemon.err.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path).map_err(FtsError::io)?;
    let err = std::fs::OpenOptions::new().create(true).append(true).open(&err_path).map_err(FtsError::io)?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("daemon");
    if !cfg_path.as_os_str().is_empty() {
        cmd.arg("--config").arg(&cfg_path);
    }
    cmd.stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(err));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NO_WINDOW:脱离控制台,不闪窗
        cmd.creation_flags(0x0000_0008 | 0x0800_0000);
    }
    let child = cmd
        .spawn()
        .map_err(|e| FtsError::Io(format!("启动 daemon 失败: {e}")))?;
    let pid = child.id();
    std::fs::write(pid_file(&fc), pid.to_string()).map_err(FtsError::io)?;

    // 健康检查:TCP 可连即视为就绪
    let addr = rest_addr_of(&fc);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if rest_alive(&addr) {
            println!(
                "daemon 已启动: pid={pid} rest=http://{addr} out={}",
                out_dir.display()
            );
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    let _ = std::fs::remove_file(pid_file(&fc));
    Err(FtsError::Io(format!(
        "daemon 10s 内未就绪(rest={addr}),已中止并清理 pid;日志见 {}",
        err_path.display()
    )))
}

/// `fts stop`:按 pid 文件终止(taskkill)
pub fn ctl_stop(config: Option<PathBuf>) -> Result<(), crate::error::FtsError> {
    let (fc, _) = resolve_config(config)?;
    match read_pid(&fc) {
        Some(pid) if process_alive(pid) => {
            let out = std::process::Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .output()
                .map_err(FtsError::io)?;
            if out.status.success() {
                println!("daemon 已停止(pid={pid})");
            } else {
                return Err(FtsError::Io(format!(
                    "taskkill 失败: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
        }
        Some(_) => println!("daemon 未运行(pid 文件为残留,已清理)"),
        None => println!("daemon 未运行"),
    }
    let _ = std::fs::remove_file(pid_file(&fc));
    Ok(())
}

/// `fts status`:pid 文件 + TCP 健康检查
pub fn ctl_status(config: Option<PathBuf>) -> Result<(), crate::error::FtsError> {
    let (fc, _) = resolve_config(config)?;
    let pid = read_pid(&fc);
    let alive = pid.map(process_alive).unwrap_or(false);
    let addr = rest_addr_of(&fc);
    let healthy = rest_alive(&addr);
    match (alive, healthy) {
        (true, true) => println!("running (pid={}, rest=http://{addr}, healthy)", pid.unwrap()),
        (false, true) => println!("running (rest=http://{addr} healthy, pid 未知)"),
        (true, false) => println!("degraded (pid={} 存活但 REST 未就绪)", pid.unwrap()),
        (false, false) => println!("stopped"),
    }
    Ok(())
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub async fn run(cfg: DaemonCfg) -> Result<(), crate::error::FtsError> {
    // 启动预检:默认接收目录必须可写(网络路径凭据问题在此暴露,而非传输中途)
    crate::daemon::precheck_dir(&cfg.out_dir)?;
    eprintln!("fts daemon 接收目录预检通过: {}", cfg.out_dir.display());

    let inbox = Arc::new(crate::inbox::InboxRegistry::new());
    // 本机身份:与 recv 监听同一张缓存证书,指纹即发送端 pin 所需值。
    // 编排方(server)通过 capabilities 读取它,免去人工抄指纹。
    let cert = crate::tls::load_or_generate_cert(&cfg.cert_cache)
        .map_err(|e| crate::error::FtsError::Io(format!("加载接收证书失败: {e}")))?;
    let registry = Registry::new(
        inbox.clone(),
        cert.fingerprint.clone(),
        cfg.listen.clone(),
        unix_secs(),
        cfg.config_path.clone(),
    );

    // 接收服务循环(W3:并发会话,单会话失败不影响 daemon)
    {
        let rcfg = crate::recv::RecvCfg {
            listen: cfg.listen.clone(),
            out_dir: cfg.out_dir.clone(),
            out: Output::Silent,
            transport: "auto".into(),
            cert_cache: cfg.cert_cache.clone(),
            events: None,
            inbox: Some(inbox.clone()),
        };
        let max_sessions = cfg.max_sessions;
        tokio::spawn(async move {
            if let Err(e) = crate::recv::serve(rcfg, max_sessions).await {
                eprintln!("[error] 接收服务循环退出: {e}");
            }
        });
    }
    // inbox 过期项定期清理(低频)
    {
        let inbox = inbox.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                inbox.gc();
            }
        });
    }

    let app = Router::new()
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/inbox", post(reserve_inbox))
        .route("/v1/inbox/{token}", axum::routing::delete(revoke_inbox))
        .route("/v1/inbox/{token}/purge", post(purge_inbox))
        .route("/v1/transfers", post(submit).get(list))
        .route("/v1/transfers/{id}", get(status))
        .route("/v1/transfers/{id}/cancel", post(cancel))
        .route("/v1/transfers/{id}/events", get(events))
        .with_state(registry.clone());

    // W4:gRPC 服务(独立端口,与 REST 共享任务表)
    if let Some(grpc_addr) = cfg.grpc {
        let reg = registry.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::grpc::serve(grpc_addr, reg).await {
                eprintln!("[error] gRPC 服务退出: {e}");
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(&cfg.rest)
        .await
        .map_err(|e| crate::error::FtsError::Io(format!("REST 监听失败 {}: {e}", cfg.rest)))?;
    eprintln!("fts daemon REST 监听 http://{}(仅本地)", cfg.rest);
    axum::serve(listener, app)
        .await
        .map_err(|e| crate::error::FtsError::Io(format!("REST 服务失败: {e}")))?;
    Ok(())
}

// ---------- handlers ----------

#[derive(serde::Serialize)]
struct CapabilitiesReply {
    protocol_version: u32,
    fts_version: String,
    capabilities: u32,
    /// 自描述(与 gRPC CapabilitiesReply 对齐):证书指纹 + 数据监听地址
    fingerprint: String,
    listen_addr: String,
    /// W9:进程启动时刻(unix 秒)
    started_at: u64,
    rest_endpoints: Vec<String>,
}

async fn capabilities(State(reg): State<Registry>) -> Json<CapabilitiesReply> {
    // 能力位图(api-spec 7.3):bit3 = resume
    Json(CapabilitiesReply {
        protocol_version: crate::protocol::PROTOCOL_VERSION,
        fts_version: env!("CARGO_PKG_VERSION").into(),
        capabilities: 1 << 3,
        fingerprint: reg.fingerprint.clone(),
        listen_addr: reg.listen_addr.clone(),
        started_at: reg.started_at,
        rest_endpoints: vec![
            "GET /v1/capabilities".into(),
            "POST /v1/transfers".into(),
            "GET /v1/transfers".into(),
            "GET /v1/transfers/{id}".into(),
            "POST /v1/transfers/{id}/cancel".into(),
            "GET /v1/transfers/{id}/events".into(),
        ],
    })
}

#[derive(Deserialize)]
struct SubmitReq {
    /// 发送文件列表(P5 W2;目录会被展开为直接文件)。兼容旧字段:单路径可用 src_path
    paths: Option<Vec<String>>,
    src_path: Option<String>,
    dst_addr: String,
    /// 对端证书指纹(blake3 hex)
    fingerprint: Option<String>,
    #[serde(default)]
    insecure: bool,
    streams: Option<u32>,
    chunk_size: Option<u32>,
    /// auto | quic | tcp
    transport: Option<String>,
    /// W1 inbox 票据(经 /v1/inbox 获取;决定对端落盘目录)
    ticket: Option<String>,
}

/// W1:预注册接收目录 → 一次性 token(W6:可携带网络凭据,fts 代建 SMB 会话)
#[derive(Deserialize)]
struct ReserveInboxReq {
    out_dir: String,
    /// Some = 绑定精确文件名;None = 目录模式(任意文件名)
    file_name: Option<String>,
    /// 目标为需要认证的网络路径(SMB/UNC)时的凭据;密码不落日志
    credential: Option<crate::netcred::NetworkCredential>,
}

async fn reserve_inbox(
    State(reg): State<Registry>,
    Json(req): Json<ReserveInboxReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let out_dir = PathBuf::from(&req.out_dir);
    // W6:凭据会话先建立(失败 → 400 + 提示;密码不进任何错误/日志)
    if let Some(cred) = &req.credential {
        if let Err(e) = crate::netcred::establish_connection(&req.out_dir, cred) {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("{e}") })),
            ));
        }
        eprintln!(
            "[info] inbox 凭据会话已建立: {} ({})",
            req.out_dir,
            crate::netcred::describe(cred)
        );
    }
    // 注册即预检目录可写(网络路径凭据问题在提交任务前暴露)
    if let Err(e) = precheck_dir(&out_dir) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("{e}") })),
        ));
    }
    let token = reg.inbox.reserve(out_dir, req.file_name);
    Ok(Json(
        serde_json::json!({ "token": token, "expires_in": crate::inbox::TICKET_TTL.as_secs() }),
    ))
}

/// W6:危险操作 —— 清空 token 绑定目录的全部内容(force 干净复制的前置)。
/// 三重保障:仅目录模式 token;必须显式 force=true;只清空目录内部(不碰目录本身与外部)。
#[derive(Deserialize)]
struct PurgeReq {
    force: bool,
}

async fn purge_inbox(
    State(reg): State<Registry>,
    Path(token): Path<String>,
    Json(req): Json<PurgeReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !req.force {
        return Err(bad_request(
            "purge 是危险操作,必须显式 force=true",
        ));
    }
    let dir = match reg.inbox.peek_purgeable(&token) {
        Ok(d) => d,
        Err((reason, invalid)) => {
            let status = if invalid { StatusCode::NOT_FOUND } else { StatusCode::FORBIDDEN };
            return Err((
                status,
                Json(serde_json::json!({ "error": reason })),
            ));
        }
    };
    let (files, directories) = purge_dir_contents(&dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": format!("{e}") })),
        )
    })?;
    eprintln!(
        "[warn] PURGE 目录已清空: {} (删除 {files} 个文件 / {directories} 个文件夹) token={token}",
        dir.display()
    );
    Ok(Json(serde_json::json!({
        "ok": true,
        "files_deleted": files,
        "directories_deleted": directories,
        "dir": dir.display().to_string(),
    })))
}

/// 清空目录内部全部内容(不删目录本身);任一删除失败立即返回。
/// gRPC 与 REST 共用;返回 (删除文件数, 删除目录数)。
pub fn purge_dir_contents(dir: &PathBuf) -> Result<(u64, u64), crate::error::FtsError> {
    let mut files = 0u64;
    let mut directories = 0u64;
    for e in std::fs::read_dir(dir)
        .map_err(|e| crate::error::FtsError::io_with_hint(e))?
    {
        let p = e.map_err(|e| crate::error::FtsError::io_with_hint(e))?.path();
        // 只清目录内直接项;remove_dir_all 不跟随目录 symlink(Rust 标准库已加固)
        if p.is_dir() {
            std::fs::remove_dir_all(&p).map_err(crate::error::FtsError::io_with_hint)?;
            directories += 1;
        } else {
            std::fs::remove_file(&p).map_err(crate::error::FtsError::io_with_hint)?;
            files += 1;
        }
    }
    Ok((files, directories))
}

async fn revoke_inbox(
    State(reg): State<Registry>,
    Path(token): Path<String>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": reg.inbox.revoke(&token) }))
}

async fn submit(
    State(reg): State<Registry>,
    Json(req): Json<SubmitReq>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let pin = match (&req.fingerprint, req.insecure) {
        (Some(fp), false) => match crate::tls::decode_fingerprint(fp) {
            Some(bytes) => PinMode::Fingerprint(bytes),
            None => {
                return Err(bad_request("--fingerprint 格式无效,应为 64 位 hex"));
            }
        },
        (None, true) => PinMode::Insecure,
        (Some(_), true) => {
            return Err(bad_request("fingerprint 与 insecure 不能同时使用"));
        }
        (None, false) => {
            return Err(bad_request("必须提供 fingerprint 或 insecure=true"));
        }
    };

    let (events_tx, _) = broadcast::channel::<String>(256);
    let task_id = uuid::Uuid::new_v4().to_string();
    let state = Arc::new(Mutex::new("RUNNING".to_string()));
    let error: Arc<Mutex<Option<(u8, String, String)>>> = Arc::new(Mutex::new(None));
    let checksum_ok = Arc::new(AtomicBool::new(false));

    let send_cfg = SendCfg {
        paths: match (&req.paths, &req.src_path) {
            (Some(p), _) => p.iter().map(PathBuf::from).collect(),
            (None, Some(p)) => vec![PathBuf::from(p)],
            (None, None) => {
                return Err(bad_request("必须提供 paths(列表)或 src_path(单文件)"));
            }
        },
        to: req.dst_addr.clone(),
        chunk_size: req.chunk_size.unwrap_or(crate::protocol::DEFAULT_CHUNK_SIZE),
        streams: req.streams.unwrap_or(4),
        pin,
        transport: req.transport.unwrap_or_else(|| "auto".into()),
        ticket: req.ticket.clone(),
        out: Output::Silent,
        events: Some(events_tx.clone()),
    };

    let st = state.clone();
    let er = error.clone();
    let cko = checksum_ok.clone();
    let handle = tokio::spawn(async move {
        let result = crate::send::run_send(send_cfg).await;
        match result {
            Ok(()) => {
                *st.lock().unwrap() = "COMPLETED".into();
                cko.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                let cancelled = matches!(e, crate::error::FtsError::Cancelled);
                *st.lock().unwrap() = if cancelled { "CANCELLED".into() } else { "FAILED".into() };
                *er.lock().unwrap() = Some((e.exit_code(), e.name().to_string(), format!("{e}")));
            }
        }
    });

    let entry = TaskEntry {
        state,
        error,
        checksum_ok,
        abort: Arc::new(handle.abort_handle()),
        events: events_tx,
    };
    reg.inner.lock().unwrap().insert(task_id.clone(), entry);

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "task_id": task_id })),
    ))
}

fn bad_request(msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
}

async fn list(State(reg): State<Registry>) -> Json<serde_json::Value> {
    let tasks: Vec<serde_json::Value> = reg
        .inner
        .lock()
        .unwrap()
        .iter()
        .map(|(id, e)| {
            serde_json::json!({
                "task_id": id,
                "state": e.state.lock().unwrap().clone(),
            })
        })
        .collect();
    Json(serde_json::json!({ "tasks": tasks }))
}

async fn status(
    State(reg): State<Registry>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let entry = reg.inner.lock().unwrap().get(&id).cloned();
    match entry {
        Some(e) => {
            let state = e.state.lock().unwrap().clone();
            let mut body = serde_json::json!({ "task_id": id, "state": state });
            if let Some((code, name, msg)) = e.error.lock().unwrap().clone() {
                body["error_code"] = code.into();
                body["error"] = name.into();
                body["error_message"] = msg.into();
            }
            body["checksum_ok"] = e
                .checksum_ok
                .load(std::sync::atomic::Ordering::Relaxed)
                .into();
            Ok(Json(body))
        }
        None => Err(not_found()),
    }
}

fn not_found() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "任务不存在" })),
    )
}

async fn cancel(
    State(reg): State<Registry>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let entry = reg.inner.lock().unwrap().get(&id).cloned();
    match entry {
        Some(e) => {
            let running = *e.state.lock().unwrap() == "RUNNING";
            if running {
                e.abort.abort();
                *e.state.lock().unwrap() = "CANCELLED".into();
            }
            Ok(Json(serde_json::json!({ "ok": true })))
        }
        None => Err(not_found()),
    }
}

async fn events(
    State(reg): State<Registry>,
    Path(id): Path<String>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<SseEvent, std::convert::Infallible>>>, (StatusCode, Json<serde_json::Value>)>
{
    let entry = reg.inner.lock().unwrap().get(&id).cloned();
    match entry {
        Some(e) => {
            let rx = e.events.subscribe();
            let stream = BroadcastStream::new(rx).map(|item| {
                Ok(SseEvent::default().data(item.unwrap_or_else(|_| {
                    r#"{"type":"error","message":"lagged"}"#.to_string()
                })))
            });
            Ok(Sse::new(stream))
        }
        None => Err(not_found()),
    }
}
