fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    let descriptor_path = out_dir.join("greeter_descriptor.bin");

    camber_build::configure()
        .file_descriptor_set_path(&descriptor_path)
        .compile_protos(&["tests/proto/greeter.proto"], &["tests/proto"])?;

    Ok(())
}
