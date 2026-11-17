//! `amh daemon` (amhd): owns the SQLite store; runs only with `AMH_DAEMON=1`.

pub mod commit;
pub mod edits;
pub mod migrate;
pub mod outbox;
#[cfg(test)]
mod tests;

use std::fs;
use std::io::Write as _;
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::store::Store;

/// The state-dir file whose presence sends every client command through the daemon (stage 2 cut-over); removing it is the first step of a rollback.
pub const CLIENT_MARK: &str = "amhd.client";

/// Fault injection for the crash tests: with `AMHD_CRASH=POINT:N`, the Nth pass through `POINT` aborts the daemon, as `kill -9` would there.
pub fn crash_point(point: &str) {
    static PLAN: std::sync::OnceLock<Option<(String, u64)>> = std::sync::OnceLock::new();
    static PASSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let plan = PLAN.get_or_init(|| {
        let raw = std::env::var("AMHD_CRASH").ok()?;
        let (name, n) = raw.split_once(':')?;
        Some((name.to_owned(), n.parse().ok()?))
    });
    if let Some((name, n)) = plan
        && name == point
        && PASSES.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == *n
    {
        eprintln!("amhd: crash point {point} reached; aborting");
        std::process::abort();
    }
}

/// Full mtime scan of task files, catching edits inotify missed.
const FULL_SCAN: Duration = Duration::from_secs(600);
/// Longest the exporter waits without a commit before re-exporting writes made outside the daemon.
const EXPORT_TICK: Duration = Duration::from_secs(1);

/// Serve until killed; returns an exit status only when the daemon cannot start.
pub fn run() -> u8 {
    if std::env::var("AMH_DAEMON").as_deref() != Ok("1") {
        eprintln!("amh: the daemon is off; set AMH_DAEMON=1 to run it");
        return 2;
    }
    // Stage 1 of the rework runs the daemon only on copies of the production state; the cutover is stage 2.
    let production = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".local/state/amh");
    if crate::settings::state_dir() == production
        && std::env::var("AMH_DAEMON_LIVE").as_deref() != Ok("1")
    {
        eprintln!(
            "amh: refusing to run the daemon on the production state dir; use a copy (AMH_STATE_DIR)"
        );
        return 2;
    }
    match serve() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

fn serve() -> Result<(), String> {
    let settings = crate::settings::load()?;
    let store = Store::new(&settings);
    let _single = single_instance(&store.state_dir)?;
    // The first start imports the markdown records, which only the locked path does; later starts skip it.
    if store.database()?.is_none() {
        drop(store.lock()?);
    }
    let path = store.state_dir.join("amh.sqlite3");
    let mut connection =
        rusqlite::Connection::open(&path).map_err(|e| format!("amh: SQLite: {e}"))?;
    migrate::migrate(&mut connection)?;
    // The state files (holds, notices, quota pauses, start queue) stay files: the code still reads them there, and the first live cut-over showed that moving them aside drops every hold. Importing them waits until their readers use `state_files`.
    drop(connection);
    let (committed, commits) = mpsc::channel();
    let (queue, _commit_thread) = commit::Queue::start(&path, committed)?;
    commit::install(queue.clone());
    let database = edits::initialized(&store)?;
    edits::resume_all(&database, &queue)?;
    edits::import_all(&store, &queue, None)?;
    store.refresh_exports()?;
    let exporter = Store::new(&settings);
    std::thread::Builder::new()
        .name("amhd-export".into())
        .spawn(move || export_loop(&exporter, &commits))
        .map_err(|e| format!("amh: cannot start the exporter: {e}"))?;
    let importer = Store::new(&settings);
    std::thread::Builder::new()
        .name("amhd-import".into())
        .spawn({
            let queue = queue.clone();
            move || import_loop(&importer, &queue)
        })
        .map_err(|e| format!("amh: cannot start the edit importer: {e}"))?;
    std::thread::Builder::new()
        .name("amhd-watch".into())
        .spawn(|| {
            // The watcher must keep running: restart it when it returns or panics.
            loop {
                let code = std::panic::catch_unwind(crate::watch::run);
                eprintln!("amhd: the watcher ended ({code:?}); restarting it in 10 s");
                std::thread::sleep(Duration::from_secs(10));
            }
        })
        .map_err(|e| format!("amh: cannot start the watcher: {e}"))?;
    outbox::start(settings.clone(), store.state_dir.clone())?;
    listen(&store.state_dir)?;
    notify_systemd("READY=1");
    for beat in 1u64.. {
        std::thread::sleep(Duration::from_secs(3));
        notify_systemd("WATCHDOG=1");
        if beat % 100 == 0 {
            eprintln!("amhd: commit-queue wait p99 {} us", queue.wait_p99_us());
        }
    }
    Ok(())
}

/// Hold a flock on `amhd.pid` for the daemon's life; clients never take it.
fn single_instance(state_dir: &Path) -> Result<fs::File, String> {
    fs::create_dir_all(state_dir)
        .map_err(|e| format!("amh: cannot create {}: {e}", state_dir.display()))?;
    let path = state_dir.join("amhd.pid");
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("amh: cannot open {}: {e}", path.display()))?;
    if file.try_lock().is_err() {
        return Err(format!(
            "amh: amhd is already running (pid {})",
            fs::read_to_string(&path).unwrap_or_default().trim()
        ));
    }
    file.set_len(0)
        .and_then(|()| file.write_all(std::process::id().to_string().as_bytes()))
        .map_err(|e| format!("amh: cannot write {}: {e}", path.display()))?;
    Ok(file)
}

/// Re-export after each batch of commits, and at least every `EXPORT_TICK` for writes made outside the daemon.
fn export_loop(store: &Store, commits: &mpsc::Receiver<()>) {
    loop {
        match commits.recv_timeout(EXPORT_TICK) {
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            _ => while commits.try_recv().is_ok() {},
        }
        if let Err(error) = store.refresh_exports() {
            eprintln!("amhd: {error}");
        }
    }
}

/// Import a task file when inotify reports a write to it, and all changed files every `FULL_SCAN`.
fn import_loop(store: &Store, queue: &commit::Queue) {
    let watch = Inotify::watch(&store.root)
        .inspect_err(|error| {
            eprintln!(
                "amhd: inotify unavailable, scanning every {} s: {error}",
                FULL_SCAN.as_secs()
            )
        })
        .ok();
    let mut scanned = Instant::now();
    loop {
        let names = match &watch {
            Some(watch) => watch.wait(FULL_SCAN.saturating_sub(scanned.elapsed())),
            None => {
                std::thread::sleep(FULL_SCAN);
                Vec::new()
            }
        };
        let result = edits::initialized(store).and_then(|database| {
            if scanned.elapsed() >= FULL_SCAN {
                scanned = Instant::now();
                return edits::import_all(store, queue, None);
            }
            names
                .iter()
                .filter(|name| name.ends_with(".md"))
                .try_for_each(|name| edits::import(store, &database, queue, name, None))
        });
        if let Err(error) = result {
            eprintln!("amhd: {error}");
        }
    }
}

/// Tell systemd (`Type=notify`) about readiness and liveness; a no-op outside systemd.
fn notify_systemd(state: &str) {
    if let Some(path) = std::env::var_os("NOTIFY_SOCKET")
        && let Ok(socket) = UnixDatagram::unbound()
    {
        let _ = socket.send_to(state.as_bytes(), path);
    }
}

/// An inotify watch for finished writes and renames into one folder.
struct Inotify(fs::File);

impl Inotify {
    fn watch(folder: &Path) -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;
        use std::os::unix::ffi::OsStrExt;
        // SAFETY: plain syscalls; the fd is owned by the returned `File`.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let file = unsafe { fs::File::from_raw_fd(fd) };
        let folder = std::ffi::CString::new(folder.as_os_str().as_bytes())?;
        if unsafe {
            libc::inotify_add_watch(
                fd,
                folder.as_ptr(),
                libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(file))
    }

    /// The names written within `timeout`; empty on timeout.
    fn wait(&self, timeout: Duration) -> Vec<String> {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        let mut poll = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: `poll` points to one valid pollfd.
        if unsafe { libc::poll(&mut poll, 1, timeout_ms) } <= 0 {
            return Vec::new();
        }
        let mut buffer = [0u8; 64 * 1024];
        let Ok(n) = (&self.0).read(&mut buffer) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        let header = std::mem::size_of::<libc::inotify_event>();
        let mut at = 0;
        while at + header <= n {
            // SAFETY: the kernel wrote a whole event header at `at`.
            let event: libc::inotify_event =
                unsafe { std::ptr::read_unaligned(buffer[at..].as_ptr().cast()) };
            let name = &buffer[at + header..at + header + event.len as usize];
            let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
            names.push(String::from_utf8_lossy(name).into_owned());
            at += header + event.len as usize;
        }
        names
    }
}

/// Where clients reach the daemon.
pub fn socket_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("amhd.sock")
}

/// Serve client requests on `amhd.sock`, one thread per connection: a request is one JSON line `{"argv": [...], "env": {...}}`, the answer one line `{"code": N, "out": "...", "err": "..."}`.
// The rework design: "CLI: parses with clap locally; sends a request id, argv, cwd and an allowlist of request-scoped env"
fn listen(state_dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let path = socket_path(state_dir);
    let _ = fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path)
        .map_err(|e| format!("amh: cannot listen on {}: {e}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("amh: {e}"))?;
    std::thread::Builder::new()
        .name("amhd-listen".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = std::thread::Builder::new()
                    .name("amhd-request".into())
                    .spawn(move || answer(stream));
            }
        })
        .map_err(|e| format!("amh: cannot start the listener: {e}"))?;
    Ok(())
}

/// Read one request from `stream`, run it, write the answer.
fn answer(stream: std::os::unix::net::UnixStream) {
    use std::io::{BufRead, BufReader, Read};
    // A client that never finishes its line does not keep a thread forever.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let mut line = String::new();
    if BufReader::new((&stream).take(16 << 20))
        .read_line(&mut line)
        .is_err()
    {
        return;
    }
    let reply = match serde_json::from_str::<serde_json::Value>(&line) {
        Ok(request) => run_request(&request),
        Err(error) => {
            serde_json::json!({"code": 2, "out": "", "err": format!("amh: bad request: {error}\n")})
        }
    };
    let mut stream = stream;
    let _ = stream.write_all((reply.to_string() + "\n").as_bytes());
}

/// Run one request in this thread, as the caller it names.
pub fn run_request(request: &serde_json::Value) -> serde_json::Value {
    let argv: Vec<String> = request["argv"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|arg| arg.as_str().map(str::to_owned))
        .collect();
    let env: std::collections::BTreeMap<String, String> = request["env"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_owned())))
        .collect();
    let command = crate::cli::command();
    let matches = match command.clone().try_get_matches_from(&argv) {
        Ok(matches) => matches,
        Err(error) => return serde_json::json!({"code": 2, "out": "", "err": error.to_string()}),
    };
    let values = crate::parsed(&command, &matches);
    let cwd = std::path::PathBuf::from(request["cwd"].as_str().unwrap_or("/"));
    // A retried request returns the outcome of its first run.
    let state_dir = crate::settings::state_dir();
    let id = request["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    if let Some(id) = &id {
        match outbox::claim(&state_dir, id) {
            Ok(Some((code, out, err))) => {
                return serde_json::json!({"code": code, "out": out, "err": err, "replayed": true});
            }
            Ok(None) => {}
            Err(error) => return serde_json::json!({"code": 75, "out": "", "err": error + "\n"}),
        }
    }
    let began = Instant::now();
    crate::store::thread_lock_us();
    commit::take_wrote();
    let serving = outbox::serving_request(id.as_deref());
    // A panic is an error for this request; its outcome is still recorded, so a retry does not wait for it.
    let (result, out, mut err) = crate::request::serve(&env, &argv, &cwd, || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::actions::run(&values)
        }))
        .unwrap_or_else(|_| Err("amh: the request panicked inside amhd".to_owned()))
    });
    let code = match result {
        Ok(code) => code,
        Err(message) => {
            err += &(message + "\n");
            1
        }
    };
    drop(serving);
    let (lock_wait_us, lock_hold_us) = crate::store::thread_lock_us();
    // As the CLI does: a request that wrote refreshes the task files before it answers, so the caller reads its own write.
    if (commit::take_wrote() || lock_hold_us > 0)
        && let Err(error) =
            crate::settings::load().and_then(|settings| Store::new(&settings).refresh_exports())
    {
        eprintln!("amhd: task files not refreshed yet (the exporter will): {error}");
    }
    crate::request::serve(&env, &argv, &cwd, || {
        crate::log::record(
            &values.to_string(),
            &format!(
                "exit={code} duration_us={} lock_wait_us={} lock_hold_us={} via=amhd{}",
                began.elapsed().as_micros(),
                lock_wait_us,
                lock_hold_us,
                // The first line of a failure, so refusals (e.g. held agents) can be told from faults.
                match err.lines().find(|line| !line.trim().is_empty()) {
                    Some(line) if code != 0 => format!(
                        " error=\"{}\"",
                        line.chars()
                            .filter(|c| !matches!(c, '{' | '}' | '"'))
                            .take(120)
                            .collect::<String>()
                    ),
                    _ => String::new(),
                }
            ),
        )
    });
    if let Some(id) = &id
        && let Err(error) = outbox::record(&state_dir, id, code, &out, &err)
    {
        eprintln!("amhd: {error}");
    }
    serde_json::json!({"code": code, "out": out, "err": err})
}
