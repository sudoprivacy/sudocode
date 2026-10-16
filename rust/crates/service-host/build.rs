fn main() {
    let workspace = std::path::Path::new("../../Cargo.toml");
    println!("cargo:rerun-if-changed={}", workspace.display());
    println!("cargo:rerun-if-changed=runtime-artifacts.json");
    println!("cargo:rerun-if-changed=proto/nexus/secrets/v1/secrets.proto");
    let cargo: toml::Value = std::fs::read_to_string(workspace).unwrap().parse().unwrap();
    let tag = cargo["workspace"]["dependencies"]["nexus-cluster"]["tag"]
        .as_str()
        .unwrap();
    let artifacts: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string("runtime-artifacts.json").unwrap()).unwrap();
    assert_eq!(
        artifacts["versions"]["nexusd-cluster"].as_str(),
        tag.strip_prefix('v'),
        "runtime-artifacts.json must pin the workspace Nexus release and its archive checksums"
    );
    std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path().unwrap());
    prost_build::compile_protos(&["proto/nexus/secrets/v1/secrets.proto"], &["proto"])
        .expect("compile the upstream GenericSecretsService protocol");
}
