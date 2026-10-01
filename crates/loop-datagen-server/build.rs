fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("PROTOC").is_none() {
        let protoc_path = protoc_bin_vendored::protoc_bin_path()?;
        std::env::set_var("PROTOC", protoc_path);
    }
    tonic_build::compile_protos("proto/loop_agent.proto")?;
    Ok(())
}
