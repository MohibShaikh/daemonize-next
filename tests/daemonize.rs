//! Runs `examples/daemon.rs`, which `cargo test` builds, and checks what the launcher and the
//! daemon see. Tests that need root run when `DAEMONIZE_ROOT_TESTS=1`.

use std::fs::{self, File};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn example() -> PathBuf {
    // target/<profile>/deps/<test> -> target/<profile>/examples/daemon, unless build-dir
    // moved the test binary out of the target directory.
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().unwrap().parent().unwrap();
    let profile = profile_dir.file_name().unwrap();
    let target = option_env!("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target"));
    [
        profile_dir.join("examples/daemon"),
        target.join(profile).join("examples/daemon"),
    ]
    .into_iter()
    .find(|p| p.exists())
    .expect("examples/daemon not built; run cargo test")
}

fn run(args: &[&str]) -> Output {
    Command::new(example()).args(args).output().unwrap()
}

fn wait_for(path: &Path) -> String {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok(s) = fs::read_to_string(path) {
            if s.contains("Pid:") {
                return s;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("daemon never wrote {}", path.display());
}

fn kill_daemon(report: &str) {
    if let Some(pid) = report.lines().find_map(|l| l.strip_prefix("Pid: ")) {
        unsafe { libc::kill(pid.trim().parse().unwrap(), libc::SIGKILL) };
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn starts_and_writes_pid_file() {
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("d.pid");
    let report = dir.path().join("report");
    let o = run(&[
        "--pid-file",
        pid.to_str().unwrap(),
        "--report",
        report.to_str().unwrap(),
        "--sleep",
        "30",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    let r = wait_for(&report);
    let daemon_pid = r.lines().find_map(|l| l.strip_prefix("Pid: ")).unwrap();
    assert_eq!(fs::read_to_string(&pid).unwrap(), format!("{daemon_pid}\n"));
    assert!(r.contains("Cwd: /\n"));
    kill_daemon(&r);
}

#[test]
fn locked_pid_file_fails_the_launcher() {
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("d.pid");
    let held = File::create(&pid).unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );

    let o = run(&["--pid-file", pid.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    let err = stderr(&o);
    assert!(err.contains("unable to lock pid file"), "{err}");
    assert!(err.contains(pid.to_str().unwrap()), "{err}");
    assert!(err.contains("is the daemon already running?"), "{err}");
}

#[test]
fn missing_working_directory_fails_the_launcher() {
    let o = run(&["--dir", "/nonexistent/daemonize-next"]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("unable to change directory /nonexistent/daemonize-next"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn unknown_user_fails_before_forking() {
    let o = run(&["--user", "no-such-user-daemonize-next"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o)
            .contains("unable to resolve user name to user id \"no-such-user-daemonize-next\""),
        "{}",
        stderr(&o)
    );
}

#[test]
#[cfg(target_os = "linux")]
fn refuses_a_multithreaded_launcher() {
    let o = run(&["--thread"]);
    assert_eq!(o.status.code(), Some(2));
    let err = stderr(&o);
    assert!(
        err.contains("refusing to fork a multithreaded process: 2 threads are running"),
        "{err}"
    );
    assert!(err.contains(".allow_threads(true)"), "{err}");
}

#[test]
fn execute_reports_to_the_parent() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report");
    let o = run(&[
        "--execute",
        "--report",
        report.to_str().unwrap(),
        "--sleep",
        "30",
    ]);
    assert_eq!(String::from_utf8_lossy(&o.stdout), "parent: ok\n");
    kill_daemon(&wait_for(&report));

    let pid = dir.path().join("d.pid");
    let held = File::create(&pid).unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let o = run(&["--execute", "--pid-file", pid.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&o.stdout).starts_with("parent: error: unable to lock pid file")
    );
}

fn root_tests() -> bool {
    std::env::var_os("DAEMONIZE_ROOT_TESTS").is_some_and(|v| v == "1")
}

#[test]
fn dropping_to_a_user_drops_root_groups() {
    if !root_tests() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    // The daemon writes the report after dropping to nobody.
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
    let report = dir.path().join("report");
    let o = run(&[
        "--user",
        "nobody",
        "--report",
        report.to_str().unwrap(),
        "--sleep",
        "30",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    let r = wait_for(&report);
    kill_daemon(&r);
    let nobody = unsafe { &*libc::getpwnam(c"nobody".as_ptr()) };
    let field = |name: &str| {
        r.lines()
            .find_map(|l| l.strip_prefix(name))
            .unwrap()
            .split_whitespace()
            .map(String::from)
            .collect::<Vec<_>>()
    };
    assert!(
        field("Uid:")
            .iter()
            .all(|u| *u == nobody.pw_uid.to_string()),
        "{r}"
    );
    assert!(
        field("Gid:")
            .iter()
            .all(|g| *g == nobody.pw_gid.to_string()),
        "{r}"
    );
    assert!(!field("Groups:").contains(&"0".to_string()), "{r}");
}

#[test]
fn chown_with_only_a_group_keeps_the_owner() {
    if !root_tests() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let pid = dir.path().join("d.pid");
    let report = dir.path().join("report");
    let o = run(&[
        "--pid-file",
        pid.to_str().unwrap(),
        "--group",
        "nogroup",
        "--chown",
        "--report",
        report.to_str().unwrap(),
        "--sleep",
        "30",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    kill_daemon(&wait_for(&report));
    use std::os::unix::fs::MetadataExt;
    assert_eq!(fs::metadata(&pid).unwrap().uid(), 0);
}
