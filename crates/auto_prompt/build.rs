#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    if let Some(output) = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
    {
        let hash = String::from_utf8_lossy(&output.stdout);
        let hash = hash.trim();
        println!("cargo:rustc-env=AUTO_PROMPT_COMMIT_SHA={hash}");
    }
}
