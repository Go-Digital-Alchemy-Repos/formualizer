//! Stamp the wheel with the fork commit it was built from (GOD-230).
//!
//! A qualification round hands a wheel to a driver and later has to prove
//! which source tree produced it. `cargo:rustc-env` bakes that answer into the
//! binary, so `formualizer.__build__` can be read back from an installed wheel
//! with no side channel.
//!
//! Honesty rules, in order of importance:
//!
//!   * Never fabricate. If `git` is missing, fails, or this source tree is not
//!     a checkout, the commit is emitted as the sentinel `unknown` (surfaced
//!     to Python as `None`) and dirty as `unknown` (surfaced as `None`).
//!     `unknown` is unambiguous: a real commit is 40 lowercase hex digits and a
//!     real dirty flag is `true`/`false`.
//!   * Never embed a timestamp or any other value that changes between two
//!     builds of the same source. Reproducibility is a measured gate.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo"),
    );

    let commit = git_output(&manifest_dir, &["rev-parse", "HEAD"])
        .filter(|s| is_full_sha(s))
        .unwrap_or_else(|| "unknown".to_string());

    // `git status --porcelain` is empty exactly when the working tree and index
    // are clean. It is only meaningful if we actually reached git, so it is
    // gated on the commit having been resolved: otherwise a `None` from a
    // missing git would masquerade as "clean".
    let dirty = if commit == "unknown" {
        "unknown".to_string()
    } else {
        match git_output(&manifest_dir, &["status", "--porcelain"]) {
            Some(status) => (!status.trim().is_empty()).to_string(),
            None => "unknown".to_string(),
        }
    };

    println!("cargo:rustc-env=FORMUALIZER_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=FORMUALIZER_BUILD_DIRTY={dirty}");

    emit_rerun_directives(&manifest_dir);
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        // Keep the answer about *this* tree even if the caller's environment
        // points git somewhere else (a common footgun inside build sandboxes).
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
}

/// Ask cargo to re-run this script when the checked-out commit changes.
///
/// `git rev-parse --git-dir` resolves worktrees and submodules correctly (in a
/// linked worktree it points at `.git/worktrees/<name>`, which is where that
/// worktree's own `HEAD` lives). When it cannot be resolved there is nothing
/// to watch, so we fall back to `rerun-if-env-changed` on an explicit override
/// knob — cargo will then re-run the script only when that variable changes or
/// the crate's own sources do.
fn emit_rerun_directives(manifest_dir: &Path) {
    let Some(git_dir) = git_output(manifest_dir, &["rev-parse", "--absolute-git-dir"]) else {
        println!("cargo:rerun-if-env-changed=FORMUALIZER_BUILD_STAMP_REFRESH");
        println!("cargo:warning=formualizer-python: no git dir found; build stamp is 'unknown'");
        return;
    };
    let git_dir = PathBuf::from(git_dir);

    // HEAD changes on every checkout/commit.
    let head = git_dir.join("HEAD");
    if head.exists() {
        println!("cargo:rerun-if-changed={}", head.display());
    }

    // The ref HEAD points at changes on every commit while staying on a branch.
    // In a worktree the ref itself lives in the *common* dir, not the per-worktree
    // git dir, so resolve it through git rather than guessing a path.
    if let Some(symbolic) = git_output(manifest_dir, &["symbolic-ref", "--quiet", "HEAD"]) {
        let common = git_output(manifest_dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .map(PathBuf::from)
            .unwrap_or_else(|| git_dir.clone());
        let loose_ref = common.join(&symbolic);
        if loose_ref.exists() {
            println!("cargo:rerun-if-changed={}", loose_ref.display());
        }
        // Packed refs cover the case where the branch ref has been packed away.
        let packed = common.join("packed-refs");
        if packed.exists() {
            println!("cargo:rerun-if-changed={}", packed.display());
        }
    }
}
