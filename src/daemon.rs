use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};

use crate::persist;

const DEBOUNCE_SECS: u64 = 3;
const PID_FILE: &str = "daemon.pid";
const PORT_FILE: &str = "daemon.port";
const TOKEN_FILE: &str = "daemon.token";

enum Event {
    FsChange(Vec<PathBuf>),
    Socket(TcpStream),
    Shutdown,
}

struct Daemon {
    index_path: PathBuf,
    root_dir: PathBuf,
    dirty: bool,
    pending_changes: HashSet<PathBuf>,
    last_event_time: Instant,
    /// Secret required on every control command (see `daemon.token`).
    token: String,
}

// --- PID/port file management ---

fn write_pid_file(index_path: &Path) -> Result<()> {
    let pid = std::process::id();
    std::fs::write(index_path.join(PID_FILE), pid.to_string())?;
    Ok(())
}

fn read_pid_file(index_path: &Path) -> Option<u32> {
    std::fs::read_to_string(index_path.join(PID_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn write_port_file(index_path: &Path, port: u16) -> Result<()> {
    std::fs::write(index_path.join(PORT_FILE), port.to_string())?;
    Ok(())
}

fn read_port_file(index_path: &Path) -> Option<u16> {
    std::fs::read_to_string(index_path.join(PORT_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

// --- Control-socket auth token (local cookie) ---
//
// The daemon binds a localhost port that any local process could otherwise
// connect to and `stop`. We gate every command on a per-daemon secret token
// written to `daemon.token`. The real boundary is the file's permissions
// (owner-only on Unix); the token just stops a process that cannot read that
// file from driving the daemon.

/// Generate a random control token, persist it to `daemon.token`, and return it.
fn write_token_file(index_path: &Path) -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("generating daemon token: {e}"))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    write_token_secure(&index_path.join(TOKEN_FILE), token.as_bytes())?;
    Ok(token)
}

fn read_token_file(index_path: &Path) -> Option<String> {
    std::fs::read_to_string(index_path.join(TOKEN_FILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Write the token file readable only by the owner. On Unix the `0600` mode is
/// applied atomically at create time so it is never briefly world-readable.
#[cfg(unix)]
fn write_token_secure(path: &Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    f.flush()?;
    Ok(())
}

/// Windows has no portable std permission API; the token file inherits the
/// index directory's ACLs (typically user-restricted under a profile). Best
/// effort — documented in the README.
#[cfg(not(unix))]
fn write_token_secure(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data)?;
    Ok(())
}

/// Constant-time equality so token comparison leaks no timing signal.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn remove_daemon_files(index_path: &Path) {
    let _ = std::fs::remove_file(index_path.join(PID_FILE));
    let _ = std::fs::remove_file(index_path.join(PORT_FILE));
    let _ = std::fs::remove_file(index_path.join(TOKEN_FILE));
}

fn is_process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        // Unix: signal 0 checks if process exists
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
}

/// Check if a daemon is running for this index.
pub fn is_daemon_running(index_path: &Path) -> bool {
    match read_pid_file(index_path) {
        Some(pid) => {
            if is_process_alive(pid) {
                true
            } else {
                // Stale PID file — clean up
                remove_daemon_files(index_path);
                false
            }
        }
        None => false,
    }
}

/// Send a command to the daemon and return its response.
pub fn send_command(index_path: &Path, cmd: &str) -> Result<String> {
    let port = read_port_file(index_path).context("no daemon port file found")?;
    let token = read_token_file(index_path)
        .context("daemon token file missing — is the daemon running?")?;
    let stream = TcpStream::connect(("127.0.0.1", port)).context("connecting to daemon")?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(&stream);
    let mut writer = std::io::BufWriter::new(&stream);
    // Auth line first, then the command.
    writeln!(writer, "{}", token)?;
    writeln!(writer, "{}", cmd)?;
    writer.flush()?;
    let mut response = String::new();
    reader.read_line(&mut response)?;
    Ok(response.trim().to_string())
}

// --- Daemon update logic ---

impl Daemon {
    fn run_update(&mut self) -> Result<()> {
        if self.pending_changes.is_empty() && !self.dirty {
            return Ok(());
        }
        // Non-blocking lock: if a background compaction holds it, skip this
        // round (keep dirty/pending) and retry on the next debounce — the event
        // loop must never block, so it keeps answering `status` while a
        // compaction runs and searches never stall.
        let lock = match persist::try_acquire_index_lock(&self.index_path)? {
            Some(l) => l,
            None => return Ok(()),
        };
        let stats = persist::update_incremental(&self.index_path, &self.root_dir, false)?;
        persist::release_index_lock(&self.index_path);
        drop(lock);
        self.pending_changes.clear();
        self.dirty = false;
        if stats.added > 0 || stats.modified > 0 || stats.deleted > 0 {
            eprintln!(
                "[daemon] Updated index: +{} added, {} modified, {} deleted in {}ms",
                stats.added, stats.modified, stats.deleted, stats.duration_ms
            );
        }

        // Rebaseline OFF the event loop when divergence crosses the threshold.
        // The worker re-acquires the lock itself; since it holds the lock for
        // the whole fold, the loop's next try-lock update simply skips until it
        // finishes — so only one compaction runs at a time without any tracking.
        let cfg = crate::config::load(&self.index_path);
        if cfg
            .compaction
            .should_compact(stats.main_docs, stats.delta_docs, stats.tombstones)
        {
            let idx = self.index_path.clone();
            eprintln!("[daemon] Divergence over threshold — compacting in background...");
            std::thread::spawn(move || match persist::compact(&idx, false) {
                Ok(out) => {
                    if let Some(s) = out.stats {
                        eprintln!(
                            "[daemon] Auto-compacted: {} live docs, {} dropped, {} trigrams",
                            s.live_docs, s.dropped_docs, s.num_ngrams
                        );
                    }
                }
                Err(e) => eprintln!("[daemon] Compaction error: {}", e),
            });
        }
        Ok(())
    }
}

/// Start the daemon for the given index. Blocks until stopped.
pub fn start_daemon(index_path: &Path) -> Result<()> {
    if is_daemon_running(index_path) {
        anyhow::bail!(
            "Daemon already running for this index (PID {})",
            read_pid_file(index_path).unwrap_or(0)
        );
    }

    // Verify index exists
    if !persist::index_exists(index_path) {
        anyhow::bail!(
            "No index found at {:?}. Build one first with: fgr index {:?}",
            index_path,
            index_path.parent().unwrap_or(Path::new("."))
        );
    }

    // Load index and get root dir
    let idx = persist::load(index_path)?;
    let root_dir = PathBuf::from(&idx.meta.root_dir);
    eprintln!(
        "[daemon] Starting for index at {:?}, root: {:?}",
        index_path, root_dir
    );

    // Full stale check at startup (zero false negatives)
    if persist::full_stale_check(&idx, index_path) {
        eprintln!("[daemon] Index is stale, updating...");
        let (_lock, _) = persist::acquire_index_lock(index_path)?;
        let stats = persist::update_incremental(index_path, &root_dir, true)?;
        let compaction = persist::maybe_auto_compact(index_path, &stats, false)?;
        persist::release_index_lock(index_path);
        eprintln!(
            "[daemon] Startup update: +{} added, {} modified, {} deleted in {}ms",
            stats.added, stats.modified, stats.deleted, stats.duration_ms
        );
        if let Some(s) = compaction.and_then(|c| c.stats) {
            eprintln!(
                "[daemon] Startup auto-compacted: {} live docs, {} dropped, {} trigrams",
                s.live_docs, s.dropped_docs, s.num_ngrams
            );
        }
    } else {
        eprintln!("[daemon] Index is up to date");
    }
    drop(idx);

    // Bind TCP listener on localhost (OS picks a free port)
    let listener = TcpListener::bind("127.0.0.1:0").context("binding TCP listener")?;
    let port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;

    // Write PID and port files, and a per-daemon control token (gates commands).
    write_pid_file(index_path)?;
    write_port_file(index_path, port)?;
    let token = write_token_file(index_path)?;
    eprintln!("[daemon] PID: {}, port: {}", std::process::id(), port);

    // Set up event channel
    let (tx, rx) = mpsc::channel::<Event>();

    // Start filesystem watcher
    let tx_fs = tx.clone();
    let idx_path_clone = index_path.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let paths: Vec<PathBuf> = event
                .paths
                .into_iter()
                .filter(|p| !p.starts_with(&idx_path_clone))
                .collect();
            if !paths.is_empty() {
                let _ = tx_fs.send(Event::FsChange(paths));
            }
        }
    })?;
    watcher.watch(&root_dir, RecursiveMode::Recursive)?;
    eprintln!("[daemon] Watching {:?} for changes", root_dir);

    // Accept connections in a separate thread
    let tx_sock = tx.clone();
    std::thread::spawn(move || loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = tx_sock.send(Event::Socket(stream));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    });

    // Handle Ctrl+C
    let tx_sig = tx.clone();
    ctrlc::set_handler(move || {
        let _ = tx_sig.send(Event::Shutdown);
    })
    .context("setting Ctrl+C handler")?;

    let mut daemon = Daemon {
        index_path: index_path.to_path_buf(),
        root_dir,
        dirty: false,
        pending_changes: HashSet::new(),
        last_event_time: Instant::now(),
        token,
    };

    eprintln!("[daemon] Ready, listening for events...");

    // Main event loop
    loop {
        let timeout = if daemon.dirty {
            let elapsed = daemon.last_event_time.elapsed();
            let debounce = Duration::from_secs(DEBOUNCE_SECS);
            if elapsed >= debounce {
                Duration::from_millis(1) // fire immediately
            } else {
                debounce - elapsed
            }
        } else {
            Duration::from_secs(60)
        };

        match rx.recv_timeout(timeout) {
            Ok(Event::FsChange(paths)) => {
                for p in paths {
                    daemon.pending_changes.insert(p);
                }
                daemon.dirty = true;
                daemon.last_event_time = Instant::now();
            }
            Ok(Event::Socket(stream)) => {
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut reader = BufReader::new(&stream);
                let mut writer = std::io::BufWriter::new(&stream);
                // First line is the auth token; reject before reading or acting
                // on any command (in particular, never `stop` unauthenticated).
                let mut auth = String::new();
                if reader.read_line(&mut auth).is_err()
                    || !constant_time_eq(auth.trim().as_bytes(), daemon.token.as_bytes())
                {
                    let _ = writeln!(writer, "error: unauthorized");
                    let _ = writer.flush();
                    continue;
                }
                let mut line = String::new();
                if reader.read_line(&mut line).is_ok() {
                    match line.trim() {
                        "status" => {
                            let resp = if daemon.dirty { "dirty" } else { "clean" };
                            let _ = writeln!(writer, "{}", resp);
                            let _ = writer.flush();
                        }
                        "flush" => {
                            if let Err(e) = daemon.run_update() {
                                eprintln!("[daemon] Update error: {}", e);
                            }
                            let _ = writeln!(writer, "ready");
                            let _ = writer.flush();
                        }
                        "stop" => {
                            let _ = writeln!(writer, "stopped");
                            let _ = writer.flush();
                            eprintln!("[daemon] Stop requested");
                            break;
                        }
                        other => {
                            let _ = writeln!(writer, "error: unknown command '{}'", other);
                            let _ = writer.flush();
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Debounce timer expired — run pending update
                if daemon.dirty {
                    if let Err(e) = daemon.run_update() {
                        eprintln!("[daemon] Update error: {}", e);
                    }
                }
            }
            Ok(Event::Shutdown) => {
                eprintln!("[daemon] Shutting down...");
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Cleanup
    remove_daemon_files(&daemon.index_path);
    eprintln!("[daemon] Stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_basics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab")); // different length
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn token_file_roundtrips_and_differs() {
        let dir = tempfile::tempdir().unwrap();
        let token = write_token_file(dir.path()).unwrap();
        assert_eq!(token.len(), 64, "32 random bytes hex-encoded");
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(read_token_file(dir.path()).as_deref(), Some(token.as_str()));
        // A fresh token is different (overwrites the file).
        let token2 = write_token_file(dir.path()).unwrap();
        assert_ne!(token, token2);
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write_token_file(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(TOKEN_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn missing_token_file_reads_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_token_file(dir.path()), None);
    }
}
