use std::process::Command;

fn main() {
    // Link to libproc for proc_pidinfo (child CWD lookup).
    // cfg!(target_os) can't be used here: build scripts are compiled for the
    // host, so it would also fire when cross-compiling to Linux.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=proc");
    }

    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let count = git(&["rev-list", "--count", "HEAD"]).unwrap_or_else(|| "0".into());
    let hash = git(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let version = format!("{branch}-{count}-{hash}");
    println!("cargo:rustc-env=LRMUX_VERSION={version}");

    // Re-run when git head moves.
    // HEAD itself only changes on branch checkout; the branch ref changes on commit.
    println!("cargo:rerun-if-changed=.git/HEAD");
    if branch != "unknown" {
        println!("cargo:rerun-if-changed=.git/refs/heads/{branch}");
    }
    println!("cargo:rerun-if-changed=.git/packed-refs");
    println!("cargo:rerun-if-changed=build.rs");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(std::env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout)
        .ok()
        .map(|s| s.trim().to_string())
}
