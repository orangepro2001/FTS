//! W9:远程自更新(apply-update)。
//!
//! 流程:BeginUpdate(gRPC)校验暂存区后,以 DETACHED 拉起暂存区里的新版 fts.exe
//! 执行 `apply-update`;本模块只实现 apply-update 侧逻辑(它自己就是暂存区副本):
//!   1. 等待旧 daemon(pid)退出(轮询,超时 taskkill /F 兜底)
//!   2. 备份安装目录中将被覆盖的文件 → cert_dir/update/backup-<ts>/
//!   3. 复制暂存文件覆盖安装目录
//!   4. 拉起安装目录下的 `fts.exe start` —— ctl_start 内部用 current_exe,必须让
//!      新装 exe 自己拉起 daemon;绝不能用暂存副本(暂存区随时可能被清理)
//!   5. 失败回滚(恢复备份后重试启动);结果写 cert_dir/update/result.json
//!
//! apply-update 无控制台(DETACHED),所有输出走 apply.log 文件。

use crate::error::FtsError;
use crate::grpc::pb;
use std::path::{Path, PathBuf};
use std::time::Duration;

// ---------- 客户端侧:推送新版并触发远程自替换(push_update,GUI/CLI 共用) ----------

/// dist 里的标准文件集(存在才推;config.json 默认不覆盖,各机配置不同)
pub const DEFAULT_FILE_SET: &[&str] = &[
    "fts.exe",
    "fts.dll",
    "fts.h",
    "fts-daemon.ps1",
    "fts-daemon.cmd",
    "DEPLOY.txt",
    "VERSION",
];

#[derive(Debug, Clone)]
pub struct PushUpdateCfg {
    /// 目标 daemon gRPC 地址(host[:50051])
    pub grpc_addr: String,
    /// 本机新版 dist 目录
    pub dist_dir: PathBuf,
    /// true = 连 config.json 也覆盖(默认保留目标机配置)
    pub overwrite_config: bool,
    /// 透传 BeginUpdate(存在 RUNNING 任务时需 true)
    pub force: bool,
    /// 进度行接收端(文本行/NDJSON;None = 静默)
    pub on_event: Option<tokio::sync::mpsc::Sender<String>>,
}

#[derive(Debug)]
pub struct UpdateReport {
    pub ok: bool,
    pub old_version: String,
    pub new_version: String,
    pub detail: String,
}

/// 完整更新流程:探测 → ReserveInbox(暂存) → fts 传输推送 → BeginUpdate → 轮询确认。
pub async fn push_update(cfg: PushUpdateCfg) -> UpdateReport {
    let say = |m: String| {
        if let Some(tx) = &cfg.on_event {
            let _ = tx.try_send(m);
        }
    };
    let mut report = UpdateReport {
        ok: false,
        old_version: String::new(),
        new_version: String::new(),
        detail: String::new(),
    };

    // dist 校验:fts.exe 必须,预期版本取 VERSION(若有)
    if !cfg.dist_dir.join("fts.exe").is_file() {
        report.detail = format!("dist 目录缺少 fts.exe: {}", cfg.dist_dir.display());
        return report;
    }
    let expected_version = std::fs::read_to_string(cfg.dist_dir.join("VERSION"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut files: Vec<String> = DEFAULT_FILE_SET
        .iter()
        .map(|s| s.to_string())
        .collect();
    if cfg.overwrite_config {
        files.push("config.json".into());
    }
    files.retain(|f| cfg.dist_dir.join(f).is_file());
    if !files.iter().any(|f| f == "fts.exe") {
        report.detail = "文件集校验失败".into();
        return report;
    }

    say(format!("[push] 连接 {}/v1 (gRPC) ...", cfg.grpc_addr));
    let mut client = match connect_client(&cfg.grpc_addr).await {
        Ok(c) => c,
        Err(e) => {
            report.detail = format!("gRPC 连接失败: {e}");
            return report;
        }
    };
    let caps = match capabilities(&mut client).await {
        Ok(c) => c,
        Err(e) => {
            report.detail = format!("Capabilities 失败: {e}");
            return report;
        }
    };
    report.old_version = caps.fts_version.clone();
    let old_started_at = caps.started_at;
    say(format!(
        "[push] 目标在线 version={} started_at={} fingerprint={} listen={}",
        caps.fts_version, caps.started_at, caps.fingerprint, caps.listen_addr
    ));

    // 数据面地址:gRPC host + 数据监听端口(listen_addr 可能为 0.0.0.0,host 部分替换)
    let grpc_host = cfg
        .grpc_addr
        .rsplit_once(':')
        .map(|(h, _)| h.to_string())
        .unwrap_or_else(|| cfg.grpc_addr.clone());
    let data_port = caps
        .listen_addr
        .rsplit_once(':')
        .map(|(_, p)| p.to_string())
        .unwrap_or_else(|| "4433".into());
    let data_addr = format!("{grpc_host}:{data_port}");

    // 暂存目录(daemon cwd 相对路径;ReserveInbox 会创建并预检)
    let ts = unix_secs();
    let staging = PathBuf::from(".fts").join("update").join(format!("stage-{ts}"));
    say(format!("[push] 预注册暂存目录 {} ...", staging.display()));
    let token = match client
        .reserve_inbox(pb::ReserveInboxRequest {
            out_dir: staging.display().to_string(),
            file_name: String::new(),
            credential: None,
        })
        .await
    {
        Ok(r) => r.into_inner().token,
        Err(e) => {
            report.detail = format!("ReserveInbox 失败: {e}");
            return report;
        }
    };

    // 推送(fts 自有传输,QUIC 优先;事件行直通)
    let paths: Vec<PathBuf> = files.iter().map(|f| cfg.dist_dir.join(f)).collect();
    say(format!("[push] 推送 {} 个文件 → {data_addr} ...", paths.len()));
    let (tx, mut rx) = tokio::sync::broadcast::channel::<String>(256);
    let fwd = cfg.on_event.clone();
    let forwarder = tokio::spawn(async move {
        while let Ok(line) = rx.recv().await {
            if let Some(out) = &fwd {
                let _ = out.try_send(line);
            }
        }
    });
    let send_result = crate::send::run_send(crate::send::SendCfg {
        paths,
        to: data_addr,
        chunk_size: crate::protocol::DEFAULT_CHUNK_SIZE,
        streams: 4,
        pin: match crate::tls::decode_fingerprint(&caps.fingerprint) {
            Some(b) => crate::tls::PinMode::Fingerprint(b),
            None => crate::tls::PinMode::Insecure,
        },
        transport: "auto".into(),
        ticket: Some(token),
        out: crate::ui::Output::Silent,
        events: Some(tx.clone()),
    })
    .await;
    drop(tx);
    let _ = forwarder.await;
    if let Err(e) = send_result {
        report.detail = format!("推送失败: {e}");
        return report;
    }

    // 触发自替换
    say("[push] BeginUpdate(目标 daemon 即将自替换重启)...".into());
    match client
        .begin_update(pb::UpdateRequest {
            staging_dir: staging.display().to_string(),
            files: files.clone(),
            force: cfg.force,
        })
        .await
    {
        Ok(ack) if ack.get_ref().ok => {}
        Err(status) if status.code() == tonic::Code::Unimplemented => {
            report.detail =
                "目标 daemon 版本过旧(无 BeginUpdate RPC):请按老方式手工部署一次新版 dist,之后即可远程更新"
                    .into();
            return report;
        }
        Err(e) => {
            report.detail = format!("BeginUpdate 失败: {e}");
            return report;
        }
        _ => {
            report.detail = "BeginUpdate 返回异常".into();
            return report;
        }
    }

    // 轮询确认:凭 Capabilities.started_at 变化判定目标已重启(本地回环更新全程
    // 可能 <2s,靠连接空窗判断是竞态,不可靠 —— 已实测踩坑)。
    // 版本规则:dist 声明了不同版本 → 必须看到新版本;同版本重部署/无 VERSION → 重启即成功。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let t0 = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if tokio::time::Instant::now() >= deadline {
            report.detail =
                "120s 内未确认到新版本(目标可能仍在重启或已回滚;查目标机 .fts/update/apply.log)".into();
            return report;
        }
        let at = t0.elapsed().as_millis();
        let mut probe = match connect_client(&cfg.grpc_addr).await {
            Ok(c) => c,
            Err(e) => {
                say(format!("[poll {at}ms] 连接失败(重启窗口): {e}"));
                continue;
            }
        };
        let c = match capabilities(&mut probe).await {
            Ok(c) => c,
            Err(e) => {
                say(format!("[poll {at}ms] capabilities 失败(重启窗口): {e}"));
                continue;
            }
        };
        let v = c.fts_version.clone();
        let restarted = c.started_at != old_started_at;
        say(format!(
            "[poll {at}ms] 在线 version={v} started_at={} restarted={restarted}",
            c.started_at
        ));
        let success = match &expected_version {
            // dist 声明了不同版本:必须看到新版本才算成功(重启后仍是旧版本 = 已回滚)
            Some(ev) if ev != &report.old_version => v == *ev,
            // 同版本重部署或 dist 无 VERSION:确认重启即成功
            Some(ev) => restarted && v == *ev,
            None => restarted,
        };
        if success {
            report.ok = true;
            report.new_version = v;
            say(format!(
                "[push] 更新成功: {} → {}(重启确认)",
                report.old_version, report.new_version
            ));
            return report;
        }
    }
}

/// 连接目标 gRPC(供 push_update 与 GUI 更新器共用);地址 host[:50051]
pub async fn connect_client(
    addr: &str,
) -> Result<pb::fts_client::FtsClient<tonic::transport::Channel>, String> {
    let ep = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .map_err(|e| format!("地址无效 {addr}: {e}"))?
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(15));
    let ch = ep
        .connect()
        .await
        .map_err(|e| format!("连接 {addr} 失败: {e}"))?;
    Ok(pb::fts_client::FtsClient::new(ch))
}

pub async fn capabilities(
    client: &mut pb::fts_client::FtsClient<tonic::transport::Channel>,
) -> Result<pb::CapabilitiesReply, String> {
    client
        .capabilities(pb::Empty {})
        .await
        .map(|r| r.into_inner())
        .map_err(|e| e.to_string())
}


#[derive(Debug, Clone)]
pub struct ApplyUpdateCfg {
    /// 旧 daemon 进程 pid(BeginUpdate 传入)
    pub pid: u32,
    /// 安装目录(覆盖目标;current_exe 所在目录)
    pub install_dir: PathBuf,
    /// 暂存目录(新版文件来源)
    pub staging_dir: PathBuf,
    /// 要应用的文件名(已在暂存区就位)
    pub files: Vec<String>,
    /// daemon 启动配置(透传给 fts start;None/空则按 cwd 解析)
    pub config: Option<PathBuf>,
    /// 更新前版本(仅记录)
    pub from_version: String,
}

/// apply-update 入口:等旧进程退出 → 备份+覆盖 → 重启 → 失败回滚。
pub fn apply_update(cfg: ApplyUpdateCfg) -> Result<(), FtsError> {
    let to_version = env!("CARGO_PKG_VERSION").to_string();
    // 结果/日志目录:与 daemon 相同的配置解析规则(cwd 从 daemon 继承)
    let (fc, _) = crate::daemon::resolve_config(cfg.config.clone())
        .map_err(|e| FtsError::Io(format!("apply-update 解析配置失败: {e}")))?;
    let update_dir = crate::daemon::cert_dir(&fc).join("update");
    std::fs::create_dir_all(&update_dir).map_err(FtsError::io_with_hint)?;
    let log_path = update_dir.join("apply.log");

    log_line(
        &log_path,
        &format!(
            "apply-update 开始: pid={} install={} from={to_version}",
            cfg.pid,
            cfg.install_dir.display(),
        ),
    );

    // 1. 等待旧 daemon 退出(它收到 Ack 后 ~0.8s 自退;apply-update 兜底强杀)
    if !wait_pid_exit(cfg.pid, Duration::from_secs(15)) {
        let msg = format!("旧进程(pid={}) 15s 内未退出,放弃更新(文件未动)", cfg.pid);
        log_line(&log_path, &msg);
        write_result(&update_dir, false, &cfg.from_version, &to_version, &cfg.files, &msg);
        return Err(FtsError::Io(msg));
    }
    std::thread::sleep(Duration::from_millis(300)); // 句柄释放缓冲

    // 2+3. 备份 + 覆盖
    let ts = unix_secs();
    let backup_dir = update_dir.join(format!("backup-{ts}"));
    if let Err(e) = swap_files(&cfg.install_dir, &cfg.staging_dir, &cfg.files, &backup_dir) {
        let msg = format!("文件替换失败: {e}(文件未动或部分覆盖,见备份)");
        log_line(&log_path, &msg);
        write_result(&update_dir, false, &cfg.from_version, &to_version, &cfg.files, &msg);
        return Err(e);
    }
    log_line(
        &log_path,
        &format!("文件替换完成({} 个),备份: {}", cfg.files.len(), backup_dir.display()),
    );

    // 4. 用安装目录的新 exe 重启(见模块注释:不能 current_exe —— 那是暂存副本)
    let start_exe = cfg.install_dir.join("fts.exe");
    match start_installed(&start_exe, &cfg.config, &log_path) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&backup_dir); // 成功清备份
            log_line(&log_path, &format!("更新成功: {to_version}(daemon 健康检查通过)"));
            write_result(&update_dir, true, &cfg.from_version, &to_version, &cfg.files, "");
            Ok(())
        }
        Err(e) => {
            log_line(&log_path, &format!("重启失败: {e};回滚备份并重试启动"));
            let restore = restore_backup(&cfg.install_dir, &backup_dir, &cfg.files);
            log_line(&log_path, &format!("回滚结果: {:?}", restore.as_ref().map(|_| "ok")));
            let _ = start_installed(&start_exe, &cfg.config, &log_path);
            write_result(
                &update_dir,
                false,
                &cfg.from_version,
                &to_version,
                &cfg.files,
                &format!("{e}(已回滚)"),
            );
            Err(e)
        }
    }
}

/// 等待 pid 退出:轮询 process_alive;超时后 taskkill /F 兜底再等 1s。
/// 返回 true = 已确认退出。非 Windows 无 taskkill,超时即放弃。
pub fn wait_pid_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !crate::daemon::process_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        std::thread::sleep(Duration::from_secs(1));
        !crate::daemon::process_alive(pid)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 备份 + 覆盖:先统一校验暂存源齐全,再备份安装目录现存文件,最后整体覆盖。
/// backup_dir 会被创建;返回后安装目录 = 暂存内容。
pub(crate) fn swap_files(
    install_dir: &Path,
    staging_dir: &Path,
    files: &[String],
    backup_dir: &Path,
) -> Result<(), FtsError> {
    for f in files {
        let src = staging_dir.join(f);
        if !src.is_file() {
            return Err(FtsError::InvalidArg(format!("暂存区缺少文件: {}", src.display())));
        }
        // 拒绝路径穿越/子目录:files 必须是纯文件名,直接落在安装目录
        if f.contains('/') || f.contains('\\') || f.contains("..") {
            return Err(FtsError::InvalidArg(format!("文件名不允许包含路径分隔符: {f}")));
        }
    }
    std::fs::create_dir_all(backup_dir).map_err(FtsError::io_with_hint)?;
    for f in files {
        let dst = install_dir.join(f);
        if dst.is_file() {
            let bdst = backup_dir.join(f);
            if let Some(parent) = bdst.parent() {
                std::fs::create_dir_all(parent).map_err(FtsError::io_with_hint)?;
            }
            std::fs::copy(&dst, &bdst).map_err(FtsError::io)?;
        }
    }
    for f in files {
        let src = staging_dir.join(f);
        let dst = install_dir.join(f);
        std::fs::copy(&src, &dst).map_err(FtsError::io)?;
    }
    Ok(())
}

/// 回滚:备份里有的恢复原样;备份里没有的(本次新增)删除。
pub(crate) fn restore_backup(
    install_dir: &Path,
    backup_dir: &Path,
    files: &[String],
) -> Result<(), FtsError> {
    for f in files {
        let dst = install_dir.join(f);
        let src = backup_dir.join(f);
        if src.is_file() {
            if dst.is_file() {
                std::fs::remove_file(&dst).map_err(FtsError::io)?;
            }
            std::fs::copy(&src, &dst).map_err(FtsError::io)?;
        } else if dst.is_file() {
            std::fs::remove_file(&dst).map_err(FtsError::io)?;
        }
    }
    Ok(())
}

/// 拉起安装目录的 `fts start`(自后台化 + pid 文件 + REST 健康检查)。
///
/// 两个坑(都已实测踩过):
///   * 必须 CREATE_NO_WINDOW —— 本进程无控制台,不加会闪窗;
///   * stdio 必须直写 apply.log 而非用 output() 捕获 —— daemon 孙进程经
///     bInheritHandles=TRUE 继承管道写句柄,output() 将永远等不到 EOF,apply-update 卡死。
fn start_installed(exe: &Path, config: &Option<PathBuf>, log_path: &Path) -> Result<(), FtsError> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(FtsError::io)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("start");
    if let Some(c) = config {
        // Start 子命令的 config 是短参 -c(与 CLI 定义一致)
        if !c.as_os_str().is_empty() {
            cmd.arg("-c").arg(c);
        }
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone().map_err(FtsError::io)?))
        .stderr(std::process::Stdio::from(log));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let status =
        cmd.status().map_err(|e| FtsError::Io(format!("spawn {} 失败: {e}", exe.display())))?;
    if !status.success() {
        let msg = format!("fts start 失败({status}),详见 {}", log_path.display());
        log_line(log_path, &msg);
        return Err(FtsError::Io(msg));
    }
    Ok(())
}

fn log_line(path: &Path, msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[{}] {}", unix_secs(), msg);
    }
}

fn write_result(update_dir: &Path, ok: bool, from: &str, to: &str, files: &[String], error: &str) {
    let body = serde_json::json!({
        "ok": ok,
        "from": from,
        "to": to,
        "time": unix_secs(),
        "files": files,
        "error": error,
    });
    let _ = std::fs::write(update_dir.join("result.json"), body.to_string());
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("fts-update-test-{}-{}", tag, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn put(dir: &Path, name: &str, content: &[u8]) {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn get(dir: &Path, name: &str) -> Vec<u8> {
        std::fs::read(dir.join(name)).unwrap()
    }

    #[test]
    fn swap_replaces_and_keeps_backup() {
        let install = temp_dir("swap-install");
        let staging = temp_dir("swap-staging");
        let backup = temp_dir("swap-backup");
        put(&install, "fts.exe", b"old");
        put(&install, "fts.dll", b"old-dll");
        put(&staging, "fts.exe", b"new");
        put(&staging, "fts.dll", b"new-dll");
        put(&staging, "fts.h", b"new-header");

        swap_files(
            &install,
            &staging,
            &["fts.exe".into(), "fts.dll".into(), "fts.h".into()],
            &backup,
        )
        .unwrap();

        assert_eq!(get(&install, "fts.exe"), b"new");
        assert_eq!(get(&install, "fts.dll"), b"new-dll");
        assert_eq!(get(&install, "fts.h"), b"new-header");
        // 备份只含原有文件
        assert_eq!(get(&backup, "fts.exe"), b"old");
        assert_eq!(get(&backup, "fts.dll"), b"old-dll");
        assert!(!backup.join("fts.h").exists());
    }

    #[test]
    fn swap_fails_cleanly_when_staging_incomplete() {
        let install = temp_dir("swapf-install");
        let staging = temp_dir("swapf-staging");
        let backup = temp_dir("swapf-backup");
        put(&install, "fts.exe", b"old");
        put(&staging, "fts.exe", b"new");
        // fts.dll 缺失 → 整体拒绝,安装目录不动
        let err = swap_files(&install, &staging, &["fts.exe".into(), "fts.dll".into()], &backup);
        assert!(err.is_err());
        assert_eq!(get(&install, "fts.exe"), b"old");
        assert!(!install.join("fts.dll").exists());
    }

    #[test]
    fn swap_rejects_path_traversal() {
        let install = temp_dir("trav-install");
        let staging = temp_dir("trav-staging");
        let backup = temp_dir("trav-backup");
        put(&staging, "evil", b"x");
        let err = swap_files(&install, &staging, &["..\\evil".into()], &backup);
        assert!(err.is_err());
    }

    #[test]
    fn restore_recovers_old_and_removes_new() {
        let install = temp_dir("rest-install");
        let staging = temp_dir("rest-staging");
        let backup = temp_dir("rest-backup");
        put(&install, "fts.exe", b"old");
        put(&staging, "fts.exe", b"new");
        put(&staging, "extra.dll", b"new-extra");
        let files = vec!["fts.exe".to_string(), "extra.dll".to_string()];
        swap_files(&install, &staging, &files, &backup).unwrap();
        assert_eq!(get(&install, "fts.exe"), b"new");
        assert!(install.join("extra.dll").exists());

        restore_backup(&install, &backup, &files).unwrap();
        assert_eq!(get(&install, "fts.exe"), b"old");
        assert!(!install.join("extra.dll").exists());
    }
}
