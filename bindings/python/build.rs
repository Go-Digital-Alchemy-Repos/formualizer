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
//!     to Python as `None`). `unknown` is unambiguous: a real commit is 40
//!     lowercase hex digits.
//!   * Never report anything a cached build script cannot keep true. Cargo
//!     re-runs this script only when a declared input changes, so the working
//!     tree's dirtiness is not measured here at all - the builder passes it in
//!     through `FORMUALIZER_BUILD_DIRTY`, and an absent answer is `unknown`
//!     (surfaced as `None`) rather than an optimistic `false`.
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

    // Dirtiness is deliberately NOT measured here. A build script's output is
    // cached: cargo re-runs this script only when something it declared below
    // changes, and no `rerun-if-changed` can cover "any file in the working
    // tree". A `dirty=false` measured on one build would therefore survive
    // into a later build of an edited tree - a fabricated provenance claim on
    // the one field whose whole purpose is honesty.
    //
    // Instead the builder tells us, and `rerun-if-env-changed` below makes
    // cargo re-run the script whenever that answer changes.
    // `research/scripts/build_wheel.py` sets it (and refuses a dirty tree
    // outright). Anything else - a bare `cargo build`, an IDE - leaves it
    // unset, and the wheel honestly reports `dirty: None`.
    // Gated on the commit: a dirty flag describes a specific checkout, so
    // "clean" is meaningless next to "I could not identify this tree". Without
    // the gate a vendored copy with no .git, built by a caller that exports the
    // variable, would report {commit: None, dirty: False}.
    let dirty = if commit == "unknown" {
        "unknown".to_string()
    } else {
        match std::env::var("FORMUALIZER_BUILD_DIRTY").as_deref() {
            Ok("true") => "true".to_string(),
            Ok("false") => "false".to_string(),
            _ => "unknown".to_string(),
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
/// `git rev-parse --absolute-git-dir` resolves worktrees and submodules
/// correctly (in a linked worktree it points at `.git/worktrees/<name>`, which
/// is where that worktree's own `HEAD` lives). When it cannot be resolved
/// there is nothing to watch, and only the environment knobs remain.
///
/// Paths are declared unconditionally, including ones that do not exist yet:
/// cargo treats a declared-but-missing path as changed when it later appears,
/// which is exactly what is wanted for a branch ref that is currently packed.
fn emit_rerun_directives(manifest_dir: &Path) {
    // Whoever measured dirtiness tells us; changing that answer must re-run us.
    println!("cargo:rerun-if-env-changed=FORMUALIZER_BUILD_DIRTY");
    // Explicit escape hatch for a tree git cannot describe.
    println!("cargo:rerun-if-env-changed=FORMUALIZER_BUILD_STAMP_REFRESH");

    let Some(git_dir) = git_output(manifest_dir, &["rev-parse", "--absolute-git-dir"]) else {
        println!("cargo:warning=formualizer-python: no git dir found; build stamp is 'unknown'");
        return;
    };
    let git_dir = PathBuf::from(git_dir);

    // HEAD changes on every checkout/commit. Listed unconditionally: cargo
    // treats a declared path that does not exist as changed, so watching a
    // path *before* it appears is correct and watching it only when it already
    // exists is the bug (a commit that creates a previously packed loose ref
    // would otherwise not re-run this script, and the wheel would stamp the
    // parent commit).
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());

    // The ref HEAD points at changes on every commit while staying on a branch.
    // In a worktree the ref itself lives in the *common* dir, not the
    // per-worktree git dir, so resolve it through git rather than guessing.
    if let Some(symbolic) = git_output(manifest_dir, &["symbolic-ref", "--quiet", "HEAD"]) {
        let common = git_output(
            manifest_dir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .map(PathBuf::from)
        .unwrap_or_else(|| git_dir.clone());
        println!("cargo:rerun-if-changed={}", common.join(&symbolic).display());
        // Packed refs cover the case where the branch ref has been packed away.
        println!("cargo:rerun-if-changed={}", common.join("packed-refs").display());
    }
}
