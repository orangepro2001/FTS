//! gRPC 服务(W4):api-spec 7.2 的 P5 子集 + ReserveInbox。
//! 与 REST 共享同一 Registry(任务表)与 InboxRegistry;仅监听 localhost(契约 7.6)。
//! 事件:内部 broadcast<String>(NDJSON)→ TransferEvent{raw_json}(向前兼容)。

use crate::daemon::Registry;
use crate::send::SendCfg;
use crate::tls::PinMode;
use crate::ui::Output;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{StreamExt, Stream};
use tonic::{Request, Response, Status};

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/fts.v1.rs"));
}

/// FtsError → gRPC Status(错误名映射;消息原文保留)
fn status_from_fts(e: crate::error::FtsError) -> Status {
    match &e {
        crate::error::FtsError::InvalidArg(_) => Status::invalid_argument(format!("{e}")),
        crate::error::FtsError::Auth(_) => Status::permission_denied(format!("{e}")),
        crate::error::FtsError::Connect(_) | crate::error::FtsError::Version(_) => {
            Status::unavailable(format!("{e}"))
        }
        _ => Status::internal(format!("{e}")),
    }
}

use pb::fts_server::{Fts, FtsServer};
use pb::{
    Ack, CapabilitiesReply, Empty, InboxToken, ObserveRequest, PurgeReply, PurgeRequest,
    ReserveInboxRequest, TaskRef, TransferEvent, TransferHandle, TransferRequest, TransferStatus,
    UpdateRequest,
};

pub struct GrpcService {
    reg: Registry,
}

#[tonic::async_trait]
impl Fts for GrpcService {
    async fn capabilities(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<CapabilitiesReply>, Status> {
        Ok(Response::new(CapabilitiesReply {
            protocol_version: crate::protocol::PROTOCOL_VERSION.to_string(),
            capabilities: 1 << 3, // bit3 = resume(对齐 REST)
            fts_version: env!("CARGO_PKG_VERSION").into(),
            fingerprint: self.reg.fingerprint.clone(),
            listen_addr: self.reg.listen_addr.clone(),
            started_at: self.reg.started_at,
        }))
    }

    async fn reserve_inbox(
        &self,
        req: Request<ReserveInboxRequest>,
    ) -> Result<Response<InboxToken>, Status> {
        let r = req.into_inner();
        if r.out_dir.is_empty() {
            return Err(Status::invalid_argument("out_dir 不能为空"));
        }
        // W6:凭据会话先建立(密码不进任何错误/日志)
        if let Some(cred) = &r.credential {
            let local = crate::netcred::NetworkCredential {
                user_name: cred.user_name.clone(),
                password: cred.password.clone(),
                domain: cred.domain.clone(),
            };
            crate::netcred::establish_connection(&r.out_dir, &local).map_err(status_from_fts)?;
            eprintln!(
                "[info] inbox 凭据会话已建立: {} ({})",
                r.out_dir,
                crate::netcred::describe(&local)
            );
        }
        let file_name = if r.file_name.is_empty() { None } else { Some(r.file_name) };
        let token = self.reg.inbox.reserve(PathBuf::from(&r.out_dir), file_name);
        Ok(Response::new(InboxToken {
            token,
            expires_in: crate::inbox::TICKET_TTL.as_secs(),
        }))
    }

    /// W6:危险操作 —— 清空目录模式 token 绑定目录的全部内容
    async fn purge_inbox(
        &self,
        req: Request<PurgeRequest>,
    ) -> Result<Response<PurgeReply>, Status> {
        let r = req.into_inner();
        if !r.force {
            return Err(Status::invalid_argument(
                "purge 是危险操作,必须显式 force=true",
            ));
        }
        let dir = self.reg.inbox.peek_purgeable(&r.token).map_err(|(reason, invalid)| {
            if invalid {
                Status::not_found(reason)
            } else {
                Status::permission_denied(reason)
            }
        })?;
        let (files, directories) = crate::daemon::purge_dir_contents(&dir)
            .map_err(|e| Status::internal(format!("{e}")))?;
        eprintln!(
            "[warn] PURGE 目录已清空: {} (删除 {files} 个文件 / {directories} 个文件夹) token={}",
            dir.display(),
            r.token
        );
        Ok(Response::new(PurgeReply {
            files_deleted: files,
            directories_deleted: directories,
        }))
    }

    async fn submit(
        &self,
        req: Request<TransferRequest>,
    ) -> Result<Response<TransferHandle>, Status> {
        let r = req.into_inner();
        let pin = match (!r.fingerprint.is_empty(), r.insecure) {
            (true, false) => match crate::tls::decode_fingerprint(&r.fingerprint) {
                Some(b) => PinMode::Fingerprint(b),
                None => {
                    return Err(Status::invalid_argument("fingerprint 格式无效,应为 64 位 hex"))
                }
            },
            (false, true) => PinMode::Insecure,
            (true, true) => return Err(Status::invalid_argument("fingerprint 与 insecure 互斥")),
            (false, false) => {
                return Err(Status::invalid_argument("必须提供 fingerprint 或 insecure=true"))
            }
        };
        if r.paths.is_empty() {
            return Err(Status::invalid_argument("paths 不能为空"));
        }

        let (events_tx, _) = broadcast::channel::<String>(256);
        let task_id = uuid::Uuid::new_v4().to_string();
        let state = Arc::new(Mutex::new("RUNNING".to_string()));
        let error: Arc<Mutex<Option<(u8, String, String)>>> = Arc::new(Mutex::new(None));
        let checksum_ok = Arc::new(AtomicBool::new(false));

        let send_cfg = SendCfg {
            paths: r.paths.iter().map(PathBuf::from).collect(),
            to: r.dst_addr.clone(),
            chunk_size: if r.chunk_size == 0 {
                crate::protocol::DEFAULT_CHUNK_SIZE
            } else {
                u32::try_from(r.chunk_size).unwrap_or(crate::protocol::DEFAULT_CHUNK_SIZE)
            },
            streams: if r.streams == 0 { 4 } else { r.streams.clamp(1, 64) },
            pin,
            transport: if r.transport.is_empty() { "auto".into() } else { r.transport.clone() },
            ticket: if r.ticket.is_empty() { None } else { Some(r.ticket.clone()) },
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
                    *st.lock().unwrap() =
                        if cancelled { "CANCELLED".into() } else { "FAILED".into() };
                    *er.lock().unwrap() =
                        Some((e.exit_code(), e.name().to_string(), format!("{e}")));
                }
            }
        });

        self.reg.inner.lock().unwrap().insert(
            task_id.clone(),
            crate::daemon::TaskEntry {
                state,
                error,
                checksum_ok,
                abort: Arc::new(handle.abort_handle()),
                events: events_tx,
            },
        );
        Ok(Response::new(TransferHandle { task_id }))
    }

    type ObserveStream =
        std::pin::Pin<Box<dyn Stream<Item = Result<TransferEvent, Status>> + Send>>;

    async fn observe(
        &self,
        req: Request<ObserveRequest>,
    ) -> Result<Response<Self::ObserveStream>, Status> {
        let id = req.into_inner().task_id;
        let entry = self.reg.inner.lock().unwrap().get(&id).cloned();
        match entry {
            Some(e) => {
                let rx = e.events.subscribe();
                let stream = BroadcastStream::new(rx).map(|item| {
                    Ok(TransferEvent {
                        raw_json: item.unwrap_or_else(|_| {
                            r#"{"type":"error","message":"lagged"}"#.to_string()
                        }),
                    })
                });
                Ok(Response::new(Box::pin(stream)))
            }
            None => Err(Status::not_found("任务不存在")),
        }
    }

    async fn status(&self, req: Request<TaskRef>) -> Result<Response<TransferStatus>, Status> {
        let id = req.into_inner().task_id;
        let entry = self.reg.inner.lock().unwrap().get(&id).cloned();
        match entry {
            Some(e) => {
                let state = e.state.lock().unwrap().clone();
                let mut body = TransferStatus {
                    task_id: id,
                    state,
                    checksum_ok: e.checksum_ok.load(std::sync::atomic::Ordering::Relaxed),
                    error_code: 0,
                    error: String::new(),
                    error_message: String::new(),
                };
                if let Some((code, name, msg)) = e.error.lock().unwrap().clone() {
                    body.error_code = code as i32;
                    body.error = name;
                    body.error_message = msg;
                }
                Ok(Response::new(body))
            }
            None => Err(Status::not_found("任务不存在")),
        }
    }

    async fn cancel(&self, req: Request<TaskRef>) -> Result<Response<Ack>, Status> {
        let id = req.into_inner().task_id;
        let entry = self.reg.inner.lock().unwrap().get(&id).cloned();
        match entry {
            Some(e) => {
                let running = *e.state.lock().unwrap() == "RUNNING";
                if running {
                    e.abort.abort();
                    *e.state.lock().unwrap() = "CANCELLED".into();
                }
                Ok(Response::new(Ack { ok: true }))
            }
            None => Err(Status::not_found("任务不存在")),
        }
    }

    /// W9:远程更新 —— 校验暂存区后拉起新版 fts.exe apply-update,随后本进程自退。
    /// Ack 返回即代表已受理;调用方之后轮询 Capabilities 等待新 fts_version。
    async fn begin_update(&self, req: Request<UpdateRequest>) -> Result<Response<Ack>, Status> {
        let r = req.into_inner();
        if r.staging_dir.trim().is_empty() {
            return Err(Status::invalid_argument("staging_dir 不能为空"));
        }
        let staging = PathBuf::from(&r.staging_dir);
        if !staging.is_dir() {
            return Err(Status::invalid_argument(format!("暂存目录不存在: {}", staging.display())));
        }
        if !r.files.iter().any(|f| f.eq_ignore_ascii_case("fts.exe")) {
            return Err(Status::invalid_argument("files 必须包含 fts.exe"));
        }
        for f in &r.files {
            // 纯文件名:拒绝路径分隔符/穿越(直接落在安装目录)
            if f.contains('/') || f.contains('\\') || f.contains("..") {
                return Err(Status::invalid_argument(format!("文件名不允许包含路径分隔符: {f}")));
            }
            if !staging.join(f).is_file() {
                return Err(Status::invalid_argument(format!("暂存区缺少文件: {f}")));
            }
        }
        let running = self
            .reg
            .inner
            .lock()
            .unwrap()
            .values()
            .any(|e| *e.state.lock().unwrap() == "RUNNING");
        if running && !r.force {
            return Err(Status::failed_precondition(
                "存在 RUNNING 传输任务,更新会中断传输;确认无影响后传 force=true",
            ));
        }

        let exe = std::env::current_exe()
            .map_err(|e| Status::internal(format!("current_exe 失败: {e}")))?;
        let install = exe
            .parent()
            .ok_or_else(|| Status::internal("current_exe 无父目录"))?
            .to_path_buf();

        // 以 DETACHED 拉起暂存区里的新版 exe(自己负责等旧进程退出/强杀/换文件/重启)
        let mut cmd = std::process::Command::new(staging.join("fts.exe"));
        cmd.arg("apply-update")
            .arg("--pid").arg(std::process::id().to_string())
            .arg("--install").arg(&install)
            .arg("--staging").arg(&staging)
            .arg("--from").arg(env!("CARGO_PKG_VERSION"));
        for f in &r.files {
            cmd.arg("--files").arg(f);
        }
        if !self.reg.config_path.as_os_str().is_empty() {
            cmd.arg("--config").arg(&self.reg.config_path);
        }
        cmd.current_dir(
            std::env::current_dir().map_err(|e| Status::internal(format!("current_dir 失败: {e}")))?,
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // DETACHED_PROCESS | CREATE_NO_WINDOW:更新器无窗口,输出走 apply.log
            cmd.creation_flags(0x0000_0008 | 0x0800_0000);
        }
        cmd.spawn()
            .map_err(|e| Status::internal(format!("拉起 apply-update 失败: {e}")))?;

        eprintln!(
            "[warn] UPDATE:apply-update 已拉起(install={}, files={:?}),daemon 即将退出以应用更新",
            install.display(),
            r.files
        );
        // 给 tonic 留出把 Ack 发回的时间,然后自退;pid 文件残留由 fts start 清理
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            std::process::exit(0);
        });
        Ok(Response::new(Ack { ok: true }))
    }
}

/// 启动 gRPC 服务(阻塞当前任务直到服务退出)
pub async fn serve(
    addr: std::net::SocketAddr,
    reg: Registry,
) -> Result<(), crate::error::FtsError> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| crate::error::FtsError::Io(format!("gRPC 监听失败 {addr}: {e}")))?;
    eprintln!("fts daemon gRPC 监听 http://{addr}");
    tonic::transport::Server::builder()
        .add_service(FtsServer::new(GrpcService { reg }))
        .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
        .await
        .map_err(|e| crate::error::FtsError::Io(format!("gRPC 服务失败: {e}")))
}
