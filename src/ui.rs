//! 输出层:人类可读(默认)、NDJSON(api-spec 3.4 契约)或 Silent(daemon,事件走 channel)。
//! 契约:JSON 模式下 stdout 只输出事件,人类日志走 stderr;未知事件类型应被下游忽略。

use crate::error::FtsError;
use std::time::Instant;
use tokio::sync::broadcast;

#[derive(Clone, Copy)]
pub enum Output {
    Human,
    Json,
    /// 不打印(daemon 模式);事件走 events channel
    Silent,
}

impl Output {
    /// 降级事件(契约:永远 warn 级别,不得被默认日志过滤)
    pub fn log_degradation(&self, from: &str, to: &str, reason: &str) {
        let json = format!(
            r#"{{"type":"degradation","from":"{from}","to":"{to}","reason":"{reason}"}}"#
        );
        match self {
            Output::Human => eprintln!("[warn] 降级 {from} → {to}: {reason}"),
            Output::Json => println!("{json}"),
            Output::Silent => {}
        }
    }

    /// 路径事件(探测观测,P2)
    pub fn path(&self, link: &str, rtt_ms: Option<u32>) {
        let json = format!(
            r#"{{"type":"path","link":"{link}","rtt_ms":{}}}"#,
            rtt_ms.map(|v| v.to_string()).unwrap_or_else(|| "null".into())
        );
        match self {
            Output::Human => match rtt_ms {
                Some(ms) => eprintln!("[info] 链路 {link} rtt={ms}ms"),
                None => eprintln!("[info] 链路 {link}"),
            },
            Output::Json => println!("{json}"),
            Output::Silent => {}
        }
    }
}

pub struct Progress {
    pub bytes_done: u64,
    pub bytes_total: u64,
}

pub struct SessionOut {
    pub out: Output,
    pub task_id: String,
    pub started: Instant,
    last_bytes: u64,
    last_instant: Instant,
    /// daemon 模式:所有事件(json 字符串)同步给 REST SSE 订阅者
    events: Option<broadcast::Sender<String>>,
}

impl SessionOut {
    pub fn new(out: Output) -> Self {
        SessionOut {
            out,
            task_id: uuid::Uuid::new_v4().to_string(),
            started: Instant::now(),
            last_bytes: 0,
            last_instant: Instant::now(),
            events: None,
        }
    }

    pub fn with_events(mut self, tx: broadcast::Sender<String>) -> Self {
        self.events = Some(tx);
        self
    }

    /// 无 mut 借位克隆(事件通道 Arc 共享)
    pub fn clone_with_events(&self, tx: broadcast::Sender<String>) -> Self {
        SessionOut {
            out: self.out,
            task_id: self.task_id.clone(),
            started: self.started,
            last_bytes: self.last_bytes,
            last_instant: self.last_instant,
            events: Some(tx),
        }
    }

    fn emit(&self, json: &str) {
        if let Some(tx) = &self.events {
            let _ = tx.send(json.to_string());
        }
    }

    pub fn task_created(&self) {
        let json = format!(
            r#"{{"type":"task","task_id":"{}","state":"RUNNING"}}"#,
            self.task_id
        );
        match self.out {
            Output::Human => {}
            Output::Json => println!("{json}"),
            Output::Silent => {}
        }
        self.emit(&json);
    }

    /// 周期性进度;JSON 模式带吞吐与 ETA
    pub fn progress(&mut self, p: Progress) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_instant).as_secs_f64();
        let throughput = if dt > 0.05 {
            ((p.bytes_done - self.last_bytes) as f64 / dt) as u64
        } else {
            0
        };
        if throughput > 0 {
            self.last_bytes = p.bytes_done;
            self.last_instant = now;
        }
        let eta = if throughput > 0 && p.bytes_total > p.bytes_done {
            Some(((p.bytes_total - p.bytes_done) / throughput.max(1)) as u32)
        } else {
            None
        };
        let json = format!(
            r#"{{"type":"progress","bytes_done":{},"bytes_total":{},"throughput_bps":{},"eta_seconds":{}}}"#,
            p.bytes_done,
            p.bytes_total,
            throughput,
            eta.map(|v| v.to_string()).unwrap_or_else(|| "null".into())
        );
        match self.out {
            Output::Human => {}
            Output::Json => println!("{json}"),
            Output::Silent => {}
        }
        self.emit(&json);
    }

    /// W2:多文件任务内的文件开始事件
    pub fn file(&self, name: &str, index: usize, total: usize) {
        let json = format!(
            r#"{{"type":"file","name":"{name}","index":{index},"total":{total}}}"#
        );
        match self.out {
            Output::Human => eprintln!("send: [{index}/{total}] {name}"),
            Output::Json => println!("{json}"),
            Output::Silent => {}
        }
        self.emit(&json);
    }

    /// W2:多文件任务内的文件完成事件
    pub fn file_done(&self, name: &str, checksum_ok: bool, seconds: f64, bytes: u64) {
        let json = format!(
            r#"{{"type":"file_done","name":"{name}","checksum_ok":{},"seconds":{seconds:.2},"bytes":{bytes}}}"#,
            checksum_ok
        );
        match self.out {
            Output::Human => {}
            Output::Json => println!("{json}"),
            Output::Silent => {}
        }
        self.emit(&json);
    }

    pub fn result(&self, state: &str, checksum_ok: bool, error: Option<&FtsError>) {
        let err = error
            .map(|e| {
                format!(
                    r#","error_code":{},"error":"{}""#,
                    e.exit_code(),
                    e.name()
                )
            })
            .unwrap_or_default();
        let json = format!(
            r#"{{"type":"result","state":"{}","checksum_ok":{},"duration_seconds":{:.2}{}}}"#,
            state,
            checksum_ok,
            self.started.elapsed().as_secs_f64(),
            err
        );
        match self.out {
            Output::Human => {}
            Output::Json => println!("{json}"),
            Output::Silent => {}
        }
        self.emit(&json);
    }

    /// 人类可读日志(stderr;JSON/Silent 模式下也走 stderr 保持 stdout 纯净)
    pub fn log(&self, msg: &str) {
        if !matches!(self.out, Output::Silent) {
            eprintln!("{msg}");
        }
    }

    /// 重置计时起点(收到 manifest / 真正开始传数据时调用,避免把对端准备时间计入吞吐)
    pub fn reset_started(&mut self) {
        self.started = Instant::now();
    }
}
