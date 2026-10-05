//! 统一错误类型与退出码(api-spec 第 9.1 节契约的 P1 子集)。

use std::fmt;

#[derive(Debug, Clone)]
pub enum FtsError {
    /// 3 = 连接失败
    Connect(String),
    /// 3 = 协议版本不匹配(同属连接类)
    Version(String),
    /// 4 = 校验失败,数据不可信
    Checksum(String),
    /// 7 = 认证失败(证书指纹不匹配等)
    Auth(String),
    /// 5 = 传输中断且不可续传
    Interrupted(String),
    /// 6 = 被取消(P1 预留,Ctrl+C 处理接入后启用)
    #[allow(dead_code)]
    Cancelled,
    /// 2 = 参数错误(多文件重名等;CLI 解析错误由 clap 直接处理)
    InvalidArg(String),
    /// 1 = 本地 I/O / 其他错误
    Io(String),
}

impl fmt::Display for FtsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FtsError::Connect(m) => write!(f, "连接失败: {m}"),
            FtsError::Version(m) => write!(f, "协议版本不匹配: {m}"),
            FtsError::Checksum(m) => write!(f, "校验失败: {m}"),
            FtsError::Auth(m) => write!(f, "认证失败: {m}"),
            FtsError::Interrupted(m) => write!(f, "传输中断: {m}"),
            FtsError::Cancelled => write!(f, "被取消"),
            FtsError::InvalidArg(m) => write!(f, "参数错误: {m}"),
            FtsError::Io(m) => write!(f, "I/O 错误: {m}"),
        }
    }
}

impl std::error::Error for FtsError {}

impl FtsError {
    pub fn io(e: std::io::Error) -> FtsError {
        FtsError::Io(e.to_string())
    }

    pub fn exit_code(&self) -> u8 {
        match self {
            FtsError::Connect(_) | FtsError::Version(_) => 3,
            FtsError::Checksum(_) => 4,
            FtsError::Interrupted(_) => 5,
            FtsError::Cancelled => 6,
            FtsError::Auth(_) => 7,
            FtsError::InvalidArg(_) => 2,
            FtsError::Io(_) => 1,
        }
    }

    /// 简短错误名(api-spec 9.1),用于 JSON error 事件。
    pub fn name(&self) -> &'static str {
        match self {
            FtsError::Connect(_) => "CONNECT_FAILED",
            FtsError::Version(_) => "INCOMPATIBLE_VERSION",
            FtsError::Checksum(_) => "CHECKSUM_FAILED",
            FtsError::Auth(_) => "AUTH_FAILED",
            FtsError::Interrupted(_) => "INTERRUPTED",
            FtsError::Cancelled => "CANCELLED",
            FtsError::InvalidArg(_) => "INVALID_ARG",
            FtsError::Io(_) => "IO_ERROR",
        }
    }

    /// I/O 错误 → 带可操作提示的 Io 错误。
    /// 重点翻译网络路径(SMB/UNC)常见错误:fts 不弹凭据框(CLI/服务程序),
    /// 凭据须由调用方预先建立(cmdkey / net use / 域环境自动)。
    pub fn io_with_hint(e: std::io::Error) -> FtsError {
        let hint = match e.raw_os_error() {
            Some(5) => Some("拒绝访问:确认共享与 NTFS 权限;若凭据缺失见 1326 的处理"),
            Some(1326) => Some(
                "登录失败(凭据无效或未提供):网络路径请先建立凭据 —— \
                 `cmdkey /add:<主机> /user:<用户> /pass:<密码>`(推荐,一次即可)或 \
                 `net use \\\\主机\\共享 /user:<用户> <密码>`",
            ),
            Some(53) | Some(67) => {
                Some("找不到网络路径/网络名:主机名或共享名错误,或网络不通;UNC 应形如 \\\\主机\\共享\\子目录")
            }
            Some(1219) => Some(
                "存在到同一网络资源的多个凭据会话:执行 `net use * /delete` 清理后重试",
            ),
            _ => None,
        };
        match hint {
            Some(h) => FtsError::Io(format!("{e} — 提示:{h}")),
            None => FtsError::Io(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_hint_translates_smb_errors() {
        let e = std::io::Error::from_raw_os_error(1326);
        let msg = FtsError::io_with_hint(e).to_string();
        assert!(msg.contains("cmdkey"), "应包含凭据建立提示: {msg}");
        let e = std::io::Error::from_raw_os_error(5);
        let msg = FtsError::io_with_hint(e).to_string();
        assert!(msg.contains("权限"), "应包含权限提示: {msg}");
        let e = std::io::Error::from_raw_os_error(2); // 无提示的普通错误
        let msg = FtsError::io_with_hint(e).to_string();
        assert!(!msg.contains("提示"), "无映射错误不应带提示: {msg}");
    }
}
