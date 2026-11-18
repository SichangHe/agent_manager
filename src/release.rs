//! `amh release`: install a tested build as a read-only versioned directory, switch to it by symlink, restart the watcher; or roll back.
// The rework design's stage 0: "`amh release` replaces `cargo install` + `amh manager watchers`" ... "one-command rollback"; the main manager: refuse anything not on `revive` or older than the live build.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How many releases stay on disk besides the live one.
const KEEP: usize = 5;
/// The branch every release must be on.
const BRANCH: &str = "origin/main";

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
    // After the stage-2 cut-over the daemon runs the watcher; before it, amh-watch does.
    let unit = if crate::settings::state_dir()
        .join(crate::daemon::CLIENT_MARK)
        .exists()
    {
        "amhd.service"
    } else {
        "amh-watch.service"
    };
    run(&home(), "systemctl", &["--user", "restart", unit])?;
    if unit == "amhd.service" {
        // A daemon that does not open its socket within 30 s leaves clients waiting: say so, with the way back.
        let socket = crate::daemon::socket_path(&crate::settings::state_dir());
        let up = (0..300).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::os::unix::net::UnixStream::connect(&socket).is_ok()
        });
        if !up {
            return Err(format!(
                "amh: amhd did not come up on {} after the switch; run `amh release --rollback` (clients wait for it meanwhile)",
                target.display()
            ));
        }
    }
    // The quota observer runs the same binary; restart it only if it runs.
    run(
        &home(),
        "systemctl",
        &["--user", "try-restart", "amh-quota.service"],
    )?;
    Ok(format!("live: {}\n", target.display()))
}

/// Build the clean commit checked out in `repo`, test it, install it read-only and switch to it.
///
/// Refused when the tree has changes, when the commit is not on `origin/main` (unless `hotfix`), or when the live release is not an ancestor of it.
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
    let mut said = String::new();
    // Rolling back from the daemon to a build without it (stage-2 rollback, docs/rework-and-upgrades.md): clients first, then the units.
    let state_dir = crate::settings::state_dir();
    let mark = state_dir.join(crate::daemon::CLIENT_MARK);
    let daemon_live = mark.exists()
        || Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", "amhd.service"])
            .status()
            .is_ok_and(|s| s.success());
    if daemon_live && !has_daemon(&releases_dir().join(&previous).join("amh"))? {
        said += &leave_daemon(&state_dir, &mark)?;
    }
    // An older build reads whole task rows: put the older bodies kept in `task_old_body` back first, with the live service stopped so it cannot split them again; `switch` starts it.
    said += &flush_task_bodies()?;
    write_history(&names)?;
    Ok(said + &switch(&previous)?)
}

/// Stop the live service and put every task's older body back into its row.
fn flush_task_bodies() -> Result<String, String> {
    let path = crate::settings::state_dir().join("amh.sqlite3");
    if !path.exists() {
        return Ok(String::new());
    }
    for unit in ["amhd.service", "amh-watch.service"] {
        let _ = Command::new("systemctl")
            .args(["--user", "stop", unit])
            .status();
    }
    let mut database = crate::database::Database::open(&path)?;
    database.expand_export_records()?;
    let moved = database.flush_old_bodies()?;
    // Queued task-list moves and resolved requests go back where an older build reads them.
    database.fold_todo(true)?;
    database.fold_sent_rows()?;
    // Stopped marks an older build sets are not remembered: index them again when this build returns.
    database.delete_record("stopped-index:v1")?;
    let restored = database.restore_resolved()?;
    Ok(format!(
        "amh: {moved} long task bodies joined back and {restored} resolved requests put back for the older build\n"
    ))
}

/// Whether the build at `binary` has the daemon: `amh daemon` without `AMH_DAEMON=1` says it is off; older builds reject the command. Anything else is an error, never a guess.
fn has_daemon(binary: &Path) -> Result<bool, String> {
    let mut child = Command::new(binary)
        .arg("daemon")
        .env_remove("AMH_DAEMON")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("amh: cannot run {}: {e}", binary.display()))?;
    let began = std::time::Instant::now();
    while child.try_wait().map_err(|e| format!("amh: {e}"))?.is_none() {
        if began.elapsed() > std::time::Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "amh: `{} daemon` did not exit; cannot tell whether it has the daemon",
                binary.display()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
    }
    if stderr.contains("the daemon is off") {
        Ok(true)
    } else if stderr.contains("unrecognized subcommand") {
        Ok(false)
    } else {
        Err(format!(
            "amh: cannot tell whether {} has the daemon: {}",
            binary.display(),
            stderr.trim()
        ))
    }
}

/// Switch from amhd back to amh-watch: new commands run in-process, the outbox drains (60 s at most; what is left is listed to resend by hand), clients still retrying finish, then amhd stops and amh-watch takes over, also at boot.
fn leave_daemon(state_dir: &Path, mark: &Path) -> Result<String, String> {
    // Each step may already be done by an earlier, interrupted rollback.
    if mark.exists() {
        fs::remove_file(mark).map_err(|e| format!("amh: cannot remove {}: {e}", mark.display()))?;
    }
    let mut said = String::from("amh: commands run in-process again\n");
    // A client retries a request for about 30 s; let those reach the daemon and queue first, then drain.
    std::thread::sleep(std::time::Duration::from_secs(30));
    let open = || -> Vec<(String, String, String)> {
        rusqlite::Connection::open(state_dir.join("amh.sqlite3"))
            .and_then(|db| {
                let mut query = db.prepare("SELECT id, recipient, substr(body, 1, 80) FROM outbox WHERE state IN ('pending', 'sending')")?;
                query.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect()
            })
            .unwrap_or_default()
    };
    for _ in 0..60 {
        if open().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    run(&home(), "systemctl", &["--user", "stop", "amhd.service"])?;
    // Listed after the stop, so nothing can still be queued behind the list.
    for (id, to, body) in open() {
        said += &format!("amh: not delivered by amhd; resend by hand: {id} to {to}: {body}\n");
    }
    run(&home(), "systemctl", &["--user", "disable", "amhd.service"])?;
    run(
        &home(),
        "systemctl",
        &["--user", "enable", "amh-watch.service"],
    )?;
    said += "amh: amhd stopped; amh-watch takes over (also at boot)\n";
    Ok(said)
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
