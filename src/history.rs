//! Record work-log Markdown history in git and push it, without touching anyone's index, worktree, or hooks.
//!
//! Commits are built with plumbing in a private index (`GIT_INDEX_FILE`). Origin is merged in object space.
//! Only afterwards are the worktree and the shared index brought to the new commit, path by path, keeping local edits.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Files above this size are never committed.
// 🧑 "amh refuses to commit any file over 5 MB and tells the manager"
pub const MAX_BYTES: u64 = 5 * 1024 * 1024;
const REMOTE: &str = "origin";
const PUSH_TRIES: usize = 3;
const PATHSPEC: [&str; 2] = ["*.md", ":(exclude)amh_views"];

/// What one run did.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    pub committed: bool,
    pub pushed: bool,
    /// Changed files left uncommitted for exceeding `MAX_BYTES`, with their sizes.
    pub oversize: Vec<(String, u64)>,
}

struct Repo {
    root: PathBuf,
    index: Option<PathBuf>,
}

impl Repo {
    fn run(&self, args: &[&str], input: Option<&[u8]>) -> Result<Output, String> {
        let mut command = Command::new("timeout");
        command.args(["60s", "git", "-C"]).arg(&self.root).args(args);
        if let Some(index) = &self.index {
            command.env("GIT_INDEX_FILE", index);
        }
        let mut child = command
            .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("amh: git: {e}"))?;
        if let Some(input) = input {
            let mut stdin = child.stdin.take().ok_or("amh: git stdin unavailable")?;
            stdin.write_all(input).map_err(|e| format!("amh: git: {e}"))?;
        }
        child.wait_with_output().map_err(|e| format!("amh: git: {e}"))
    }

    /// Run git and return trimmed-of-nothing stdout, failing on a non-zero exit.
    fn out(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
        let output = self.run(args, input)?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        Err(format!(
            "amh: git {} failed ({}): {}",
            args.first().unwrap_or(&""),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }

    fn text(&self, args: &[&str]) -> Result<String, String> {
        Ok(String::from_utf8_lossy(&self.out(args, None)?).trim().to_owned())
    }

    fn with_index(&self, index: PathBuf) -> Repo {
        Repo { root: self.root.clone(), index: Some(index) }
    }

    fn succeeds(&self, args: &[&str]) -> Result<bool, String> {
        Ok(self.run(args, None)?.status.success())
    }
}

fn nul_list(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

/// A `mode sha stage\tpath` line of `ls-files -s`/`-u` or `ls-tree`-style output.
fn index_entries(bytes: &[u8]) -> Vec<(String, String, u8, String)> {
    nul_list(bytes)
        .into_iter()
        .filter_map(|entry| {
            let (meta, path) = entry.split_once('\t')?;
            let mut parts = meta.split(' ');
            Some((
                parts.next()?.to_owned(),
                parts.next()?.to_owned(),
                parts.next()?.parse().ok()?,
                path.to_owned(),
            ))
        })
        .collect()
}

/// Commit changed Markdown to the current branch, merge and push origin, and move the local branch forward.
///
/// Files that are tracked, then removed from the shared index by someone, are left alone.
pub fn commit_and_push(root: &Path) -> Result<Outcome, String> {
    let shared = Repo { root: root.to_owned(), index: None };
    let branch = shared
        .text(&["symbolic-ref", "--short", "-q", "HEAD"])
        .map_err(|_| "amh: HEAD is not on a branch (rebase in progress?); retrying later".to_owned())?;
    let head = shared.text(&["rev-parse", "HEAD"])?;
    let git_dir = PathBuf::from(shared.text(&["rev-parse", "--absolute-git-dir"])?);
    let private = shared.with_index(git_dir.join("amh.index"));
    let mut outcome = Outcome::default();
    let ours = match own_commit(&shared, &private, &head, &mut outcome)? {
        Some(commit) => {
            sync_local(&shared, &head, &commit, false)?;
            move_branch(&shared, &branch, &commit, &head)?;
            commit
        }
        None => head,
    };
    if shared.text(&["remote", "get-url", REMOTE]).is_err() {
        return Ok(outcome);
    }
    publish(&shared, &branch, ours, &mut outcome)?;
    Ok(outcome)
}

/// Build a commit of the changed, non-ignored, small Markdown files on top of `head` in the private index.
fn own_commit(
    shared: &Repo,
    private: &Repo,
    head: &str,
    outcome: &mut Outcome,
) -> Result<Option<String>, String> {
    let head_tree = shared.text(&["rev-parse", &format!("{head}^{{tree}}")])?;
    let reusable = private.index.as_ref().is_some_and(|p| p.exists())
        && private.text(&["write-tree"]).is_ok_and(|tree| tree == head_tree);
    if !reusable {
        private.out(&["read-tree", &head_tree], None)?;
    }
    let spec = |first: &'static [&'static str]| -> Vec<&'static str> {
        first.iter().chain(["--"].iter()).chain(PATHSPEC.iter()).copied().collect()
    };
    let mut paths = nul_list(&private.out(&spec(&["diff-files", "-z", "--name-only"]), None)?);
    paths.extend(nul_list(
        &private.out(&spec(&["ls-files", "-z", "--others", "--exclude-standard"]), None)?,
    ));
    let unstaged_removals = nul_list(&shared.out(
        &["diff", "--cached", "-z", "--name-only", "--diff-filter=D", head, "--"],
        None,
    )?);
    paths.retain(|path| !unstaged_removals.contains(path));
    paths.sort();
    paths.dedup();
    let listed = paths.iter().fold(Vec::new(), |mut all, path| {
        all.extend_from_slice(path.as_bytes());
        all.push(0);
        all
    });
    let ignored = nul_list(
        &private
            .run(&["check-ignore", "--no-index", "-z", "--stdin"], Some(&listed))?
            .stdout,
    );
    paths.retain(|path| !ignored.contains(path));
    let mut keep = Vec::new();
    for path in paths {
        match fs::symlink_metadata(shared.root.join(&path)) {
            Ok(meta) if !meta.is_file() => {}
            Ok(meta) if meta.len() > MAX_BYTES => outcome.oversize.push((path, meta.len())),
            _ => keep.push(path),
        }
    }
    let listed = keep.iter().fold(Vec::new(), |mut all, path| {
        all.extend_from_slice(path.as_bytes());
        all.push(0);
        all
    });
    private.out(&["update-index", "--add", "--remove", "-z", "--stdin"], Some(&listed))?;
    let tree = private.text(&["write-tree"])?;
    if tree == head_tree {
        return Ok(None);
    }
    let commit = shared.text(&["commit-tree", &tree, "-p", head, "-m", "docs: record work logs"])?;
    outcome.committed = true;
    Ok(Some(commit))
}

/// Fetch, merge origin when it moved, push, and move the local branch to the pushed commit.
fn publish(
    shared: &Repo,
    branch: &str,
    ours: String,
    outcome: &mut Outcome,
) -> Result<(), String> {
    let tracking = format!("refs/remotes/{REMOTE}/{branch}");
    for _ in 0..PUSH_TRIES {
        let fetched = shared.run(&["fetch", "-q", "--no-tags", REMOTE, &format!("+refs/heads/{branch}:{tracking}")], None)?;
        // A branch absent on origin is created by the push below.
        let theirs = if fetched.status.success() { shared.text(&["rev-parse", &tracking]).ok() } else { None };
        if !fetched.status.success() && shared.text(&["ls-remote", "--heads", REMOTE, branch]).is_ok_and(|l| !l.is_empty()) {
            return Err(format!("amh: git fetch failed: {}", String::from_utf8_lossy(&fetched.stderr).trim()));
        }
        let mut target = ours.clone();
        if let Some(theirs) = theirs {
            if shared.succeeds(&["merge-base", "--is-ancestor", &theirs, &ours])? {
                if theirs == ours {
                    return Ok(());
                }
            } else if shared.succeeds(&["merge-base", "--is-ancestor", &ours, &theirs])? {
                sync_local(shared, &ours, &theirs, true)?;
                move_branch(shared, branch, &theirs, &ours)?;
                return Ok(());
            } else {
                target = merge(shared, &ours, &theirs)?;
            }
        }
        let pushed = shared.run(&["push", "-q", "--no-verify", REMOTE, &format!("{target}:refs/heads/{branch}")], None)?;
        if !pushed.status.success() {
            continue;
        }
        outcome.pushed = true;
        if target != ours {
            sync_local(shared, &ours, &target, true)?;
        }
        move_branch(shared, branch, &target, &ours)?;
        shared.out(&["update-ref", &tracking, &target], None)?;
        return Ok(());
    }
    Err("amh: git push was rejected repeatedly".to_owned())
}

/// Move the branch only if nobody else moved it meanwhile; their move wins and the next run resumes from it.
fn move_branch(shared: &Repo, branch: &str, to: &str, from: &str) -> Result<(), String> {
    shared.out(&["update-ref", &format!("refs/heads/{branch}"), to, from], None).map(|_| ())
}

/// A merge commit of `ours` and `theirs` made in a temporary index; clashing files keep both sides' lines.
///
/// The files are append-mostly logs, so a line-wise union loses the least.
fn merge(shared: &Repo, ours: &str, theirs: &str) -> Result<String, String> {
    let base = shared.text(&["merge-base", ours, theirs]).ok();
    let scratch = tempdir(shared)?;
    let index = scratch.join("merge.index");
    let temp = shared.with_index(index);
    let mut args = vec!["read-tree", "-m", "--aggressive"];
    let empty;
    match &base {
        Some(base) => args.push(base),
        None => {
            empty = shared.text(&["hash-object", "-t", "tree", "/dev/null"])?;
            args.push(&empty);
        }
    }
    args.extend([ours, theirs]);
    temp.out(&args, None)?;
    let mut stages: BTreeMap<String, [Option<(String, String)>; 3]> = BTreeMap::new();
    for (mode, sha, stage, path) in index_entries(&temp.out(&["ls-files", "-u", "-z"], None)?) {
        stages.entry(path).or_default()[usize::from(stage) - 1] = Some((mode, sha));
    }
    for (path, [base, mine, other]) in stages {
        let chosen = match (mine, other) {
            (Some(mine), Some(other)) => {
                let blob = |name: &str, entry: &Option<(String, String)>| -> Result<PathBuf, String> {
                    let file = scratch.join(name);
                    let bytes = match entry {
                        Some((_, sha)) => shared.out(&["cat-file", "blob", sha], None)?,
                        None => Vec::new(),
                    };
                    fs::write(&file, bytes).map_err(|e| format!("amh: merge: {e}"))?;
                    Ok(file)
                };
                let (b, m, o) = (blob("b", &base)?, blob("m", &Some(mine.clone()))?, blob("o", &Some(other))?);
                let merged = Command::new("git")
                    .args(["merge-file", "-p", "--union"])
                    .args([&m, &b, &o])
                    .output()
                    .map_err(|e| format!("amh: git merge-file: {e}"))?;
                if merged.status.code().is_none_or(|code| code > 127) {
                    return Err(format!("amh: git merge-file failed for {path}"));
                }
                let sha = String::from_utf8_lossy(&shared.out(&["hash-object", "-w", "--stdin"], Some(&merged.stdout))?)
                    .trim()
                    .to_owned();
                Some((mine.0, sha))
            }
            // Deleted on one side, edited on the other: keep the edit.
            (one, other) => one.or(other),
        };
        if let Some((mode, sha)) = chosen {
            temp.out(&["update-index", "--add", "--cacheinfo", &format!("{mode},{sha},{path}")], None)?;
        }
    }
    let tree = temp.text(&["write-tree"])?;
    let merged = shared.text(&["commit-tree", &tree, "-p", ours, "-p", theirs, "-m", "docs: merge origin work logs"]);
    let _ = fs::remove_dir_all(&scratch);
    merged
}

fn tempdir(shared: &Repo) -> Result<PathBuf, String> {
    let git_dir = PathBuf::from(shared.text(&["rev-parse", "--absolute-git-dir"])?);
    let dir = git_dir.join(format!("amh-scratch-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| format!("amh: merge: {e}"))?;
    Ok(dir)
}

/// Bring the shared index, and with `worktree` the files, from commit `from` to commit `to`, per changed path.
///
/// A path is only moved when the worktree or index still holds what `from` had, so uncommitted edits stay as edits against `to`.
/// A worktree file that has local edits gets `from`->`to` merged into it, keeping both.
fn sync_local(shared: &Repo, from: &str, to: &str, worktree: bool) -> Result<(), String> {
    let diff = shared.out(&["diff-tree", "-r", "-z", "--no-renames", "--raw", from, to], None)?;
    let fields = nul_list(&diff);
    let mut changes = Vec::new();
    for pair in fields.chunks(2) {
        let [meta, path] = pair else { continue };
        let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        if let [old_mode, new_mode, old_sha, new_sha, _] = parts[..] {
            changes.push((path.clone(), (old_mode.to_owned(), old_sha.to_owned()), (new_mode.to_owned(), new_sha.to_owned())));
        }
    }
    let zero = "0".repeat(40);
    let mut info = Vec::new();
    let current: BTreeMap<String, String> = index_entries(&shared.out(&["ls-files", "-s", "-z", "--"], None)?)
        .into_iter()
        .filter(|(_, _, stage, _)| *stage == 0)
        .map(|(_, sha, _, path)| (path, sha))
        .collect();
    for (path, old, new) in &changes {
        let held = current.get(path).map_or(zero.as_str(), String::as_str);
        if held == old.1 {
            let mode = if new.0 == "000000" { "0" } else { new.0.as_str() };
            info.extend_from_slice(format!("{mode} {}\t{path}\0", new.1).as_bytes());
        }
        if worktree {
            sync_file(shared, path, &old.1, &new.1)?;
        }
    }
    if !info.is_empty() {
        shared.out(&["update-index", "-z", "--index-info"], Some(&info))?;
    }
    Ok(())
}

/// Apply `old`->`new` to one worktree file: replace it when untouched, else merge, else leave it.
fn sync_file(shared: &Repo, path: &str, old: &str, new: &str) -> Result<(), String> {
    let file = shared.root.join(path);
    let blob = |sha: &str| -> Result<Vec<u8>, String> {
        if sha.chars().all(|c| c == '0') { Ok(Vec::new()) } else { shared.out(&["cat-file", "blob", sha], None) }
    };
    let new_missing = new.chars().all(|c| c == '0');
    let have = fs::read(&file).ok();
    let (old_bytes, new_bytes) = (blob(old)?, blob(new)?);
    let result = match &have {
        None if old.chars().all(|c| c == '0') => Some(new_bytes),
        None => None,
        Some(have) if *have == new_bytes && !new_missing => None,
        Some(have) if *have == old_bytes => {
            if new_missing {
                let _ = fs::remove_file(&file);
                return Ok(());
            }
            Some(new_bytes)
        }
        Some(_) if new_missing => None,
        Some(_) => {
            let dir = tempdir(shared)?;
            let (a, b, c) = (dir.join("a"), dir.join("b"), dir.join("c"));
            fs::write(&b, &old_bytes).and_then(|_| fs::write(&c, &new_bytes)).map_err(|e| format!("amh: merge: {e}"))?;
            fs::copy(&file, &a).map_err(|e| format!("amh: merge: {e}"))?;
            let merged = Command::new("git").args(["merge-file", "--union"]).args([&a, &b, &c]).status().map_err(|e| format!("amh: git merge-file: {e}"))?;
            let bytes = fs::read(&a).map_err(|e| format!("amh: merge: {e}"));
            let _ = fs::remove_dir_all(&dir);
            if merged.code().is_none_or(|code| code > 127) {
                return Ok(());
            }
            Some(bytes?)
        }
    };
    if let Some(bytes) = result {
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("amh: {e}"))?;
        }
        let temp = file.with_file_name(format!(".{}.amh-tmp", file.file_name().unwrap_or_default().to_string_lossy()));
        fs::write(&temp, bytes).and_then(|_| fs::rename(&temp, &file)).map_err(|e| format!("amh: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn put(dir: &Path, name: &str, text: &str) {
        fs::write(dir.join(name), text).unwrap();
    }

    /// An origin plus two clones, `a` (the amh worktree) and `b` (another machine), on `main`.
    fn setup(tag: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let top = std::env::temp_dir().join(format!("amh-history-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&top);
        fs::create_dir_all(&top).unwrap();
        let origin = top.join("origin.git");
        sh(&top, &["init", "-q", "--bare", "-b", "main", "origin.git"]);
        let (a, b) = (top.join("a"), top.join("b"));
        sh(&top, &["clone", "-q", origin.to_str().unwrap(), "a"]);
        sh(&a, &["config", "user.name", "T"]);
        sh(&a, &["config", "user.email", "t@example.invalid"]);
        put(&a, ".gitignore", "amh_views/\nskip.md\n");
        put(&a, "t.md", "one\ntwo\nthree\n");
        put(&a, "code.txt", "x\n");
        sh(&a, &["add", "."]);
        sh(&a, &["commit", "-qm", "init"]);
        sh(&a, &["push", "-q", "origin", "main"]);
        sh(&top, &["clone", "-q", origin.to_str().unwrap(), "b"]);
        sh(&b, &["config", "user.name", "T"]);
        sh(&b, &["config", "user.email", "t@example.invalid"]);
        (top, origin, a, b)
    }

    #[test]
    fn commits_only_small_tracked_or_new_markdown_and_pushes() {
        let (top, origin, a, _b) = setup("commit");
        put(&a, "t.md", "one\ntwo\nthree\nfour\n");
        put(&a, "new.md", "new\n");
        put(&a, "skip.md", "ignored\n");
        fs::create_dir_all(a.join("amh_views")).unwrap();
        put(&a, "amh_views/events.md", "generated\n");
        put(&a, "code.txt", "edited\n");
        sh(&a, &["add", "code.txt"]);
        fs::write(a.join("huge.md"), vec![b'x'; (MAX_BYTES + 1) as usize]).unwrap();
        let out = commit_and_push(&a).unwrap();
        assert!(out.committed && out.pushed);
        assert_eq!(out.oversize, vec![("huge.md".to_owned(), MAX_BYTES + 1)]);
        let files = sh(&origin, &["ls-tree", "-r", "--name-only", "main"]);
        assert_eq!(files.lines().collect::<Vec<_>>(), [".gitignore", "code.txt", "new.md", "t.md"]);
        assert_eq!(sh(&origin, &["show", "main:code.txt"]), "x");
        assert_eq!(sh(&a, &["diff", "--cached", "--name-only"]), "code.txt", "shared index keeps others' staging");
        assert_eq!(sh(&a, &["rev-parse", "HEAD"]), sh(&origin, &["rev-parse", "main"]));
        assert_eq!(sh(&a, &["status", "--porcelain", "-uno"]), "M  code.txt");
        assert!(!commit_and_push(&a).unwrap().committed);
        fs::remove_dir_all(top).unwrap();
    }

    #[test]
    fn a_file_removed_from_the_index_stays_out() {
        let (top, origin, a, _b) = setup("rm");
        sh(&a, &["rm", "-q", "--cached", "t.md"]);
        put(&a, "t.md", "changed\n");
        commit_and_push(&a).unwrap();
        assert!(sh(&origin, &["ls-tree", "-r", "--name-only", "main"]).contains("t.md"), "history keeps the file");
        assert_eq!(sh(&a, &["status", "--porcelain", "-uno"]), "D  t.md");
        fs::remove_dir_all(top).unwrap();
    }

    #[test]
    fn merges_origin_without_touching_local_edits() {
        let (top, origin, a, b) = setup("merge");
        put(&b, "other.md", "from b\n");
        put(&b, "t.md", "one\ntwo\nthree\nfrom b\n");
        sh(&b, &["add", "."]);
        sh(&b, &["commit", "-qm", "b"]);
        sh(&b, &["push", "-q", "origin", "main"]);
        // a: its own committed append, plus an uncommitted code edit and an uncommitted edit of a file b changed.
        put(&a, "t.md", "one\ntwo\nthree\nfrom a\n");
        put(&a, "code.txt", "local edit\n");
        let out = commit_and_push(&a).unwrap();
        assert!(out.pushed);
        let merged = sh(&origin, &["show", "main:t.md"]);
        assert!(merged.contains("from a") && merged.contains("from b"), "{merged}");
        assert_eq!(fs::read_to_string(a.join("other.md")).unwrap(), "from b\n");
        assert_eq!(fs::read_to_string(a.join("t.md")).unwrap(), format!("{merged}\n"));
        assert_eq!(fs::read_to_string(a.join("code.txt")).unwrap(), "local edit\n");
        assert_eq!(sh(&a, &["status", "--porcelain"]), "M code.txt", "only the local edit shows as a change");
        assert_eq!(sh(&a, &["rev-parse", "HEAD"]), sh(&origin, &["rev-parse", "main"]));
        fs::remove_dir_all(top).unwrap();
    }

    #[test]
    fn fast_forwards_over_origin_and_keeps_uncommitted_edits_of_the_same_file() {
        let (top, origin, a, b) = setup("ff");
        put(&b, "t.md", "one\ntwo\nthree\nfrom b\n");
        sh(&b, &["commit", "-qam", "b"]);
        sh(&b, &["push", "-q", "origin", "main"]);
        // a's own file `new.md` is uncommitted-and-committed by amh; t.md is edited locally at its top.
        put(&a, "t.md", "zero\none\ntwo\nthree\n");
        let out = commit_and_push(&a).unwrap();
        assert!(out.pushed);
        assert_eq!(fs::read_to_string(a.join("t.md")).unwrap().lines().count(), 5);
        assert_eq!(sh(&a, &["rev-parse", "HEAD"]), sh(&origin, &["rev-parse", "main"]));
        assert_eq!(sh(&a, &["diff", "--stat"]), "", "worktree equals the commit, nothing looks reverted");
        fs::remove_dir_all(top).unwrap();
    }

    #[test]
    fn a_failing_push_is_an_error() {
        let (top, _origin, a, _b) = setup("fail");
        sh(&a, &["remote", "set-url", "origin", "/nonexistent/origin.git"]);
        put(&a, "t.md", "changed\n");
        let err = commit_and_push(&a).unwrap_err();
        assert!(err.contains("amh: git"), "{err}");
        assert!(sh(&a, &["log", "--oneline"]).contains("record work logs"), "commit is kept locally");
        fs::remove_dir_all(top).unwrap();
    }
}
