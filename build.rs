fn get_git_hash() -> String {
    use std::process::Command;

    let branch = Command::new("git").arg("rev-parse").arg("--abbrev-ref").arg("HEAD").output();
    if let Ok(branch_output) = branch {
        let branch_string = String::from_utf8_lossy(&branch_output.stdout);
        let commit = Command::new("git").arg("rev-parse").arg("--verify").arg("HEAD").output();
        if let Ok(commit_output) = commit {
            let commit_string = String::from_utf8_lossy(&commit_output.stdout);

            format!(
                "branch {}, hash {}",
                branch_string.lines().next().unwrap_or("?"),
                commit_string.lines().next().unwrap_or("?")
            )
        } else {
            panic!("Can not get git commit: {}", commit.unwrap_err());
        }
    } else {
        panic!("Can not get git branch: {}", branch.unwrap_err());
    }
}

fn main() {
    // Don't rebuild miri when nothing changed.
    println!("cargo:rerun-if-changed=build.rs");
    // Re-export the TARGET environment variable so it can be accessed by miri. Needed to know the
    // "host" triple inside Miri.
    let target = std::env::var("TARGET").unwrap();
    println!("cargo:rustc-env=TARGET={target}");
    // Allow some cfgs.
    println!("cargo::rustc-check-cfg=cfg(bootstrap)");
    println!("cargo:rustc-env=GIT_HASH={}", get_git_hash());
}
