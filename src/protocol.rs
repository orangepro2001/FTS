//! 线协议 fts-protocol v1 的 P1 子集(api-spec 第 8 节)。
//! 帧 = u32 LE 长度前缀 + 载荷;控制帧载荷为 JSON,分块帧为裸字节。
//! P1 新增:Hello 能力协商、分块哈希表 + 单层 Merkle 根、续传位图。
//! v2 新增(fts-more-fast):Hello.caps 能力位、控制帧可选 slot(文件级并发流水线)、
//! 数据流 v2 前导 [slot: u32 LE]。Sequential 模式字节级与 v1 相同。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const ALPN: &[u8] = b"fts/1";
pub const PROTOCOL_VERSION: u32 = 2;
pub const DEFAULT_CHUNK_SIZE: u32 = 8 * 1024 * 1024;
/// 控制帧上限(manifest 含分块哈希表,按 10GB 文件 ≈ 1MB 量级留余量)
pub const MAX_CTRL_FRAME: usize = 16 * 1024 * 1024;
/// 分块帧头:[chunk_id: u32 LE][offset: u64 LE][len: u32 LE]
pub const CHUNK_HEADER: usize = 4 + 8 + 4;
/// v2 数据流前导:[slot: u32 LE](仅 Concurrent 模式)
pub const SLOT_PREAMBLE: usize = 4;
/// Hello.caps 能力位:bit0 = 支持槽位化多文件流水线
pub const CAP_MULTIFILE: u32 = 1 << 0;
/// 文件级并发默认值(0 = auto 时的生效值)
pub const DEFAULT_FILE_CONCURRENCY: u32 = 4;

#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub fts_version: String,
    pub role: String, // "source" | "dest"
    /// v2 能力位图(#[serde(default)]:v1 对端帧无此字段时按 0 解析)
    #[serde(default)]
    pub caps: u32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Manifest {
    pub file_name: String,
    pub file_size: u64,
    pub chunk_size: u32,
    pub checksum_algo: String, // 固定 "blake3"
    /// 每块 BLAKE3 hex;单层 Merkle:root = blake3(全部分块哈希拼接)
    pub chunk_hashes: Vec<String>,
    pub merkle_root: String,
    /// inbox 预注册票据(P5):接收端查表决定落盘目录;None = 落默认 --out
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    /// v2:槽位号(文件级并发);None = Sequential(字节级与 v1 相同)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct ManifestAck {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 续传:接收端已校验通过的分块位图,hex(每 bit 一个分块,低位在前)。None = 从头传。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_bitmap_hex: Option<String>,
    /// v2:槽位号(见 Manifest.slot)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct Finish {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct FinishAck {
    pub checksum_ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// v2:槽位号(见 Manifest.slot)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    w.write_all(&(data.len() as u32).to_le_bytes()).await?;
    w.write_all(data).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, cap: usize) -> Result<Vec<u8>> {
    let mut hdr = [0u8; 4];
    r.read_exact(&mut hdr).await?;
    let len = u32::from_le_bytes(hdr) as usize;
    if len > cap {
        bail!("帧超长: {len} > {cap}");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

/// (分块帧头在 recv_stream 内联解析,便于统一 QUIC/TCP 的 EOF 语义)

/// 校验通过的分块位图(每 bit 一个分块)。
#[derive(Clone)]
pub struct Bitmap {
    pub bits: Vec<bool>,
}

impl Bitmap {
    pub fn new(chunks: usize) -> Self {
        Bitmap { bits: vec![false; chunks] }
    }

    pub fn set(&mut self, id: usize) {
        if id < self.bits.len() && !self.bits[id] {
            self.bits[id] = true;
        }
    }

    pub fn get(&self, id: usize) -> bool {
        self.bits.get(id).copied().unwrap_or(false)
    }

    pub fn count(&self) -> u64 {
        self.bits.iter().filter(|b| **b).count() as u64
    }

    pub fn is_complete(&self) -> bool {
        self.bits.iter().all(|b| *b)
    }

    pub fn to_hex(&self) -> String {
        let mut bytes = vec![0u8; self.bits.len().div_ceil(8)];
        for (i, b) in self.bits.iter().enumerate() {
            if *b {
                bytes[i / 8] |= 1 << (i % 8);
            }
        }
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(hex: &str, chunks: usize) -> Self {
        let mut bits = vec![false; chunks];
        let bytes = hex.as_bytes();
        for i in 0..chunks {
            if let (Some(hi), Some(lo)) = (bytes.get((i / 8) * 2), bytes.get((i / 8) * 2 + 1)) {
                let byte = u8::from_str_radix(
                    std::str::from_utf8(&[*hi, *lo]).unwrap_or("00"),
                    16,
                )
                .unwrap_or(0);
                bits[i] = (byte >> (i % 8)) & 1 == 1;
            }
        }
        Bitmap { bits }
    }
}

/// 单层 Merkle:root = blake3(所有分块哈希的原字节顺序拼接)
pub fn merkle_root(chunk_hashes: &[[u8; 32]]) -> String {
    let mut hasher = blake3::Hasher::new();
    for h in chunk_hashes {
        hasher.update(h);
    }
    hasher.finalize().to_hex().to_string()
}

/// 校验对端 Hello;不匹配时返回错误信息
pub fn check_hello(hello: &Hello) -> Result<()> {
    if hello.protocol_version != PROTOCOL_VERSION {
        bail!(
            "对端协议版本 {}, 本端 {}, 需升级",
            hello.protocol_version,
            PROTOCOL_VERSION
        );
    }
    Ok(())
}

/// 传输模式:Sequential 字节级与 v1 相同;Concurrent 控制帧带 slot、数据流带槽位前导
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransferMode {
    Sequential,
    Concurrent,
}

/// 模式协商:仅 QUIC 且对端宣告 MULTIFILE 才并发;TCP 回退/能力缺失自动退回串行。
/// 两端各算一遍,规则一致即对齐(无需额外协商帧)。
pub fn negotiate_mode(is_quic: bool, peer_caps: u32) -> TransferMode {
    if is_quic && peer_caps & CAP_MULTIFILE != 0 {
        TransferMode::Concurrent
    } else {
        TransferMode::Sequential
    }
}

/// v2 数据流前导编码:[slot: u32 LE]
pub fn slot_preamble(slot: u32) -> [u8; SLOT_PREAMBLE] {
    slot.to_le_bytes()
}

/// v2 数据流前导解码
pub fn parse_slot_preamble(bytes: &[u8]) -> Result<u32> {
    let arr: [u8; SLOT_PREAMBLE] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("槽位前导长度不足 {}", bytes.len()))?;
    Ok(u32::from_le_bytes(arr))
}

/// W8:校验 manifest.file_name 的相对路径(递归传输)并转为落盘 PathBuf。
/// 信任边界:file_name 来自网络,必须拒绝路径穿越与平台逃逸:
///   禁止:空、'..'、'.'、空组件、反斜杠、盘符/冒号、绝对路径形态。
/// 组件间只接受 '/'(协议约定,见 W8);返回的 PathBuf 直接 join 到接收根目录之下。
pub fn sanitize_rel_path(name: &str) -> Result<std::path::PathBuf> {
    if name.is_empty() {
        bail!("文件名为空");
    }
    let mut pb = std::path::PathBuf::new();
    for comp in name.split('/') {
        if comp.is_empty() || comp == "." || comp == ".." {
            bail!("非法路径组件: {name:?}");
        }
        if comp.contains('\\') || comp.contains(':') {
            bail!("路径组件含非法字符(\\ 或 :): {name:?}");
        }
        pb.push(comp);
    }
    Ok(pb)
}

pub fn parse_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).context("JSON 帧解析失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 帧编解码往返:长度前缀 + JSON 载荷,在内存 buffer 上模拟流
    #[tokio::test]
    async fn frame_roundtrip() {
        let mut buf = std::io::Cursor::new(Vec::new());
        let hello = Hello {
            protocol_version: PROTOCOL_VERSION,
            fts_version: "0.1.0".into(),
            role: "source".into(),
            caps: CAP_MULTIFILE,
        };
        write_frame(&mut buf, &serde_json::to_vec(&hello).unwrap()).await.unwrap();
        buf.set_position(0);
        let bytes = read_frame(&mut buf, 4096).await.unwrap();
        let back: Hello = parse_json(&bytes).unwrap();
        assert_eq!(back.protocol_version, PROTOCOL_VERSION);
        assert_eq!(back.role, "source");
    }

    #[tokio::test]
    async fn frame_cap_enforced() {
        let mut buf = std::io::Cursor::new(Vec::new());
        write_frame(&mut buf, &vec![0u8; 100]).await.unwrap();
        buf.set_position(0);
        let err = read_frame(&mut buf, 16).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("帧超长"));
    }

    #[test]
    fn bitmap_roundtrip() {
        let mut bm = Bitmap::new(70); // 跨多个字节
        bm.set(0);
        bm.set(8);
        bm.set(63);
        bm.set(64);
        bm.set(69);
        let hex = bm.to_hex();
        let mut back = Bitmap::from_hex(&hex, 70);
        assert!(back.get(0) && back.get(8) && back.get(63) && back.get(64) && back.get(69));
        assert!(!back.get(1) && !back.get(62) && !back.get(68));
        assert_eq!(back.count(), 5);
        assert!(!back.is_complete());
        for i in 0..70 {
            back.set(i);
        }
        let full = Bitmap::from_hex(&back.to_hex(), 70);
        assert!(full.is_complete());
        assert_eq!(full.count(), 70);
    }

    #[test]
    fn bitmap_from_hex_short_input() {
        // 位图截断/损坏时不 panic,缺省为未收到
        let bm = Bitmap::from_hex("ff", 70);
        assert_eq!(bm.count(), 8);
        assert!(!bm.get(69));
    }

    #[test]
    fn merkle_root_matches_chunk_hashes() {
        let h1 = *blake3::hash(b"chunk1").as_bytes();
        let h2 = *blake3::hash(b"chunk2").as_bytes();
        let root = merkle_root(&[h1, h2]);
        let mut hasher = blake3::Hasher::new();
        hasher.update(&h1);
        hasher.update(&h2);
        assert_eq!(root, hasher.finalize().to_hex().to_string());
        // 顺序敏感
        assert_ne!(merkle_root(&[h2, h1]), root);
    }

    #[test]
    fn hello_version_rejected() {
        let bad = Hello {
            protocol_version: PROTOCOL_VERSION + 1,
            fts_version: "x".into(),
            role: "source".into(),
            caps: 0,
        };
        assert!(check_hello(&bad).is_err());
    }

    #[test]
    fn hello_v1_frame_parses_with_default_caps() {
        // v1 对端帧无 caps 字段 → serde default 解析为 0(向后反序列化)
        let v1_json = r#"{"protocol_version":2,"fts_version":"0.1.0","role":"source"}"#;
        let h: Hello = serde_json::from_str(v1_json).unwrap();
        assert_eq!(h.caps, 0);
        assert_eq!(negotiate_mode(true, h.caps), TransferMode::Sequential);
    }

    #[test]
    fn negotiate_matrix() {
        // 协商矩阵:仅 QUIC + MULTIFILE → Concurrent;其余全 Sequential
        assert_eq!(negotiate_mode(true, CAP_MULTIFILE), TransferMode::Concurrent);
        assert_eq!(negotiate_mode(false, CAP_MULTIFILE), TransferMode::Sequential);
        assert_eq!(negotiate_mode(true, 0), TransferMode::Sequential);
        assert_eq!(negotiate_mode(false, 0), TransferMode::Sequential);
        // 其他能力位不影响
        assert_eq!(negotiate_mode(true, 0b1010), TransferMode::Sequential);
        assert_eq!(negotiate_mode(true, 0b1011), TransferMode::Concurrent);
    }

    #[test]
    fn v1_control_frames_parse_without_slot() {
        // v1 帧(无 slot)→ slot=None;v2 串行帧字节级相同
        let m: Manifest = serde_json::from_str(
            r#"{"file_name":"a.bin","file_size":1,"chunk_size":8,"checksum_algo":"blake3",
                "chunk_hashes":["aa"],"merkle_root":"bb"}"#,
        )
        .unwrap();
        assert_eq!(m.slot, None);
        assert_eq!(m.ticket, None);
        let a: ManifestAck = serde_json::from_str(r#"{"ok":true}"#).unwrap();
        assert_eq!(a.slot, None);
        let f: Finish = serde_json::from_str("{}").unwrap();
        assert_eq!(f.slot, None);
        let fa: FinishAck = serde_json::from_str(r#"{"checksum_ok":true}"#).unwrap();
        assert_eq!(fa.slot, None);
        // v2 串行序列化不含 slot 键(字节级兼容 v1)
        let ser = serde_json::to_vec(&Finish { slot: None }).unwrap();
        assert_eq!(ser, b"{}");
        let ser = serde_json::to_vec(&Manifest {
            file_name: "a".into(),
            file_size: 0,
            chunk_size: 8,
            checksum_algo: "blake3".into(),
            chunk_hashes: vec![],
            merkle_root: "r".into(),
            ticket: None,
            slot: None,
        })
        .unwrap();
        assert!(!serde_json::from_slice::<serde_json::Value>(&ser)
            .unwrap()
            .get("slot")
            .is_some());
    }

    #[test]
    fn slot_preamble_roundtrip() {
        for slot in [0u32, 1, 15, u32::MAX] {
            let p = slot_preamble(slot);
            assert_eq!(p.len(), SLOT_PREAMBLE);
            assert_eq!(parse_slot_preamble(&p).unwrap(), slot);
        }
        assert!(parse_slot_preamble(&[0, 0, 0]).is_err());
    }

    #[test]
    fn rel_path_sanitizing() {
        // 合法:多层相对路径
        let p = sanitize_rel_path("sub/dir/file.bin").unwrap();
        assert_eq!(p, std::path::PathBuf::from("sub").join("dir").join("file.bin"));
        assert_eq!(sanitize_rel_path("a.bin").unwrap(), std::path::PathBuf::from("a.bin"));
        // 穿越/逃逸:全部拒绝
        for bad in [
            "../evil.bin",
            "a/../../evil.bin",
            "..",
            "a/../b.bin",
            "a\\b.bin",        // 反斜杠(Windows 分隔符混入)
            "C:/x.bin",        // 盘符
            "a:/x.bin",        // 伪盘符
            "",                // 空
            "a//b.bin",        // 空组件
            "/abs/path",       // 绝对形态
            "a/./b.bin",       // 当前目录组件
        ] {
            assert!(sanitize_rel_path(bad).is_err(), "应拒绝: {bad:?}");
        }
    }
}
