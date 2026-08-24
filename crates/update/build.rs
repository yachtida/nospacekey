fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    #[cfg(windows)]
    {
        let hash = std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|hash| hash.trim().to_string())
            .filter(|hash| !hash.is_empty())
            .unwrap_or_else(|| "unknown".into());
        let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
        let mut resource = winresource::WindowsResource::new();
        resource.set("ProductVersion", &format!("{version}+{hash}"));
        resource.set("FileDescription", "NospacekeyUpdateChecker.exe");
        resource
            .compile()
            .expect("checker VERSIONINFO requires rc.exe");
    }
}
