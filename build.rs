// gRPC 代码生成:protox(纯 Rust protoc 替代)编译 → tonic-build 生成代码。
// 不依赖系统 protoc。
// 注:build-dep 有两份 prost(0.13 由 tonic-build 链引入、0.14 由 protox 链引入),
// protox 返回的类型实现的是 prost 0.14 的 Message —— 显式用 0.14 编码。

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let descriptor = protox::compile(["proto/fts.proto"], ["proto/"])?;
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    let descriptor_path = out_dir.join("fts_descriptor.bin");
    let bytes: Vec<u8> = protox::prost::Message::encode_to_vec(&descriptor);
    std::fs::write(&descriptor_path, bytes)?;

    let protos: [std::path::PathBuf; 1] = [descriptor_path.clone()];
    let includes: [std::path::PathBuf; 0] = [];
    tonic_build::configure()
        .skip_protoc_run()
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(&protos, &includes)?;
    Ok(())
}

