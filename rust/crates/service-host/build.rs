fn main() {
    let workspace = std::path::Path::new("../../Cargo.toml");
    println!("cargo:rerun-if-changed={}", workspace.display());
    println!("cargo:rerun-if-changed=proto/nexus/secrets/v1/secrets.proto");
    println!("cargo:rerun-if-changed=runtime-artifacts.json");
    let cargo: toml::Value = std::fs::read_to_string(workspace).unwrap().parse().unwrap();
    let tag = cargo["workspace"]["dependencies"]["nexus-cluster"]["tag"]
        .as_str()
        .unwrap();
    println!(
        "cargo:rustc-env=NEXUS_RUNTIME_VERSION={}",
        tag.strip_prefix('v')
            .expect("versioned Nexus workspace dependency")
    );
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path().unwrap());
    let artifacts: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string("runtime-artifacts.json")
            .expect("runtime artifact metadata")
            .trim_start_matches('\u{feff}'),
    )
    .expect("valid runtime artifact metadata");
    let os = std::env::var("CARGO_CFG_TARGET_OS").expect("target OS");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").expect("target architecture");
    let extension = if os == "windows" { "zip" } else { "tar.gz" };
    let artifact = format!("{tag}/nexusd-cluster-{os}-{arch}.{extension}");
    let checksum = artifacts["sha256"][&artifact].as_str().unwrap_or_default();
    assert!(
        checksum.len() == 64
            && checksum.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "Missing checksum for the pinned runtime {artifact}; update runtime-artifacts.json from that release"
    );
    prost_build::compile_protos(&["proto/nexus/secrets/v1/secrets.proto"], &["proto"])
        .expect("compile the upstream GenericSecretsService protocol");
}
