//! 网络凭据会话(W6):目标为需要认证的 SMB/UNC 路径时,fts 代为建立 Windows 会话
//! (WNetAddConnection2W,会话级 —— 进程生命周期内有效,不持久化、不写入任何日志)。
//!
//! 安全边界:
//! - 凭据仅经 localhost 的 gRPC/REST 传输(gRPC 明文 —— 不要暴露端口);
//! - 密码不进入任何错误消息、日志、事件;
//! - fts 不存储凭据;OS 会话随进程结束失效;
//! - 凭据管理主渠道仍是 Windows 凭据管理器(cmdkey),本接口是集成方代建立会话的通道。

use crate::error::FtsError;
use std::os::windows::ffi::OsStrExt;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct NetworkCredential {
    pub user_name: String,
    pub password: String,
    #[serde(default)]
    pub domain: String,
}

/// 从 UNC 路径提取 `\\host\share` 根(忽略大小写比较由 OS 完成)。
/// 非 UNC(不以 \\ 开头)返回 None。
pub fn parse_unc_root(path: &str) -> Option<String> {
    let p = path.trim_start_matches('?'); // \\?\UNC\ 前缀形态不管,只处理裸 UNC
    let p = p.trim();
    let rest = p.strip_prefix(r"\\")?;
    let mut it = rest.splitn(3, '\\');
    let host = it.next()?;
    let share = it.next()?;
    if host.is_empty() || share.is_empty() {
        return None;
    }
    Some(format!(r"\\{host}\{share}"))
}

fn to_wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// 建立到 UNC 根的网络会话。已存在会话且凭据不同时 OS 返回 1219(冲突),
/// 此时不动已有会话,报错并提示(会话可能被其他进程使用,不得代为断开)。
pub fn establish_connection(out_dir: &str, cred: &NetworkCredential) -> Result<(), FtsError> {
    let remote = parse_unc_root(out_dir).ok_or_else(|| {
        FtsError::InvalidArg(
            "credential 仅支持 UNC 网络路径(形如 \\\\主机\\共享\\...);本地路径无需凭据".into(),
        )
    })?;

    let mut remote_w = to_wide(&remote);
    // 用户名可带域:域字段非空时拼 "domain\user";不覆盖 user_name 内已带的写法
    let user_full = if cred.domain.is_empty() || cred.user_name.contains('\\') {
        cred.user_name.clone()
    } else {
        format!("{}\\{}", cred.domain, cred.user_name)
    };
    let user_w = to_wide(&user_full);
    let pass_w = to_wide(&cred.password);

    let mut nr = windows_sys::Win32::NetworkManagement::WNet::NETRESOURCEW {
        dwScope: 0,
        dwType: windows_sys::Win32::NetworkManagement::WNet::RESOURCETYPE_DISK,
        dwDisplayType: 0,
        dwUsage: 0,
        lpLocalName: std::ptr::null_mut(), // 不映射盘符
        lpRemoteName: remote_w.as_mut_ptr(),
        lpComment: std::ptr::null_mut(),
        lpProvider: std::ptr::null_mut(),
    };
    let rc = unsafe {
        windows_sys::Win32::NetworkManagement::WNet::WNetAddConnection2W(
            &mut nr,
            pass_w.as_ptr(),
            user_w.as_ptr(),
            windows_sys::Win32::NetworkManagement::WNet::CONNECT_TEMPORARY, // 会话级,不写 profile
        )
    };
    match rc {
        0 => Ok(()),
        1219 => Err(FtsError::Auth(format!(
            "网络会话冲突(1219):到 {remote} 已存在其他凭据的连接;先 `net use \\\\{remote} /delete` 或改用该会话已有的凭据"
        ))),
        1326 => Err(FtsError::Auth(
            "网络登录失败(1326):用户名/密码/域不正确".into(),
        )),
        5 => Err(FtsError::Auth("网络访问被拒绝(5):账号无该共享权限".into())),
        53 | 67 => Err(FtsError::Connect(format!(
            "找不到网络路径({rc}):主机或共享名错误,或网络不通({remote})"
        ))),
        other => Err(FtsError::Auth(format!(
            "建立网络会话失败(WNet 错误 {other})"
        ))),
    }
}

/// 脱敏显示凭据(日志/事件用):绝不含密码
pub fn describe(cred: &NetworkCredential) -> String {
    let who = if cred.domain.is_empty() {
        cred.user_name.clone()
    } else {
        format!("{}\\{}", cred.domain, cred.user_name)
    };
    format!("user={who} password=***")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unc_root_extraction() {
        assert_eq!(parse_unc_root(r"\\nas01\share\fts"), Some(r"\\nas01\share".to_string()));
        assert_eq!(parse_unc_root(r"\\nas01\share"), Some(r"\\nas01\share".to_string()));
        assert_eq!(parse_unc_root(r"\\nas01\share\a\b\c.bin"), Some(r"\\nas01\share".to_string()));
        assert_eq!(parse_unc_root(r"C:\local\path"), None);
        assert_eq!(parse_unc_root(r"relative\path"), None);
        assert_eq!(parse_unc_root(r"\\host\"), None); // 缺 share
        assert_eq!(parse_unc_root(r"\\\share"), None); // 缺 host
    }

    #[test]
    fn describe_never_contains_password() {
        let c = NetworkCredential {
            user_name: "svc".into(),
            password: "SECRET-PW".into(),
            domain: "CORP".into(),
        };
        let d = describe(&c);
        assert!(!d.contains("SECRET-PW"));
        assert!(d.contains("CORP\\svc"));
    }

    #[test]
    fn establish_rejects_local_path() {
        let c = NetworkCredential {
            user_name: "u".into(),
            password: "p".into(),
            domain: String::new(),
        };
        let err = establish_connection(r"C:\local", &c).unwrap_err();
        assert!(matches!(err, FtsError::InvalidArg(_)));
        assert!(!err.to_string().contains('p')); // 密码不泄漏
    }
}
