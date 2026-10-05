//! 路径探测:观测当前链路类型(design 第 4 节,Tailscale 感知)。
//! P2 范围:仅观测 + 事件记录,不参与切换决策;Tailscale 缺失/超时返回 Unknown。

use std::time::Duration;
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkType {
    Direct,
    PeerRelay,
    Derp,
    Unknown,
}

impl LinkType {
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkType::Direct => "direct",
            LinkType::PeerRelay => "peer_relay",
            LinkType::Derp => "derp",
            LinkType::Unknown => "unknown",
        }
    }
}

/// 探测对端路径。输出 (链路类型, RTT ms)。
/// 命令:`tailscale ping --c 1 --timeout 2s <ip>`;探测不得干扰传输,限时 3 秒。
pub async fn probe_peer(ip: &str) -> (LinkType, Option<u32>) {
    let probe = async {
        let out = Command::new("tailscale")
            .args(["ping", "--c", "1", "--timeout", "2s", ip])
            .output()
            .await?;
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    match tokio::time::timeout(Duration::from_secs(3), probe).await {
        Ok(Ok(text)) => parse_ping(&text),
        _ => (LinkType::Unknown, None),
    }
}

/// 解析 `tailscale ping` 输出。示例:
///   pong from 100.64.0.1:41641 via 1.2.3.4:41641 in 12ms     → 直连
///   pong from 100.64.0.1:41641 via DERP(den) in 150ms        → DERP 中继
///   pong from 100.64.0.1:41641 via peer-relay(...) in 42ms   → Peer Relay
///   ping failed / 空输出                                      → Unknown
pub fn parse_ping(text: &str) -> (LinkType, Option<u32>) {
    let mut link = LinkType::Unknown;
    let mut rtt = None;
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("pong") {
            continue;
        }
        if line.contains("via DERP") {
            link = LinkType::Derp;
        } else if line.contains("peer-relay") {
            link = LinkType::PeerRelay;
        } else if line.contains(" via ") {
            link = LinkType::Direct;
        }
        // "in 12ms" → 12
        if let Some(idx) = line.find(" in ") {
            let tail = &line[idx + 4..];
            let num: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(ms) = num.parse::<u32>() {
                rtt = Some(ms);
            }
        }
        if link != LinkType::Unknown {
            break; // 首个 pong 即判;Tailscale 会持续升级路径,多轮探测可观测升级过程
        }
    }
    (link, rtt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_direct() {
        let (link, rtt) = parse_ping("pong from 100.64.0.1:41641 via 1.2.3.4:41641 in 12ms");
        assert_eq!(link, LinkType::Direct);
        assert_eq!(rtt, Some(12));
    }

    #[test]
    fn parse_derp() {
        let (link, rtt) = parse_ping("pong from 100.64.0.1:41641 via DERP(den) in 150ms");
        assert_eq!(link, LinkType::Derp);
        assert_eq!(rtt, Some(150));
    }

    #[test]
    fn parse_peer_relay() {
        let (link, rtt) = parse_ping("pong from 100.64.0.1:41641 via peer-relay(ts-pr-1) in 42ms");
        assert_eq!(link, LinkType::PeerRelay);
        assert_eq!(rtt, Some(42));
    }

    #[test]
    fn parse_failure() {
        let (link, rtt) = parse_ping("ping failed: no matching peer");
        assert_eq!(link, LinkType::Unknown);
        assert_eq!(rtt, None);
        let (link, rtt) = parse_ping("");
        assert_eq!(link, LinkType::Unknown);
        assert_eq!(rtt, None);
    }
}
