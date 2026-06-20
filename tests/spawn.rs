//! Integration test: the `md-preview <file>` launch path auto-spawns a detached
//! daemon when none is running, returns promptly, opens the browser, and a
//! second invocation reuses the SAME daemon.
//!
//! This exercises the real binary (`CARGO_BIN_EXE_md-preview`) end-to-end in an
//! isolated environment (temp HOME/XDG dirs, a `BROWSER` recorder script). It
//! asserts the four properties the chair asked for:
//!   (a) the command RETURNS (doesn't hang) within a few seconds,
//!   (b) a detached daemon is now running and listening on the control socket,
//!   (c) the recorder logged a `file://…bootstrap.html` URL,
//!   (d) a SECOND invocation reuses the daemon and also returns + logs a URL.
//!
//! It tracks the spawned daemon's pgid and kills ONLY that group at the end —
//! no broad `pkill`.

#![cfg(all(feature = "daemon", target_os = "linux"))]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Read the pid of the listening daemon from the control socket's directory by
/// scanning `/proc` for a process whose argv contains `--daemon` and whose
/// `XDG_RUNTIME_DIR` matches ours. Returns the first match.
fn find_daemon_pid(runtime_dir: &Path) -> Option<u32> {
    let want = runtime_dir.to_string_lossy().into_owned();
    for entry in fs::read_dir("/proc").ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let pid: u32 = match name.to_string_lossy().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let cmdline = match fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // argv is NUL-separated.
        let args: Vec<String> = cmdline
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if !args.iter().any(|a| a == "--daemon") {
            continue;
        }
        // Confirm the process's environment points at our isolated runtime dir,
        // so we never touch a real daemon belonging to the developer.
        if let Ok(environ) = fs::read(format!("/proc/{pid}/environ")) {
            let env_ok = environ
                .split(|b| *b == 0)
                .filter_map(|kv| std::str::from_utf8(kv).ok())
                .any(|kv| kv == format!("XDG_RUNTIME_DIR={want}"));
            if env_ok {
                return Some(pid);
            }
        }
    }
    None
}

/// Kill the entire process group of `pid` (the detached daemon is its own
/// session/group leader, so this reaps it and any children).
fn kill_group(pid: u32) {
    // SAFETY: `kill` is async-signal-safe; negative pid targets the group.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
}

#[test]
fn launch_path_spawns_detached_daemon_and_reuses_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path();
    let home = base.join("home");
    let runtime = base.join("run");
    let cache = base.join("cache");
    for d in [&home, &runtime, &cache] {
        fs::create_dir_all(d).expect("mkdir");
    }
    // The control dir requires a 0700 runtime dir.
    fs::set_permissions(&runtime, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .expect("chmod runtime");

    // A recorder `BROWSER`: logs the URL it is asked to open.
    let opened_log = base.join("opened.log");
    let rec = base.join("rec.sh");
    {
        let mut f = fs::File::create(&rec).expect("create recorder");
        write!(
            f,
            "#!/bin/sh\necho \"$1\" >> {}\n",
            opened_log.to_string_lossy()
        )
        .expect("write recorder");
    }
    fs::set_permissions(&rec, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod recorder");

    let file_a = base.join("a.md");
    let file_b = base.join("b.md");
    fs::write(&file_a, b"# A\n").expect("write a");
    fs::write(&file_b, b"# B\n").expect("write b");

    let bin = PathBuf::from(env!("CARGO_BIN_EXE_md-preview"));

    let run = |file: &Path| -> std::process::ExitStatus {
        Command::new(&bin)
            .arg(file)
            .env("HOME", &home)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CACHE_HOME", &cache)
            .env("BROWSER", &rec)
            // Make sure no inherited override leaks in.
            .env_remove("MD_PREVIEW_NO_OPEN")
            .status()
            .expect("spawn md-preview")
    };

    // (a) First invocation: no daemon yet → must spawn one and RETURN promptly.
    let start = Instant::now();
    let status1 = run(&file_a);
    let elapsed1 = start.elapsed();
    assert!(status1.success(), "first invocation must exit 0");
    assert!(
        elapsed1 < Duration::from_secs(8),
        "first invocation must return promptly, took {elapsed1:?}"
    );

    // (b) A detached daemon is now running and listening.
    let pid = {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(pid) = find_daemon_pid(&runtime) {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "no detached --daemon process found after first invocation"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let sock = runtime.join("md-preview").join("sock");
    assert!(sock.exists(), "control socket must exist after spawn");

    // (c) The recorder logged a file://…bootstrap.html URL.
    let log1 = wait_for_log_lines(&opened_log, 1);
    assert!(
        log1[0].starts_with("file://") && log1[0].contains("bootstrap.html"),
        "recorder must log a file://…bootstrap.html URL, got {:?}",
        log1[0]
    );

    // (d) Second invocation reuses the SAME daemon and also returns + logs.
    let start2 = Instant::now();
    let status2 = run(&file_b);
    let elapsed2 = start2.elapsed();
    assert!(status2.success(), "second invocation must exit 0");
    assert!(
        elapsed2 < Duration::from_secs(8),
        "second invocation must return promptly, took {elapsed2:?}"
    );
    // Same daemon pid still the only one.
    assert_eq!(
        find_daemon_pid(&runtime),
        Some(pid),
        "second invocation must reuse the same daemon, not spawn a new one"
    );
    let log2 = wait_for_log_lines(&opened_log, 2);
    assert!(
        log2[1].starts_with("file://") && log2[1].contains("bootstrap.html"),
        "second invocation must also log a bootstrap URL, got {:?}",
        log2[1]
    );

    // Cleanup: kill ONLY the daemon group we spawned.
    kill_group(pid);
}

/// Poll `path` until it contains at least `n` non-empty lines (bounded).
fn wait_for_log_lines(path: &Path, n: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let lines: Vec<String> = fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect();
        if lines.len() >= n {
            return lines;
        }
        assert!(
            Instant::now() < deadline,
            "log {path:?} did not reach {n} lines"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
