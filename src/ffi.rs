//! C ABI 最小导出(api-spec 第 5 节子集,P3)。
//! 内存策略:全部返回静态或全局托管字符串,宿主**不得释放**——从根上消除跨边界 free;
//! `fts_last_error()` 返回值在下一次调用前有效(api-spec 5.4 契约:需保留必须拷贝)。
//! 错误码 = CLI 退出码契约(0 成功 / 3 连接 / 4 校验 / 7 认证 …)。

use crate::send::SendCfg;
use crate::tls::PinMode;
use crate::ui::Output;
use std::ffi::{c_char, CStr, CString};
use std::sync::{Mutex, OnceLock};

static LAST_ERROR: Mutex<Option<CString>> = Mutex::new(None);
static EMPTY: OnceLock<CString> = OnceLock::new();

fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("tokio runtime"))
}

fn empty() -> &'static CString {
    EMPTY.get_or_init(|| CString::new(String::new()).expect("empty is valid"))
}

fn set_err(msg: String) {
    if let Ok(mut guard) = LAST_ERROR.lock() {
        *guard = CString::new(msg.replace('\0', " ")).ok();
    }
}

unsafe fn read_str(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY:调用方(FFI 宿主)保证 p 指向有效 NUL 结尾字符串
    Some(unsafe { CStr::from_ptr(p).to_string_lossy().into_owned() })
}

/// fts 产品版本(静态字符串,勿释放)
#[unsafe(no_mangle)]
pub extern "C" fn fts_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// C ABI 版本(结构变化时递增)
#[unsafe(no_mangle)]
pub extern "C" fn fts_abi_version() -> u32 {
    1
}

/// 最近一次错误的可读消息;下次任何 fts_* 调用前有效。无错误时返回空串。
#[unsafe(no_mangle)]
pub extern "C" fn fts_last_error() -> *const c_char {
    match LAST_ERROR.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(s) => s.as_ptr(),
            None => empty().as_ptr(),
        },
        Err(_) => empty().as_ptr(),
    }
}

/// 阻塞式发送文件(P3 最小集成面:提交 → 等待 → 校验退出码)。
///
/// 参数:
///   src_path         UTF-8 源文件路径(必填)
///   dst_addr         "host:port"(必填)
///   fingerprint_hex  对端证书 BLAKE3 指纹 64 hex;可为 NULL(此时必须 insecure != 0)
///   insecure         非 0 = 跳过证书校验(仅测试)
///   streams          并行流数,0 = 4
///   chunk_size       分块字节,0 = 8 MiB
///   transport        "auto"/"quic"/"tcp",NULL 视为 "auto"
///
/// 返回:0 成功;否则为契约退出码(3 连接/4 校验/7 认证/1 其他),详情见 fts_last_error。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fts_send_file(
    src_path: *const c_char,
    dst_addr: *const c_char,
    fingerprint_hex: *const c_char,
    insecure: i32,
    streams: u32,
    chunk_size: u32,
    transport: *const c_char,
) -> i32 {
    let src = unsafe { read_str(src_path) };
    let Some(src) = src else {
        set_err("src_path 不能为空".into());
        return 1;
    };
    let dst = unsafe { read_str(dst_addr) };
    let Some(dst) = dst else {
        set_err("dst_addr 不能为空".into());
        return 1;
    };
    let fp = unsafe { read_str(fingerprint_hex) };
    let pin = match (&fp, insecure != 0) {
        (Some(hex), false) => match crate::tls::decode_fingerprint(hex) {
            Some(bytes) => PinMode::Fingerprint(bytes),
            None => {
                set_err("fingerprint 格式无效,应为 64 位 hex".into());
                return 2;
            }
        },
        (None, true) => PinMode::Insecure,
        (Some(_), true) => {
            set_err("fingerprint 与 insecure 不能同时使用".into());
            return 2;
        }
        (None, false) => {
            set_err("必须提供 fingerprint 或 insecure".into());
            return 2;
        }
    };
    let transport = unsafe { read_str(transport) }.unwrap_or_else(|| "auto".into());

    let cfg = SendCfg {
        paths: vec![src.into()],
        to: dst,
        chunk_size: if chunk_size == 0 { crate::protocol::DEFAULT_CHUNK_SIZE } else { chunk_size },
        streams: if streams == 0 { 4 } else { streams.clamp(1, 64) },
        pin,
        transport,
        ticket: None,
        out: Output::Silent,
        events: None,
    };

    match rt().block_on(crate::send::run_send(cfg)) {
        Ok(()) => {
            set_err(String::new());
            0
        }
        Err(e) => {
            let code = e.exit_code() as i32;
            set_err(format!("{e}"));
            code
        }
    }
}
