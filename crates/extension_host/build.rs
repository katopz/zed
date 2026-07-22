use std::env;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    clean_apple_double_files()?;
    copy_extension_api_rust_files()
}

/// Remove macOS AppleDouble (`._*`) metadata files from the extension_api WIT
/// directories before the `wasmtime::component::bindgen!` macro expands.
///
/// On non-APFS macOS filesystems (exFAT, FAT32, SMB/NFS mounts), every real
/// file gets a `._<filename>` companion holding its extended attributes. The
/// `bindgen!` macro globs `*.wit` from these directories and tries to parse the
/// AppleDouble file as WIT, failing with `stream did not contain valid UTF-8`
/// and silently aborting generation of the `zed` module. Without that module,
/// every `since_v*.rs` file cascades into E0117 / E0433 / E0432 errors that
/// look like orphan-rule or import bugs but are purely environmental.
///
/// AppleDouble files are never legitimate source, so unconditional removal
/// is safe.
fn clean_apple_double_files() -> Result<(), Box<dyn std::error::Error>> {
    let wit_dir = PathBuf::from("../extension_api/wit");
    if !wit_dir.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(&wit_dir)? {
        let subdir = entry?.path();
        if !subdir.is_dir() {
            continue;
        }
        for subentry in fs::read_dir(&subdir)? {
            let path = subentry?.path();
            let is_apple_double = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("._"));
            if is_apple_double {
                println!(
                    "cargo:warning=extension_host: removing AppleDouble metadata file: {}",
                    path.display()
                );
                fs::remove_file(&path)
                    .map_err(|e| format!("removing AppleDouble file {}: {e}", path.display()))?;
            }
        }
    }

    Ok(())
}

/// rust-analyzer doesn't support include! for files from outside the crate.
/// Copy them to the OUT_DIR, so we can include them from there, which is supported.
fn copy_extension_api_rust_files() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = env::var("OUT_DIR")?;
    let input_dir = PathBuf::from("../extension_api/wit");
    let output_dir = PathBuf::from(out_dir);

    println!("cargo:rerun-if-changed={}", input_dir.display());

    for entry in fs::read_dir(&input_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            println!("cargo:rerun-if-changed={}", path.display());

            for subentry in fs::read_dir(&path)? {
                let subentry = subentry?;
                let subpath = subentry.path();
                if subpath.extension() == Some(std::ffi::OsStr::new("rs")) {
                    let relative_path = subpath.strip_prefix(&input_dir)?;
                    let destination = output_dir.join(relative_path);

                    fs::create_dir_all(destination.parent().unwrap())?;
                    fs::copy(&subpath, &destination)?;
                }
            }
        } else if path.extension() == Some(std::ffi::OsStr::new("rs")) {
            let relative_path = path.strip_prefix(&input_dir)?;
            let destination = output_dir.join(relative_path);

            fs::create_dir_all(destination.parent().unwrap())?;
            fs::copy(&path, &destination)?;
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    Ok(())
}
