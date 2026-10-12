fn main() {
    let workspace = std::path::Path::new("../../Cargo.toml");
    println!("cargo:rerun-if-changed={}", workspace.display());
    println!("cargo:rerun-if-changed=proto/nexus/secrets/v1/secrets.proto");
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
    prost_build::compile_protos(&["proto/nexus/secrets/v1/secrets.proto"], &["proto"])
        .expect("compile the upstream GenericSecretsService protocol");
}
