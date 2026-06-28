fn main() {
    // Re-run the build script if Git HEAD or refs change.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");

    // Prefer GIT_COMMIT from the environment (set as a Docker build-arg, since
    // the build context has no .git); otherwise resolve it from git locally.
    let commit = std::env::var("GIT_COMMIT").ok().unwrap_or_else(|| {
        std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "unknown".to_string())
    });

    println!("cargo:rustc-env=GIT_COMMIT={commit}");
}
