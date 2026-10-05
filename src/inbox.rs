//! inbox 预注册(P5 W1):接收目录 per-task 分配。
//! 调用方(REST/gRPC)注册 {out_dir, file_name?} 拿一次性 token;
//! 发送端把 token 放进 Manifest.ticket;接收端查表决定落盘目录。
//! 语义(见 plan/fts-p5-integration-design.md 第 2 节):
//!   - 一次性:匹配成功即焚;
//!   - 有效期 10 分钟;
//!   - file_name = Some → 绑定精确文件名(防挪用);None → 目录模式,任意文件名;
//!   - 无 ticket → 落 daemon 默认 --out(完全向后兼容)。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const TICKET_TTL: Duration = Duration::from_secs(600);

#[derive(Clone, Debug)]
pub struct InboxEntry {
    pub out_dir: PathBuf,
    /// None = 目录模式(任意文件名)
    pub file_name: Option<String>,
    pub expires_at: Instant,
}

#[derive(Default)]
pub struct InboxRegistry {
    inner: Mutex<HashMap<String, InboxEntry>>,
}

impl InboxRegistry {
    pub fn new() -> Self {
        InboxRegistry { inner: Mutex::new(HashMap::new()) }
    }

    /// 注册;返回 token。file_name=None 为目录模式。
    pub fn reserve(&self, out_dir: PathBuf, file_name: Option<String>) -> String {
        let token = Uuid::new_v4().to_string();
        self.inner.lock().unwrap().insert(
            token.clone(),
            InboxEntry { out_dir, file_name, expires_at: Instant::now() + TICKET_TTL },
        );
        token
    }

    /// 撤销(REST DELETE /v1/inbox/{token})
    pub fn revoke(&self, token: &str) -> bool {
        self.inner.lock().unwrap().remove(token).is_some()
    }

    /// W6:purge 授权检查 —— 只允许**目录模式** token(文件绑定 token 的授权范围
    /// 只是一个文件,不得触发整目录删除);不焚毁(传输还要用)。
    /// 返回 Err(("授权原因", 是否因 token 无效/过期 —— 无效时顺手焚毁防枚举))
    pub fn peek_purgeable(&self, token: &str) -> Result<PathBuf, (&'static str, bool)> {
        let mut map = self.inner.lock().unwrap();
        let Some(entry) = map.get(token) else {
            return Err(("ticket 无效或已使用", true));
        };
        if Instant::now() > entry.expires_at {
            map.remove(token);
            return Err(("ticket 已过期", true));
        }
        if entry.file_name.is_some() {
            return Err(("文件绑定 token 不允许 purge(授权范围仅为单个文件)", false));
        }
        Ok(entry.out_dir.clone())
    }

    /// 兑换:校验 token 有效性(存在、未过期、文件名匹配)→ 返回落盘目录。
    /// 文件绑定模式(file_name=Some):一次性,兑换即焚毁;
    /// 目录模式(file_name=None):TTL 内可复用(批量多文件走同一 token)。
    pub fn redeem(&self, token: &str, file_name: &str) -> Result<PathBuf, &'static str> {
        let mut map = self.inner.lock().unwrap();
        let Some(entry) = map.get(token) else {
            return Err("ticket 无效或已使用");
        };
        if Instant::now() > entry.expires_at {
            map.remove(token);
            return Err("ticket 已过期");
        }
        match &entry.file_name {
            Some(expect) if expect != file_name => {
                map.remove(token); // 错误使用也焚毁,防枚举
                Err("ticket 与文件名不匹配")
            }
            Some(_) => {
                let out_dir = entry.out_dir.clone();
                map.remove(token); // 文件绑定:一次性
                Ok(out_dir)
            }
            None => Ok(entry.out_dir.clone()), // 目录模式:可复用
        }
    }

    /// 清理过期项(定期调用;不调也只会缓慢泄漏,量级极小)
    pub fn gc(&self) {
        let now = Instant::now();
        self.inner.lock().unwrap().retain(|_, e| e.expires_at > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redeem_once_then_gone() {
        let reg = InboxRegistry::new();
        let t = reg.reserve(PathBuf::from("D:\\x"), Some("a.bin".into()));
        assert_eq!(reg.redeem(&t, "a.bin").unwrap(), PathBuf::from("D:\\x"));
        assert!(reg.redeem(&t, "a.bin").is_err()); // 文件绑定模式:一次性
    }

    #[test]
    fn dir_token_reusable() {
        let reg = InboxRegistry::new();
        let t = reg.reserve(PathBuf::from("D:\\x"), None);
        assert!(reg.redeem(&t, "a.bin").is_ok());
        assert!(reg.redeem(&t, "b.bin").is_ok()); // 目录模式:TTL 内可复用(批量)
    }

    #[test]
    fn filename_binding_rejects_mismatch() {
        let reg = InboxRegistry::new();
        let t = reg.reserve(PathBuf::from("D:\\x"), Some("a.bin".into()));
        assert!(reg.redeem(&t, "b.bin").is_err());
    }

    #[test]
    fn dir_mode_accepts_any_name() {
        let reg = InboxRegistry::new();
        let t = reg.reserve(PathBuf::from("D:\\x"), None);
        assert!(reg.redeem(&t, "whatever.bin").is_ok());
    }

    #[test]
    fn revoke_works() {
        let reg = InboxRegistry::new();
        let t = reg.reserve(PathBuf::from("D:\\x"), None);
        assert!(reg.revoke(&t));
        assert!(reg.redeem(&t, "a.bin").is_err());
    }

    #[test]
    fn purge_authorized_only_for_dir_mode() {
        let reg = InboxRegistry::new();
        let dir_token = reg.reserve(PathBuf::from("D:\\x"), None);
        let file_token = reg.reserve(PathBuf::from("D:\\y"), Some("a.bin".into()));
        // 目录模式:允许,且不焚毁(redeem 仍可用)
        assert_eq!(reg.peek_purgeable(&dir_token).unwrap(), PathBuf::from("D:\\x"));
        assert!(reg.redeem(&dir_token, "a.bin").is_ok());
        // 文件绑定:拒绝且不焚毁
        let (msg, burned) = reg.peek_purgeable(&file_token).unwrap_err();
        assert!(msg.contains("不允许 purge"));
        assert!(!burned);
        assert!(reg.redeem(&file_token, "a.bin").is_ok());
    }

    #[test]
    fn purge_invalid_token_reports_invalid() {
        let reg = InboxRegistry::new();
        let t = reg.reserve(PathBuf::from("D:\\x"), Some("a.bin".into()));
        // 陌生 token:invalid=true(HTTP 404 语义;"焚毁防枚举"只针对错误使用的已知 token)
        let (_, invalid) = reg.peek_purgeable("nonexistent").unwrap_err();
        assert!(invalid);
        let _ = t;
    }
}
