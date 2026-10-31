//! Advisory local wakeups; committed SQLite events remain the authority.

use std::fs;
use std::io;
use std::os::unix::{fs::PermissionsExt, net::UnixDatagram};
use std::path::Path;
use std::time::Duration;

const SOCKET: &str = "amh-events.sock";

// 🧑 "keep doing SQLite for now but also emit events for internal coordination"
pub fn notify(database: &Path) {
    if let Some(folder) = database.parent()
        && let Ok(socket) = UnixDatagram::unbound()
        && socket.set_nonblocking(true).is_ok()
    {
        let _ = socket.send_to(b"event", folder.join(SOCKET));
    }
}

pub fn listen(folder: &Path) -> io::Result<UnixDatagram> {
    fs::create_dir_all(folder)?;
    let path = folder.join(SOCKET);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let socket = UnixDatagram::bind(&path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(socket)
}

/// Wait for a commit or the next external poll, coalescing queued wakeups.
pub fn wait(socket: &UnixDatagram, timeout: Duration) -> io::Result<()> {
    if timeout.is_zero() {
        return Ok(());
    }
    socket.set_read_timeout(Some(timeout))?;
    let mut data = [0; 16];
    match socket.recv(&mut data) {
        Ok(_) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    }
    socket.set_nonblocking(true)?;
    while socket.recv(&mut data).is_ok() {}
    socket.set_nonblocking(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_committed_events_wake_the_listener() -> Result<(), Box<dyn std::error::Error>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("amh-wake-{}-{now}", std::process::id()));
        let socket = listen(&dir)?;
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;
        let mut database = crate::database::Database::open(&dir.join("amh.sqlite3"))?;
        let event = serde_json::json!({"time":"now", "actor":"test", "kind":"task changed", "subject":"t.md", "data":{}});
        database.commit(&[], std::slice::from_ref(&event))?;
        let mut data = [0; 16];
        assert!(socket.recv(&mut data).is_ok());
        assert_eq!(database.events()?.len(), 1);
        assert!(database.commit(&[], &[serde_json::json!({})]).is_err());
        assert!(socket.recv(&mut data).is_err());
        database.commit(&[], &[event])?;
        wait(&socket, Duration::from_millis(50))?;
        assert!(socket.recv(&mut data).is_err());
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
