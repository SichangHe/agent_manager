//! `amh release`: install a tested build as a read-only versioned directory, switch to it by symlink, restart the watcher; or roll back.
// The rework design's stage 0: "`amh release` replaces `cargo install` + `amh manager watchers`" ... "one-command rollback" ... refuses anything not on `revive` or older than the live build.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How many releases stay on disk.
const KEEP: usize = 5;
/// The branch every release must be on.
const BRANCH: &str = "origin/revive";

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn releases_dir() -> PathBuf {
    home().join(".local/lib/amh")
}

/// The command path the watcher unit and callers run.
fn installed() -> PathBuf {
    home().join(".cargo/bin/amh")
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|error| format!("amh: cannot run {program}: {error}"))?;
    match out.status.success() {
        true => Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned()),
        false => Err(format!(
            "amh: `{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// The release directories, oldest first; a name is `VERSION-COMMIT`.
fn releases() -> Vec<PathBuf> {
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(releases_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().join("amh").is_file())
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    found.sort();
    found.into_iter().map(|(_, path)| path).collect()
}

/// The release the installed command points at, if it is one.
fn live() -> Option<PathBuf> {
    fs::read_link(installed())
        .ok()
        .and_then(|target| target.parent().map(Path::to_owned))
}

fn commit_of(release: &Path) -> Option<&str> {
    release.file_name()?.to_str()?.rsplit_once('-').map(|(_, commit)| commit)
}

/// Point the installed command at `release` atomically, then restart the watcher.
fn switch(release: &Path) -> Result<String, String> {
    let link = installed();
    let temp = link.with_extension("new");
    let _ = fs::remove_file(&temp);
    std::os::unix::fs::symlink(release.join("amh"), &temp)
        .and_then(|()| fs::rename(&temp, &link))
        .map_err(|error| format!("amh: cannot point {} at {}: {error}", link.display(), release.display()))?;
    run(&home(), "systemctl", &["--user", "restart", "amh-watch.service"])?;
    Ok(format!("live: {}\n", release.display()))
}

/// Build the clean commit checked out in `repo`, test it, install it read-only and switch to it.
///
/// Refused when the tree has changes, when the commit is not on `origin/revive` (unless `hotfix`), or when the live release is not an ancestor of it.
pub fn release(repo: &Path, hotfix: bool) -> Result<String, String> {
    fs::create_dir_all(releases_dir()).map_err(|error| format!("amh: {error}"))?;
    let lock = fs::File::create(releases_dir().join("release.lock")).map_err(|error| format!("amh: {error}"))?;
    lock.try_lock()
        .map_err(|_| "amh: another release is running".to_owned())?;
    if !run(repo, "git", &["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        return Err("amh: the checkout has changes; commit them first".to_owned());
    }
    run(repo, "git", &["fetch", "--quiet", "origin"])?;
    let commit = run(repo, "git", &["rev-parse", "--short", "HEAD"])?;
    let on_branch = run(repo, "git", &["merge-base", "--is-ancestor", "HEAD", BRANCH]).is_ok();
    if !on_branch && !hotfix {
        return Err(format!("amh: {commit} is not on {BRANCH}; push it there first, or pass --hotfix"));
    }
    if let Some(live_commit) = live().as_deref().and_then(commit_of) {
        if run(repo, "git", &["merge-base", "--is-ancestor", live_commit, "HEAD"]).is_err() {
            return Err(format!(
                "amh: the live build {live_commit} is not part of {commit}; rebase onto it first, never overwrite a newer build"
            ));
        }
    }
    run(repo, "cargo", &["test", "--quiet"])?;
    run(repo, "cargo", &["build", "--release", "--quiet"])?;
    let version = run(repo, "cargo", &["pkgid"])?
        .rsplit(['#', '@'])
        .next()
        .unwrap_or("0")
        .to_owned();
    let target = releases_dir().join(format!("{version}-{commit}"));
    if !target.join("amh").is_file() {
        let staging = releases_dir().join(format!(".{version}-{commit}.{}", std::process::id()));
        let built = PathBuf::from(run(repo, "cargo", &["metadata", "--format-version=1", "--no-deps"])
            .ok()
            .and_then(|meta| serde_json::from_str::<serde_json::Value>(&meta).ok())
            .and_then(|meta| meta["target_directory"].as_str().map(str::to_owned))
            .unwrap_or_else(|| repo.join("target").display().to_string()))
        .join("release/amh");
        let copied = fs::create_dir_all(&staging)
            .and_then(|()| fs::copy(&built, staging.join("amh")).map(|_| ()))
            .and_then(|()| fs::set_permissions(staging.join("amh"), fs::Permissions::from_mode(0o555)))
            .and_then(|()| fs::set_permissions(&staging, fs::Permissions::from_mode(0o555)))
            .and_then(|()| fs::rename(&staging, &target));
        copied.map_err(|error| format!("amh: cannot install {}: {error}", target.display()))?;
    }
    let said = switch(&target)?;
    prune();
    Ok(said)
}

/// Switch back to the release before the live one.
pub fn rollback() -> Result<String, String> {
    let all = releases();
    let live = live().ok_or("amh: the installed command is no release; nothing to roll back to")?;
    let at = all
        .iter()
        .position(|release| *release == live)
        .ok_or("amh: the live release is not among the kept ones")?;
    let previous = at
        .checked_sub(1)
        .map(|before| all[before].clone())
        .ok_or("amh: no earlier release is kept")?;
    // Touching the target makes it the newest, so the rolled-back release counts as older from now on.
    let now = fs::File::open(&previous).and_then(|dir| dir.set_modified(std::time::SystemTime::now()));
    now.map_err(|error| format!("amh: {error}"))?;
    switch(&previous)
}

/// Keep the newest `KEEP` releases and the live one.
fn prune() {
    let all = releases();
    let live = live();
    for old in all.iter().take(all.len().saturating_sub(KEEP)) {
        if Some(old) != live.as_ref() {
            let _ = fs::set_permissions(old, fs::Permissions::from_mode(0o755));
            let _ = fs::remove_dir_all(old);
        }
    }
}
