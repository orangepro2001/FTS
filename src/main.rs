//! fts — 高速文件传输(Windows + QUIC)。
//! bin 目标:CLI 与退出码映射;全部逻辑在 lib(fts)中,与 cdylib(DLL)共享。

use clap::{Parser, Subcommand};
use fts::protocol::DEFAULT_CHUNK_SIZE;
use fts::{daemon, recv, send, tls, ui, update};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "fts", version, about = "高速文件传输(QUIC + BLAKE3)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 发送文件(多路径/目录;目录展开为其中直接文件,不递归,展开后不得重名)
    Send {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(long)]
        to: String,
        /// 对端证书指纹(recv 启动时打印的 BLAKE3 hex)
        #[arg(long)]
        fingerprint: Option<String>,
        /// 跳过证书校验(仅测试用,有指纹必填则不用此参数)
        #[arg(long, default_value_t = false)]
        insecure: bool,
        /// 分块大小(字节)
        #[arg(long, default_value_t = DEFAULT_CHUNK_SIZE)]
        chunk_size: u32,
        /// 并行流数
        #[arg(long, default_value_t = 4)]
        streams: u32,
        /// inbox 预注册票据(daemon REST/gRPC 获取;决定对端落盘目录)
        #[arg(long)]
        ticket: Option<String>,
        /// NDJSON 事件输出到 stdout(集成用)
        #[arg(long, default_value_t = false)]
        json: bool,
        /// 传输层:auto=QUIC 失败回退 TCP;quic / tcp 强制
        #[arg(long, default_value = "auto")]
        transport: String,
        #[arg(short = 'v', long, default_value_t = false)]
        verbose: bool,
    },
    /// 接收文件
    Recv {
        #[arg(long)]
        listen: String,
        #[arg(long, default_value = ".")]
        out: PathBuf,
        #[arg(long, default_value_t = false)]
        json: bool,
        /// 传输层:auto=QUIC 与 TCP 同端口双监听;quic / tcp 只启用其一
        #[arg(long, default_value = "auto")]
        transport: String,
        /// 证书缓存目录(指纹跨运行稳定)
        #[arg(long, default_value = ".fts")]
        cert_cache: PathBuf,
        #[arg(short = 'v', long, default_value_t = false)]
        verbose: bool,
    },
    /// 后台启动 daemon(自动读 cwd 的 config.json;自后台化 + 健康检查)
    Start {
        #[arg(short = 'c')]
        config: Option<PathBuf>,
    },
    /// 停止后台 daemon(按 pid 文件)
    Stop {
        #[arg(short = 'c')]
        config: Option<PathBuf>,
    },
    /// 查看 daemon 状态
    Status {
        #[arg(short = 'c')]
        config: Option<PathBuf>,
    },
    /// 启动常驻服务:接收循环 + 本地 REST/gRPC API(前台;后台用 fts start)
    Daemon {
        /// 配置文件(JSON;CLI 显式参数优先于它,它优先于内置默认)
        #[arg(long, short = 'c')]
        config: Option<PathBuf>,
        /// REST 监听地址(仅本地)
        #[arg(long)]
        rest: Option<String>,
        /// 传输监听地址(QUIC+TCP 双栈)
        #[arg(long)]
        listen: Option<String>,
        /// 默认接收目录(无 ticket 的传输落这里)
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        cert_cache: Option<PathBuf>,
        /// 最大并发接收会话(QUIC 并发;TCP 会话天然互斥)
        #[arg(long)]
        max_sessions: Option<usize>,
        /// gRPC 监听地址(空串禁用)
        #[arg(long)]
        grpc: Option<String>,
    },
    /// W9(内部):由 BeginUpdate 从暂存区新版 exe 拉起,执行自替换 + 重启;勿手动调用
    #[command(hide = true)]
    ApplyUpdate {
        /// 旧 daemon 进程 pid
        #[arg(long)]
        pid: u32,
        /// 安装目录(覆盖目标)
        #[arg(long)]
        install: PathBuf,
        /// 暂存目录(新版文件来源)
        #[arg(long)]
        staging: PathBuf,
        /// 要应用的文件名(可重复;必须已在暂存区)
        #[arg(long)]
        files: Vec<String>,
        /// daemon 启动配置(透传给 fts start;缺省按 cwd 解析)
        #[arg(long)]
        config: Option<PathBuf>,
        /// 更新前版本(仅记录)
        #[arg(long)]
        from: String,
    },
    /// 远程推送新版 dist 并触发目标 daemon 自替换重启(W9;目标需已支持 BeginUpdate)
    PushUpdate {
        /// 目标 daemon gRPC 地址(host 或 host:50051)
        #[arg(long)]
        to: String,
        /// 本机新版 dist 目录
        #[arg(long)]
        dist: PathBuf,
        /// 连 config.json 一起覆盖(默认保留目标机配置)
        #[arg(long, default_value_t = false)]
        overwrite_config: bool,
        /// 目标有进行中传输任务时仍强制更新
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    // 控制台默认 GBK,Rust 输出 UTF-8 → 切 65001 防中文乱码
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleOutputCP(65001);
    }
    let cli = Cli::parse();

    let result = match cli.cmd {
        Cmd::Send {
            paths,
            to,
            fingerprint,
            insecure,
            chunk_size,
            streams,
            ticket,
            json,
            transport,
            verbose: _,
        } => {
            let out = if json { ui::Output::Json } else { ui::Output::Human };
            let pin = match (&fingerprint, insecure) {
                (Some(fp), false) => match tls::decode_fingerprint(fp) {
                    Some(bytes) => tls::PinMode::Fingerprint(bytes),
                    None => {
                        eprintln!("错误: --fingerprint 格式无效,应为 64 位 hex");
                        return ExitCode::from(2);
                    }
                },
                (None, true) => tls::PinMode::Insecure,
                (Some(_), true) => {
                    eprintln!("错误: --fingerprint 与 --insecure 不能同时使用");
                    return ExitCode::from(2);
                }
                (None, false) => {
                    eprintln!("错误: 必须提供 --fingerprint <对端指纹>(recv 启动时打印)或 --insecure");
                    return ExitCode::from(2);
                }
            };
            send::run_send(send::SendCfg {
                paths,
                to,
                chunk_size,
                streams,
                pin,
                transport,
                ticket,
                out,
                events: None,
            })
            .await
        }
        Cmd::Recv { listen, out: out_dir, json, transport, cert_cache, verbose: _ } => {
            let out = if json { ui::Output::Json } else { ui::Output::Human };
            recv::run_recv(recv::RecvCfg {
                listen,
                out_dir,
                out,
                transport,
                cert_cache,
                events: None,
                inbox: None,
            })
            .await
        }
        Cmd::Start { config } => daemon::ctl_start(config).map(|_| ()),
        Cmd::Stop { config } => daemon::ctl_stop(config).map(|_| ()),
        Cmd::Status { config } => daemon::ctl_status(config).map(|_| ()),
        Cmd::Daemon { config, rest, listen, out, cert_cache, max_sessions, grpc } => {
            match build_daemon_cfg(config, rest, listen, out, cert_cache, max_sessions, grpc) {
                Ok(cfg) => daemon::run(cfg).await,
                Err(e) => Err(e),
            }
        }
        Cmd::ApplyUpdate { pid, install, staging, files, config, from } => {
            update::apply_update(update::ApplyUpdateCfg {
                pid,
                install_dir: install,
                staging_dir: staging,
                files,
                config,
                from_version: from,
            })
            .map(|_| ())
        }
        Cmd::PushUpdate { to, dist, overwrite_config, force } => {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);
            let printer = tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    println!("{line}");
                }
            });
            let report = update::push_update(update::PushUpdateCfg {
                grpc_addr: to,
                dist_dir: dist,
                overwrite_config,
                force,
                on_event: Some(tx),
            })
            .await;
            let _ = printer.await;
            if report.ok {
                println!("更新成功: {} → {}", report.old_version, report.new_version);
                Ok(())
            } else {
                Err(fts::error::FtsError::Io(report.detail))
            }
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("错误: {e}");
            ExitCode::from(e.exit_code())
        }
    }
}

/// daemon 参数合并:CLI 显式参数 > 配置文件 > 内置默认
fn build_daemon_cfg(
    config: Option<PathBuf>,
    rest: Option<String>,
    listen: Option<String>,
    out: Option<PathBuf>,
    cert_cache: Option<PathBuf>,
    max_sessions: Option<usize>,
    grpc: Option<String>,
) -> Result<fts::daemon::DaemonCfg, fts::error::FtsError> {
    let fc = match &config {
        Some(p) => fts::daemon::load_file_config(p)?,
        None => fts::daemon::FileConfig::default(),
    };
    let rest_addr = rest.or(fc.rest_addr).unwrap_or_else(|| "127.0.0.1:5050".into());
    let listen_addr = listen.or(fc.listen_addr).unwrap_or_else(|| "0.0.0.0:4433".into());
    let out_dir = out.or(fc.out_dir).unwrap_or_else(|| PathBuf::from("."));
    let cert_cache = cert_cache.or(fc.cert_cache).unwrap_or_else(|| PathBuf::from(".fts"));
    let max_sessions = max_sessions.or(fc.max_sessions).unwrap_or(3);
    let grpc_raw = grpc.or(fc.grpc_addr).unwrap_or_else(|| "127.0.0.1:50051".into());
    let grpc_addr = if grpc_raw.trim().is_empty() {
        None
    } else {
        Some(grpc_raw.parse().map_err(|e| {
            fts::error::FtsError::InvalidArg(format!("gRPC 地址无效 {grpc_raw}: {e}"))
        })?)
    };
    Ok(fts::daemon::DaemonCfg {
        rest: rest_addr,
        listen: listen_addr,
        out_dir,
        cert_cache,
        max_sessions,
        grpc: grpc_addr,
        // W9:记录启动配置路径,BeginUpdate 拉起 apply-update 时透传给 fts start
        config_path: config.unwrap_or_default(),
    })
}

