//! One amh request: who calls (request-scoped environment) and where its output goes, per thread, so the daemon can run many requests at once; outside the daemon both fall back to the process.
// The rework design: "CLI: ... sends a request id, argv, cwd and an allowlist of request-scoped env (`--task-file`, caller session); global settings (state dir, endpoints) are the daemon's own and never taken from a request"

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Write as _;

/// The environment variables a request may carry; everything else is the daemon's own.
pub const ALLOWED: [&str; 5] = [
    "OMNIGENT_RUNNER_PRIMARY_SESSION_ID",
    "OMO_AGENT_TASK_FILE",
    "AMH_HUMAN_STARTED",
    "TMUX_PANE",
    "USER",
];

thread_local! {
    static ENV: RefCell<Option<BTreeMap<String, String>>> = const { RefCell::new(None) };
    static OUT: RefCell<Option<String>> = const { RefCell::new(None) };
    static ERR: RefCell<Option<String>> = const { RefCell::new(None) };
    static ARGS: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    static CWD: RefCell<Option<std::path::PathBuf>> = const { RefCell::new(None) };
}

/// Whether this thread serves a daemon request.
pub fn serving() -> bool {
    ENV.with_borrow(Option::is_some)
}

/// The command line of the request, or of this process.
pub fn args() -> Vec<String> {
    ARGS.with_borrow(Clone::clone)
        .unwrap_or_else(|| std::env::args().collect())
}

/// The caller's working directory.
pub fn cwd() -> std::path::PathBuf {
    CWD.with_borrow(Clone::clone)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// A line for standard error: captured for the request, else printed.
pub fn err(text: &str) {
    let captured = ERR.with_borrow_mut(|out| match out {
        Some(out) => {
            out.push_str(text);
            out.push('\n');
            true
        }
        None => false,
    });
    if !captured {
        let _ = writeln!(std::io::stderr(), "{text}");
    }
}

/// A request-scoped variable: from the request being served, else the process environment.
pub fn var(name: &str) -> Option<String> {
    ENV.with_borrow(|env| match env {
        Some(env) => env.get(name).cloned(),
        None => std::env::var(name).ok(),
    })
}

/// Print to the request's output, or to standard output; a reader that went away does not fail the action.
pub fn say(text: &str) {
    let captured = OUT.with_borrow_mut(|out| match out {
        Some(out) => {
            out.push_str(text);
            true
        }
        None => false,
    });
    if !captured {
        let _ = std::io::stdout().write_all(text.as_bytes());
    }
}

/// Run `work` as the request with `env` (only `ALLOWED` names kept), `args` and `cwd`, returning what it printed to standard output and to standard error.
pub fn serve<T>(
    env: &BTreeMap<String, String>,
    args: &[String],
    cwd: &std::path::Path,
    work: impl FnOnce() -> T,
) -> (T, String, String) {
    let kept = env
        .iter()
        .filter(|(name, _)| ALLOWED.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    ENV.set(Some(kept));
    OUT.set(Some(String::new()));
    ERR.set(Some(String::new()));
    ARGS.set(Some(args.to_vec()));
    CWD.set(Some(cwd.to_owned()));
    let result = work();
    ENV.set(None);
    ARGS.set(None);
    CWD.set(None);
    let out = OUT.take().unwrap_or_default();
    let err = ERR.take().unwrap_or_default();
    (result, out, err)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_served_request_sees_only_its_allowed_env_and_captures_output() {
        let env = [
            ("OMO_AGENT_TASK_FILE".to_owned(), "a.md".to_owned()),
            ("HOME".to_owned(), "/elsewhere".to_owned()),
        ]
        .into_iter()
        .collect();
        let (seen, out, _) = super::serve(&env, &[], std::path::Path::new("/"), || {
            super::say("hello\n");
            (super::var("OMO_AGENT_TASK_FILE"), super::var("HOME"))
        });
        assert_eq!(seen, (Some("a.md".to_owned()), None));
        assert_eq!(out, "hello\n");
        assert_eq!(
            super::var("OMO_AGENT_TASK_FILE"),
            std::env::var("OMO_AGENT_TASK_FILE").ok()
        );
    }
}
