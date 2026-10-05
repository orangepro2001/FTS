//! fts 库入口:lib 目标(cdylib 输出 fts.dll;rlib 供 bin 与 Rust 集成方使用)。
//! 模块均为 crate 根下,内部 `crate::` 引用在此解析。

pub mod daemon;
pub mod error;
pub mod ffi;
pub mod grpc;
pub mod inbox;
pub mod netcred;
pub mod probe;
pub mod protocol;
pub mod recv;
pub mod send;
pub mod tls;
pub mod ui;
pub mod update;
