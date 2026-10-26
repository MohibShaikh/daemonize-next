//! Daemonizes with the options given on the command line. The daemon writes its uid, gid,
//! groups and working directory to `--report`, then sleeps. The integration tests drive it.
//!
//!     cargo run --example daemon -- --pid-file /tmp/d.pid --report /tmp/d.txt --sleep 30

use std::fs::File;
use std::io::Write;
use std::time::Duration;

use daemonize::{Daemonize, Outcome};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut d = Daemonize::new();
    let (mut report, mut sleep, mut execute) = (None, 0, false);
    while let Some(arg) = args.next() {
        let mut value = || args.next().expect("missing value");
        match arg.as_str() {
            "--pid-file" => d = d.pid_file(value()),
            "--dir" => d = d.working_directory(value()),
            "--user" => d = d.user(value().as_str()),
            "--group" => d = d.group(value().as_str()),
            "--chroot" => d = d.chroot(value()),
            "--chown" => d = d.chown_pid_file(true),
            "--report" => report = Some(value()),
            "--sleep" => sleep = value().parse().unwrap(),
            "--execute" => execute = true,
            "--thread" => {
                std::thread::spawn(|| std::thread::sleep(Duration::from_secs(60)));
                std::thread::sleep(Duration::from_millis(50));
            }
            other => panic!("unknown argument {other}"),
        }
    }

    if execute {
        match d.execute() {
            Outcome::Parent(Ok(_)) => println!("parent: ok"),
            Outcome::Parent(Err(e)) => {
                println!("parent: error: {e}");
                std::process::exit(2);
            }
            Outcome::Child(Ok(_)) => run_daemon(report, sleep),
            Outcome::Child(Err(_)) => std::process::exit(3),
        }
    } else {
        match d.start() {
            Ok(()) => run_daemon(report, sleep),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        }
    }
}

fn run_daemon(report: Option<String>, sleep: u64) {
    if let Some(path) = report {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let mut out = File::create(path).unwrap();
        for line in status
            .lines()
            .filter(|l| l.starts_with("Uid:") || l.starts_with("Gid:") || l.starts_with("Groups:"))
        {
            writeln!(out, "{line}").unwrap();
        }
        writeln!(out, "Cwd: {}", std::env::current_dir().unwrap().display()).unwrap();
        writeln!(out, "Pid: {}", std::process::id()).unwrap();
    }
    std::thread::sleep(Duration::from_secs(sleep));
}
