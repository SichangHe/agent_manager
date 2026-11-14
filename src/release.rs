//! `amh release`: install a tested build as a read-only versioned directory, switch to it by symlink, restart the watcher; or roll back.
// The rework design's stage 0: "`amh release` replaces `cargo install` + `amh manager watchers`" ... "one-command rollback"; the main manager: refuse anything not on `revive` or older than the live build.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How many releases stay on disk besides the live one.
const KEEP: usize = 5;
/// The branch every release must be on.
const BRANCH: &str = "origin/revive";

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn releases_dir() -> PathBuf {
    home().join(".local/lib/amh")
}

/// Every switch, oldest first, one release directory name per line; rollback pops the last line.
fn history_path() -> PathBuf {
    releases_dir().join("history")
}

/// The command path the watcher unit and callers run.
fn installed() -> PathBuf {
    home().join(".cargo/bin/amh")
}

/// Run a command in `dir`: `Ok(Some(stdout))` on success, `Ok(None)` when it exits 1 (a git "no"), else an error.
fn run_status(dir: &Path, program: &str, args: &[&str]) -> Result<Option<String>, String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|error| format!("amh: cannot run {program}: {error}"))?;
    match out.status.code() {
        Some(0) => Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())),
        Some(1) if program == "git" => Ok(None),
        _ => Err(format!(
            "amh: `{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    run_status(dir, program, args)?
        .ok_or_else(|| format!("amh: `{program} {}` said no", args.join(" ")))
}

fn history() -> Vec<String> {
    fs::read_to_string(history_path())
        .unwrap_or_default()
        .lines()
        .filter(|name| releases_dir().join(name).join("amh").is_file())
        .map(str::to_owned)
        .collect()
}

fn write_history(names: &[String]) -> Result<(), String> {
    let temp = releases_dir().join(".history.new");
    fs::write(
        &temp,
        names
            .iter()
            .map(|name| format!("{name}\n"))
            .collect::<String>(),
    )
    .and_then(|()| fs::rename(&temp, history_path()))
    .map_err(|error| format!("amh: cannot record the release history: {error}"))
}

/// The live release's directory name, when the installed command is a release symlink.
fn live() -> Option<String> {
    let target = fs::read_link(installed()).ok()?;
    let dir = target.parent()?;
    (dir.parent()? == releases_dir()).then(|| dir.file_name()?.to_str().map(str::to_owned))?
}

fn commit_of(release: &str) -> Option<&str> {
    release.rsplit_once('-').map(|(_, commit)| commit)
}

fn locked() -> Result<fs::File, String> {
    fs::create_dir_all(releases_dir()).map_err(|error| format!("amh: {error}"))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(releases_dir().join("release.lock"))
        .map_err(|error| format!("amh: {error}"))?;
    lock.try_lock()
        .map_err(|_| "amh: another release or rollback is running".to_owned())?;
    Ok(lock)
}

/// Point the installed command at release `name` atomically, then restart the watcher.
fn switch(name: &str) -> Result<String, String> {
    let link = installed();
    let temp = link.with_extension("new");
    let _ = fs::remove_file(&temp);
    let target = releases_dir().join(name).join("amh");
    std::os::unix::fs::symlink(&target, &temp)
        .and_then(|()| fs::rename(&temp, &link))
        .map_err(|error| {
            format!(
                "amh: cannot point {} at {}: {error}",
                link.display(),
                target.display()
            )
        })?;
    run(
        &home(),
        "systemctl",
        &["--user", "restart", "amh-watch.service"],
    )?;
    Ok(format!("live: {}\n", target.display()))
}

/// Build the clean commit checked out in `repo`, test it, install it read-only and switch to it.
///
/// Refused when the tree has changes, when the commit is not on `origin/revive` (unless `hotfix`), or when the live release is not an ancestor of it.
pub fn release(repo: &Path, hotfix: bool) -> Result<String, String> {
    let _lock = locked()?;
    let mut said = String::new();
    if installed().exists() && live().is_none() {
        said += "amh: note: the installed command was no release (e.g. from `cargo install`); the newer-build check had nothing to compare with\n";
    }
    if !run(
        repo,
        "git",
        &["status", "--porcelain", "--untracked-files=no"],
    )?
    .is_empty()
    {
        return Err("amh: the checkout has changes; commit them first".to_owned());
    }
    run(repo, "git", &["fetch", "--quiet", "origin"])?;
    let commit = run(repo, "git", &["rev-parse", "--short=12", "HEAD"])?;
    let on_branch = run_status(
        repo,
        "git",
        &["merge-base", "--is-ancestor", "HEAD", BRANCH],
    )?
    .is_some();
    if !on_branch && !hotfix {
        return Err(format!(
            "amh: {commit} is not on {BRANCH}; push it there first, or pass --hotfix"
        ));
    }
    if let Some(live) = live() {
        let live_commit = commit_of(&live).unwrap_or_default();
        if run_status(
            repo,
            "git",
            &["merge-base", "--is-ancestor", live_commit, "HEAD"],
        )?
        .is_none()
        {
            return Err(format!(
                "amh: the live build {live_commit} is not part of {commit}; rebase onto it first, never overwrite a newer build"
            ));
        }
    }
    run(repo, "cargo", &["test", "--locked", "--quiet"])?;
    run(
        repo,
        "cargo",
        &["build", "--locked", "--release", "--quiet"],
    )?;
    let version = run(repo, "cargo", &["pkgid"])?
        .rsplit(['#', '@'])
        .next()
        .unwrap_or("0")
        .to_owned();
    let name = format!("{version}-{commit}");
    let target = releases_dir().join(&name);
    // Staging left by a crashed release is removed first.
    for entry in fs::read_dir(releases_dir()).into_iter().flatten().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".amh-staging-")
        {
            let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o755));
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    if !target.join("amh").is_file() {
        let staging = releases_dir().join(format!(".amh-staging-{}", std::process::id()));
        let built = run(
            repo,
            "cargo",
            &["metadata", "--format-version=1", "--no-deps"],
        )
        .ok()
        .and_then(|meta| serde_json::from_str::<serde_json::Value>(&meta).ok())
        .and_then(|meta| meta["target_directory"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| repo.join("target"))
        .join("release/amh");
        let copied = fs::create_dir_all(&staging)
            .and_then(|()| fs::copy(&built, staging.join("amh")).map(|_| ()))
            .and_then(|()| {
                fs::set_permissions(staging.join("amh"), fs::Permissions::from_mode(0o555))
            })
            .and_then(|()| fs::set_permissions(&staging, fs::Permissions::from_mode(0o555)))
            .and_then(|()| fs::rename(&staging, &target));
        copied.map_err(|error| format!("amh: cannot install {}: {error}", target.display()))?;
    }
    let mut names = history();
    names.push(name.clone());
    write_history(&names)?;
    prune(&names);
    Ok(said + &switch(&name)?)
}

/// Switch back to the release before the live one; repeated, it goes further back.
pub fn rollback() -> Result<String, String> {
    let _lock = locked()?;
    let mut names = history();
    let live = live().ok_or("amh: the installed command is no release; nothing to roll back to")?;
    if names.last() != Some(&live) {
        return Err(
            "amh: the live release is not the last one recorded; nothing is changed".to_owned(),
        );
    }
    names.pop();
    let previous = names
        .last()
        .cloned()
        .ok_or("amh: no earlier release is kept")?;
    write_history(&names)?;
    switch(&previous)
}

/// Keep the releases named in the last `KEEP` history lines, and the live one.
fn prune(names: &[String]) {
    let kept: Vec<&String> = names.iter().rev().take(KEEP).collect();
    let live = live();
    for entry in fs::read_dir(releases_dir()).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let release = entry.path().join("amh").is_file() && !name.starts_with('.');
        if release && !kept.contains(&&name) && live.as_deref() != Some(name.as_str()) {
            let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o755));
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}
