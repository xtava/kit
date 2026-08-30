use std::{env, fs, path::Path, process::Command};

mod source_identity;

use source_identity::SOURCE_IDENTITY_PATHS;

const UNKNOWN: &str = "unknown";
const RECURSIVE_WATCH_PATHS: &[&str] = &["src", "tests"];

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-changed=source_identity.rs");
    println!("cargo:rerun-if-changed=vendor/wezterm.upstream");
    for path in RECURSIVE_WATCH_PATHS {
        println!("cargo:rerun-if-changed={path}");
    }

    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")
        .map(std::path::PathBuf::from)
        .expect("Cargo always sets CARGO_MANIFEST_DIR for build scripts");
    let revision = git_output(&manifest_dir, &["rev-parse", "--verify", "HEAD"])
        .filter(|value| is_revision(value))
        .unwrap_or_else(|| UNKNOWN.to_owned());
    let mut status_args = vec!["status", "--porcelain=v1", "--untracked-files=normal", "--"];
    status_args.extend_from_slice(SOURCE_IDENTITY_PATHS);
    let dirty = git_output(&manifest_dir, &status_args)
        .map(|status| if status.is_empty() { "false" } else { "true" })
        .unwrap_or(UNKNOWN);
    let wezterm_provenance = manifest_dir.join("vendor/wezterm.upstream");
    let wezterm_revision = pinned_wezterm_value(&wezterm_provenance, "revision")
        .unwrap_or_else(|| panic!("vendor/wezterm.upstream must contain one 40-hex revision"));
    let retained_wezterm_tree = pinned_wezterm_value(&wezterm_provenance, "retained_tree")
        .unwrap_or_else(|| panic!("vendor/wezterm.upstream must contain one 40-hex retained_tree"));
    let committed_wezterm_tree =
        git_output(&manifest_dir, &["rev-parse", "--verify", "HEAD:vendor/wezterm"])
            .filter(|value| is_revision(value))
            .unwrap_or_else(|| UNKNOWN.to_owned());
    if dirty == "false" && committed_wezterm_tree != retained_wezterm_tree {
        panic!(
            "checked-in vendor/wezterm tree {committed_wezterm_tree} does not match retained_tree \
             {retained_wezterm_tree} in vendor/wezterm.upstream"
        );
    }

    for path in git_lines(&manifest_dir, &["ls-files", "--"], SOURCE_IDENTITY_PATHS) {
        println!("cargo:rerun-if-changed={path}");
    }

    println!("cargo:rustc-env=KIT_SOURCE_REVISION={revision}");
    println!("cargo:rustc-env=KIT_SOURCE_DIRTY={dirty}");
    println!("cargo:rustc-env=KIT_WEZTERM_REVISION={wezterm_revision}");
    println!("cargo:rustc-env=KIT_WEZTERM_RETAINED_TREE={retained_wezterm_tree}");
}

fn git_lines(repository: &Path, args: &[&str], paths: &[&str]) -> Vec<String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repository).args(args).args(paths);
    let Ok(output) = command.output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect()
}

fn git_output(repository: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").arg("-C").arg(repository).args(args).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn pinned_wezterm_value(path: &Path, key: &str) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    contents.lines().find_map(|line| {
        let value = line.strip_prefix(&format!("{key} = \""))?.strip_suffix('"')?;
        is_revision(value).then(|| value.to_owned())
    })
}

fn is_revision(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
