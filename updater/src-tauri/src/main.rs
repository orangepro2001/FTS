//! fts 更新器(Tauri 2 桌面应用)。
//! 业务逻辑全部在 fts 库(update::push_update / update::connect_client),
//! 这里只做:参数编排、进度事件转发给前端、主机列表持久化。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use fts::update::{self, PushUpdateCfg};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager};

const DEFAULT_GRPC_PORT: &str = "50051";

/// 主机列表条目(持久化到 app_config_dir/hosts.json)
#[derive(Serialize, Deserialize, Clone)]
struct HostEntry {
    name: String,
    addr: String,
}

#[derive(Serialize, Clone, Default)]
struct ProbeResult {
    addr: String,
    reachable: bool,
    fts_version: String,
    fingerprint: String,
    listen_addr: String,
    started_at: u64,
    error: String,
}

#[derive(Serialize, Clone)]
struct UpdateReportDto {
    ok: bool,
    old_version: String,
    new_version: String,
    detail: String,
}

#[derive(Serialize, Clone)]
struct LogLine {
    addr: String,
    line: String,
}

/// "192.168.1.60" → "192.168.1.60:50051";已带端口则原样
fn normalize_addr(input: &str) -> String {
    let s = input.trim();
    if s.is_empty() || s.contains(':') {
        s.to_string()
    } else {
        format!("{s}:{DEFAULT_GRPC_PORT}")
    }
}

fn hosts_file(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("配置目录不可用: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建配置目录失败: {e}"))?;
    Ok(dir.join("hosts.json"))
}

#[tauri::command]
async fn probe(addr: String) -> ProbeResult {
    let addr = normalize_addr(&addr);
    let mut r = ProbeResult {
        addr: addr.clone(),
        ..Default::default()
    };
    match update::connect_client(&addr).await {
        Ok(mut c) => match update::capabilities(&mut c).await {
            Ok(caps) => {
                r.reachable = true;
                r.fts_version = caps.fts_version;
                r.fingerprint = caps.fingerprint;
                r.listen_addr = caps.listen_addr;
                r.started_at = caps.started_at;
            }
            Err(e) => r.error = e,
        },
        Err(e) => r.error = e,
    }
    r
}

#[tauri::command]
fn read_dist_version(dist_dir: String) -> Option<String> {
    std::fs::read_to_string(PathBuf::from(&dist_dir).join("VERSION"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[tauri::command]
async fn update_host(
    app: AppHandle,
    addr: String,
    dist_dir: String,
    overwrite_config: bool,
    force: bool,
) -> Result<UpdateReportDto, String> {
    let addr = normalize_addr(&addr);
    if !PathBuf::from(&dist_dir).join("fts.exe").is_file() {
        return Err(format!("dist 目录缺少 fts.exe: {dist_dir}"));
    }
    // 进度行 → 前端 "update-log" 事件(按主机 addr 区分)
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);
    let forwarder = {
        let addr = addr.clone();
        let app = app.clone();
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                let _ = app.emit("update-log", LogLine { addr: addr.clone(), line });
            }
        })
    };
    let report = update::push_update(PushUpdateCfg {
        grpc_addr: addr,
        dist_dir: PathBuf::from(&dist_dir),
        overwrite_config,
        force,
        on_event: Some(tx),
    })
    .await;
    let _ = forwarder.await;
    Ok(UpdateReportDto {
        ok: report.ok,
        old_version: report.old_version,
        new_version: report.new_version,
        detail: report.detail,
    })
}

#[tauri::command]
fn load_hosts(app: AppHandle) -> Vec<HostEntry> {
    let Ok(path) = hosts_file(&app) else { return vec![] };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[tauri::command]
fn save_hosts(app: AppHandle, hosts: Vec<HostEntry>) -> Result<(), String> {
    let path = hosts_file(&app)?;
    let body = serde_json::to_string_pretty(&hosts).map_err(|e| e.to_string())?;
    std::fs::write(path, body).map_err(|e| format!("保存主机列表失败: {e}"))
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            probe,
            read_dist_version,
            update_host,
            load_hosts,
            save_hosts
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
