// build.rs — compile every .proto under proto/ using tonic-prost-build.
//
// `proto/` is the include root (imports read `muxr/v1/<file>.proto`). The
// generated code lands in $OUT_DIR/muxr.v1.rs and is included via the
// `include_proto!` macro in src/lib.rs.

use std::path::{Path, PathBuf};

/// Every `.proto` file below `dir`, recursively, skipping names that start
/// with `.`.
fn collect_protos(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_protos(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "proto") {
            out.push(path);
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let mut files = Vec::new();
    let include = PathBuf::from("proto");
    collect_protos(&include, &mut files)?;
    files.sort();
    // Re-run this build script whenever anything under proto/ changes.
    println!("cargo:rerun-if-changed=proto");
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
    }
    // Also emit a FileDescriptorSet; tests/wire_surface.rs pins the wire
    // surface from it.
    tonic_prost_build::configure()
        .file_descriptor_set_path(out_dir.join("muxr_descriptor.bin"))
        .compile_protos(&files, &[include])?;
    Ok(())
}
