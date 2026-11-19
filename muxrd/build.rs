// build.rs — compile proto/muxr.proto using tonic-prost-build.
//
// The generated code lands in $OUT_DIR/muxr.v1.rs and is included
// via the `include_proto!` macro in src/lib.rs.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    // Also emit a FileDescriptorSet; tests/wire_surface.rs pins the wire
    // surface from it.
    tonic_prost_build::configure()
        .file_descriptor_set_path(out_dir.join("muxr_descriptor.bin"))
        .compile_protos(&["proto/muxr.proto"], &["proto"])?;
    // Re-run this build script whenever the .proto changes.
    println!("cargo:rerun-if-changed=proto/muxr.proto");
    Ok(())
}
