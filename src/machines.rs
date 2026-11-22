//! Several machines, each with its own amh, Omnigent, and agents, sharing the work-log git repository.
//!
//! A task belongs to one machine (its `machine` field, else `main`); only that machine's database holds it and only it writes its file.
//! A message to another machine's agent is a new file under `machine_mail/OWNER/`, never edited, carried by the git sync; the owner delivers it once.
// 🧑 "The state is replicated. But. Tolerates large disconnections. And. Agents can talk to each other just like in the same machine."
// 🧑 "We could probably make Git not conflict with a bit of effort ... we do want separate responsibilities"

use crate::settings;
use crate::store::Store;
use crate::tasks::Task;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

type Settings = BTreeMap<String, String>;

/// The machine that owns tasks without a `machine` field: the one amh ran on alone before.
pub const MAIN: &str = "main";
pub const MAIL_DIR: &str = "machine_mail";
/// How often the git sync runs once machines are named, so a message to another machine takes about this long.
pub const SYNC_EVERY: Duration = Duration::from_secs(60);

/// This machine's name, or `None` when amh runs on one machine (`machine` unset in `env.toml`).
pub fn me(settings: &Settings) -> Option<String> {
    Some(settings::get(settings, "machine").trim().to_owned()).filter(|name| !name.is_empty())
}

/// The machine owning `task`.
pub fn owner(task: &Task) -> &str {
    task.field("machine").filter(|name| !name.is_empty()).unwrap_or(MAIN)
}

/// The task list this machine exports: `TODO.md` on `main`, `TODO.NAME.md` elsewhere, so no two machines write one file.
pub fn todo_file(me: Option<&str>) -> String {
    match me {
        None | Some(MAIN) => "TODO.md".to_owned(),
        Some(name) => format!("TODO.{name}.md"),
    }
}

/// Whether this machine reads the email mailboxes: only `main` does.
pub fn reads_mail(settings: &Settings) -> bool {
    me(settings).is_none_or(|me| me == MAIN)
}

/// Whether this machine takes in the human's message with `subject`: the owner of its `[tag]` task does; one without a known task goes to `main`.
pub fn takes(store: &Store, subject: &str) -> Result<bool, String> {
    let Some(me) = &store.machine else {
        return Ok(true);
    };
    let name = crate::mail::tag_of(subject).map(|tag| format!("{}.md", crate::zulip::task_topic(tag)));
    let owner = match &name {
        Some(name) if store.contains(name)? => return Ok(true),
        Some(name) => remote_tasks(store)?
            .into_iter()
            .find(|task| &task.name == name)
            .map(|task| owner(&task).to_owned()),
        None => None,
    };
    Ok(owner.as_deref().unwrap_or(MAIN) == me)
}

/// Other machines' open tasks, read from their files in the root; this machine's database never holds them.
pub fn remote_tasks(store: &Store) -> Result<Vec<Task>, String> {
    let Some(me) = &store.machine else {
        return Ok(Vec::new());
    };
    let entries = fs::read_dir(&store.root)
        .map_err(|error| format!("cannot read {}: {error}", store.root.display()))?;
    let mut tasks = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(task) = name
            .ends_with(".md")
            .then(|| fs::read_to_string(entry.path()).ok())
            .flatten()
            .and_then(|text| Task::parse(&name, &text).ok())
            .filter(|task| owner(task) != me)
        {
            tasks.push(task);
        }
    }
    Ok(tasks)
}

/// The other machine's task whose agent runs at `address`, if any.
///
/// An address of one of this machine's open tasks is answered from the database, so most sends read no task file.
/// A failed lookup answers `None`: the send then goes to the local Omnigent, which reports an unknown session itself.
pub fn remote_at(settings: &Settings, address: &str) -> Option<Task> {
    me(settings)?;
    let store = Store::new(settings);
    let local = store.database().ok().flatten()?.active_names_at(address).ok()?;
    if !local.is_empty() {
        return None;
    }
    remote_tasks(&store).ok()?.into_iter().rev().find(|task| task.address() == address)
}

/// Leave `text` for the agent of `to`, another machine's task: a new file named by time, sender, and process, so no two writers share it.
pub fn post(settings: &Settings, to: &Task, text: &str) -> Result<(), String> {
    let me = me(settings).unwrap_or_else(|| MAIN.to_owned());
    let folder = settings::work_logs(settings).join(MAIL_DIR).join(owner(to));
    fs::create_dir_all(&folder).map_err(|error| format!("cannot create {}: {error}", folder.display()))?;
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = folder.join(format!("{now_ns}-{me}-{}.md", std::process::id()));
    crate::taskfile::write(&path, &render(&to.name, text))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

fn render(to: &str, text: &str) -> String {
    format!("---\nto: {to}\n---\n{text}")
}

/// The task name and text of a posted file; the header is exactly the one `render` writes, so its first `---` line ends it.
fn parse(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\nto: ")?;
    rest.split_once("\n---\n")
}

/// Deliver this machine's posts not handled yet, oldest first; returns what to log.
///
/// A handled post gets an acknowledgment `acked/FILE` beside it, written only by this machine and carried by git like the post,
/// so a new clone or a rebuilt state folder never sends it again. Posts and acknowledgments are never edited or deleted:
/// a deletion could meet a machine offline for weeks.
/// A post for a task that moved to another machine is passed on to it; one waiting for an agent, or whose send failed, is tried next round.
/// A crash between the send and the acknowledgment repeats the message once: delivery is at least once.
pub fn deliver(settings: &Settings, store: &Store) -> Vec<String> {
    let Some(me) = &store.machine else {
        return Vec::new();
    };
    let folder = store.root.join(MAIL_DIR).join(me);
    let acked = folder.join("acked");
    let mut names: Vec<String> = match fs::read_dir(&folder) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.ends_with(".md") && !acked.join(name).exists())
            .collect(),
        Err(_) => return Vec::new(),
    };
    names.sort();
    let mut log = Vec::new();
    for name in names {
        let outcome = match deliver_one(settings, store, &folder.join(&name)) {
            Ok(outcome) => outcome,
            Err(why) => {
                log.push(format!("machine mail {name}: {why}; retrying"));
                continue;
            }
        };
        log.push(format!("machine mail {name}: {outcome}"));
        let written = fs::create_dir_all(&acked).and_then(|()| fs::write(acked.join(&name), format!("{outcome}\n")));
        if let Err(error) = written {
            log.push(format!("machine mail {name}: cannot acknowledge: {error}"));
        }
    }
    log
}

/// Send one post; `Ok` says what became of it, `Err` why it waits.
fn deliver_one(settings: &Settings, store: &Store, path: &Path) -> Result<String, String> {
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let Some((to, text)) = parse(&text) else {
        return Ok("dropped: not a machine mail file".to_owned());
    };
    if !store.contains(to)? {
        return match remote_tasks(store)?.into_iter().find(|task| task.name == to) {
            Some(task) => {
                post(settings, &task, text)?;
                Ok(format!("passed on to machine {}", owner(&task)))
            }
            None => Err(format!("no task {to} here or on another machine yet")),
        };
    }
    let task = store.load(to)?;
    if !task.address().starts_with(crate::omnigent::PREFIX) {
        return Err(format!("{to} has no agent"));
    }
    crate::omnigent::send(settings, task.address(), text)?;
    Ok(format!("sent to {to}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_file_per_machine() {
        assert_eq!(todo_file(None), "TODO.md");
        assert_eq!(todo_file(Some(MAIN)), "TODO.md");
        assert_eq!(todo_file(Some("mac")), "TODO.mac.md");
    }

    #[test]
    fn post_round_trips() {
        let text = "from omnigent://a\n---\nline with --- inside\n";
        assert_eq!(parse(&render("t.md", text)), Some(("t.md", text)));
        assert_eq!(parse("no header"), None);
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn machine(root: &Path, name: &str) -> (Settings, Store) {
        let settings = Settings::from([
            ("machine".to_owned(), name.to_owned()),
            ("work_logs".to_owned(), root.display().to_string()),
        ]);
        let store = Store { root: root.to_owned(), state_dir: root.join(".state"), machine: me(&settings) };
        (settings, store)
    }

    /// Two machines sharing one origin: each holds only its own tasks, writes its own task list, and git merges without conflicts.
    #[test]
    fn two_machines_share_a_repository_without_conflicts() -> Result<(), String> {
        let top = std::env::temp_dir().join(format!("amh-machines-{}", std::process::id()));
        let _ = fs::remove_dir_all(&top);
        fs::create_dir_all(&top).unwrap();
        git(&top, &["init", "-q", "--bare", "-b", "main", "origin.git"]);
        let (a, b) = (top.join("a"), top.join("b"));
        git(&top, &["clone", "-q", "origin.git", "a"]);
        fs::write(a.join(".gitignore"), ".state/\n").unwrap();
        fs::write(a.join("srv.md"), "---\nstatus: running\nrunat: omnigent://srv\n---\n").unwrap();
        fs::write(a.join("TODO.md"), "current:\nsrv.md omnigent://srv\n\nprevious:\n").unwrap();
        git(&a, &["-c", "user.name=T", "-c", "user.email=t@x.invalid", "add", "."]);
        git(&a, &["-c", "user.name=T", "-c", "user.email=t@x.invalid", "commit", "-qm", "init"]);
        git(&a, &["push", "-q", "origin", "main"]);
        git(&top, &["clone", "-q", "origin.git", "b"]);
        for dir in [&a, &b] {
            git(dir, &["config", "user.name", "T"]);
            git(dir, &["config", "user.email", "t@x.invalid"]);
        }
        let (main_settings, main) = machine(&a, MAIN);
        let (_, mac) = machine(&b, "mac");
        drop(main.lock()?);
        drop(mac.lock()?);
        assert!(!mac.contains("srv.md")?, "the Mac's database holds no task of main");
        assert!(mac.load("srv.md").unwrap_err().contains("belongs to machine main"));
        {
            let _lock = mac.lock()?;
            mac.save(&Task::parse("wechat.md", "---\nstatus: running\nrunat: omnigent://wx\n---\n")?)?;
            mac.write("TODO.md", "current:\nwechat.md omnigent://wx\n\nprevious:\n")?;
        }
        mac.sync_files()?;
        assert!(mac.load("wechat.md")?.field("machine") == Some("mac"), "a task made on the Mac names it");
        assert!(b.join("TODO.mac.md").is_file(), "the Mac exports its own task list");
        {
            let _lock = main.lock()?;
            main.write("TODO.md", "current:\nsrv.md omnigent://srv\nnew.md retired\n\nprevious:\n")?;
        }
        main.sync_files()?;
        crate::history::commit_and_push(&b, || Ok(()))?;
        crate::history::commit_and_push(&a, || Ok(()))?;
        assert_eq!(remote_tasks(&main)?.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["wechat.md"]);
        assert_eq!(remote_tasks(&mac)?.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["srv.md"]);
        assert!(!takes(&main, "[wechat] Zulip message")?, "main leaves the Mac's topic alone");
        assert!(takes(&main, "[srv] Zulip message")? && takes(&main, "no tag")?);
        let address = remote_at(&main_settings, "omnigent://wx").map(|task| task.name);
        assert_eq!(address.as_deref(), Some("wechat.md"));
        post(&main_settings, &remote_tasks(&main)?[0], "from main\n")?;
        crate::history::commit_and_push(&a, || Ok(()))?;
        crate::history::commit_and_push(&b, || Ok(()))?;
        assert!(takes(&mac, "[wechat] Zulip message")? && !takes(&mac, "[srv] Zulip message")? && !takes(&mac, "no tag")?);
        let posted: Vec<_> = fs::read_dir(b.join(MAIL_DIR).join("mac")).unwrap().flatten().collect();
        let text = fs::read_to_string(posted[0].path()).unwrap();
        assert_eq!(parse(&text), Some(("wechat.md", "from main\n")));
        assert!(fs::read_to_string(b.join("TODO.md")).unwrap().contains("new.md"), "main's task list reached the Mac unmerged");
        let _ = fs::remove_dir_all(&top);
        Ok(())
    }

    #[test]
    fn owner_defaults_to_main() {
        let task = Task::parse("t.md", "---\nstatus: running\nrunat: retired\n---\n").unwrap();
        assert_eq!(owner(&task), MAIN);
        let task = Task::parse("t.md", "---\nstatus: running\nrunat: retired\nmachine: mac\n---\n").unwrap();
        assert_eq!(owner(&task), "mac");
    }
}
