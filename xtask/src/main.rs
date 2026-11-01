use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, ExitCode};

const NAME: &str = "imge";
const TARGETS: [&str; 2] = ["aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"];

fn cargo(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("cargo");
    command.current_dir(root).args(args);
    command
}

fn version(root: &Path) -> Result<String, String> {
    let output = cargo(root, &["pkgid", NAME])
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("cargo pkgid failed: {}", output.status));
    }
    let pkgid = String::from_utf8_lossy(&output.stdout);
    match pkgid.trim().rsplit(['#', '@']).next() {
        Some(version) if !version.is_empty() => Ok(version.to_string()),
        _ => Err(format!("cannot parse version from {pkgid:?}")),
    }
}

fn build(root: &Path, version: &str, target: &str) -> Result<(), String> {
    let status = cargo(
        root,
        &[
            "zigbuild",
            "--release",
            "--package",
            NAME,
            "--target",
            target,
        ],
    )
    .status()
    .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("cargo zigbuild failed for {target}: {status}"));
    }

    // cargo hard-links the binary to its own cache, so moving it would leave the output sharing
    // that file with the next build; a fresh copy keeps them apart.
    let out = root
        .join("target")
        .join(format!("{NAME}-{version}-{target}"));
    match fs::remove_file(&out) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.to_string()),
        _ => {}
    }
    let binary = root.join("target").join(target).join("release").join(NAME);
    fs::copy(binary, out).map_err(|e| e.to_string())?;
    Ok(())
}

fn dist() -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("xtask has no parent dir")?;
    let version = version(root)?;
    TARGETS
        .iter()
        .try_for_each(|target| build(root, &version, target))
}

fn main() -> ExitCode {
    let result = match env::args().nth(1).as_deref() {
        Some("dist") => dist(),
        _ => Err("usage: cargo dist".to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
