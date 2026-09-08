fn main() {
    let server_lib = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../crates/owo-agent-server/src/lib.rs"
    );
    println!("cargo:rerun-if-changed={server_lib}");
    let source = std::fs::read_to_string(server_lib).expect("读取 OwO 核心 API 版本源失败");
    let marker = "pub const OWO_API_VERSION: &str = \"";
    let version = source
        .split(marker)
        .nth(1)
        .and_then(|value| value.split('"').next())
        .filter(|value| !value.is_empty())
        .expect("无法从核心服务源码读取 OWO_API_VERSION");
    println!("cargo:rustc-env=OWO_CORE_API_VERSION={version}");
    tauri_build::build()
}
