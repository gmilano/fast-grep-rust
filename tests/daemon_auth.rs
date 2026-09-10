//! The daemon control socket must reject commands without the right token.
#![cfg(feature = "daemon")]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use fast_grep::{daemon, persist};

fn wait_for(path: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Send `token`\n`cmd`\n on a raw connection and return the response line.
fn raw_send(port: u16, token: &str, cmd: &str) -> std::io::Result<String> {
    let stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut writer = std::io::BufWriter::new(&stream);
    writeln!(writer, "{token}")?;
    writeln!(writer, "{cmd}")?;
    writer.flush()?;
    drop(writer);
    let mut reader = BufReader::new(&stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    Ok(resp.trim().to_string())
}

#[test]
fn daemon_rejects_unauthenticated_commands() {
    let corpus = tempfile::tempdir().unwrap();
    std::fs::write(corpus.path().join("a.txt"), "hello world\nfoo bar baz\n").unwrap();
    let idx = corpus.path().join(".fgr");
    persist::build(corpus.path(), &idx, false, &[], false, false).unwrap();

    // Start the daemon in a background thread; it blocks in its event loop.
    let idx_bg = idx.clone();
    let handle = std::thread::spawn(move || {
        let _ = daemon::start_daemon(&idx_bg);
    });

    // Wait for the daemon to publish its port + token.
    let token_path = idx.join("daemon.token");
    let port_path = idx.join("daemon.port");
    assert!(
        wait_for(&token_path, Duration::from_secs(15))
            && wait_for(&port_path, Duration::from_secs(15)),
        "daemon did not come up"
    );
    let port: u16 = std::fs::read_to_string(&port_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    // 1. A wrong token must be rejected — and must NOT stop the daemon.
    let bad = raw_send(port, "deadbeef", "stop").unwrap();
    assert_eq!(bad, "error: unauthorized");
    let blank = raw_send(port, "", "status").unwrap();
    assert_eq!(blank, "error: unauthorized");

    // 2. The daemon is still alive: an authorized command (real token from the
    //    file) still answers.
    let status = daemon::send_command(&idx, "status").expect("authorized status");
    assert!(status == "clean" || status == "dirty", "got {status:?}");

    // 3. An authorized stop shuts it down.
    let stopped = daemon::send_command(&idx, "stop").expect("authorized stop");
    assert_eq!(stopped, "stopped");

    handle.join().unwrap();
    // Token file is cleaned up on shutdown.
    assert!(!token_path.exists());
}
