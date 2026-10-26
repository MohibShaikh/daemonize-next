# daemonize-next

A drop-in replacement for [`daemonize`](https://crates.io/crates/daemonize) 0.5, which is
unmaintained ([RUSTSEC-2025-0069](https://rustsec.org/advisories/RUSTSEC-2025-0069.html)),
with its privilege-drop and error-reporting bugs fixed.

The library is named `daemonize`, so switching is one line and no code change:

```toml
[dependencies]
daemonize = { package = "daemonize-next", version = "0.1" }
```

```rust
use daemonize::Daemonize;

match Daemonize::new().pid_file("/run/app.pid").user("app").start() {
    Ok(()) => { /* daemon */ }
    Err(e) => { eprintln!("{e}"); std::process::exit(1) }
}
```

## What changes from daemonize 0.5

Measured with [`examples/daemon.rs`](examples/daemon.rs) built against each crate
([`repro/v05`](repro/v05) builds it against 0.5.0), on Linux 7.0 as a normal user and as root
in a `debian:trixie` container where root was also in groups 4 (adm) and 27 (sudo):

| Case | daemonize 0.5.0 | daemonize-next |
|---|---|---|
| Pid file already locked | launcher exits **0** | exits 2: `unable to lock pid file /tmp/…/locked.pid: Resource temporarily unavailable (os error 11) (another process holds the lock; is the daemon already running?)` |
| Working directory missing | launcher exits **0** | exits 2: `unable to change directory /nonexistent: No such file or directory (os error 2)` |
| Root, `.user("nobody")` | gid **0**, groups **0 4 27** | gid 65534, groups 65534 |
| Root, `.group("nogroup").chown_pid_file(true)` | pid file owner **4294967294**:65534 | 0:65534 |
| Another thread running | forks anyway | refuses, unless `.allow_threads(true)` |

Why each happens in 0.5.0 (`src/lib.rs` of the published crate):

- No `setgroups` or `initgroups` anywhere, and `setgid` only runs when `group` is set
  (lines 417-418), so a daemon dropped to `nobody` keeps root's gid and supplementary groups.
- The first child exits 0 right after the second fork (lines 377-378), before the pid file,
  `chown`, `chroot`, `setgid` and `setuid` steps, so their errors never reach the launcher. A
  failure before that fork exits the first child non-zero, but `start()` passes the raw
  `waitpid` status to `exit` (line 343), which keeps only the low 8 bits: 2 becomes 512
  becomes 0.
- `chown_pid_file` with only one of `user` and `group` passes `u32::MAX - 1` for the other
  (lines 396-397). `chown` leaves a field unchanged only for `-1`, which is `u32::MAX`.
- `privileged_action` transmutes `Daemonize<T>` into `Daemonize<N>`
  ([knsd/daemonize#57](https://github.com/knsd/daemonize/pull/57)).

How it works now: user and group names, the group list and every path are resolved before
the first fork, so those errors return from `start()` directly. A close-on-exec pipe carries
the result of every later step back to the launcher, which waits for it before returning.

Behavior that differs on purpose:

- `start()` and `execute()` return in the launcher only once the daemon has finished its
  setup, including the privileged action. Keep that action short.
- On failure, `start()` returns the error in the launcher. The daemon side exits 1.
- A process with more than one thread gets an error instead of a fork. Daemonize before
  starting threads, or opt out with `.allow_threads(true)`. Linux only; elsewhere it is not
  checked.

## Other options

| Crate | Same API as daemonize 0.5 | Drops supplementary groups | Launcher sees later errors | Dependents |
|---|---|---|---|---|
| daemonize 0.5.0 | yes | no | no | 184 |
| daemonize2 0.7.0 | no: `start` and `execute` are `unsafe fn`, `chown_pid_file` is split in two | no (no `setgroups` in source) | no (`exit(0)` after the second fork, `src/lib.rs:431-432`) | 4 |
| blivet 0.15.0 | no: a config struct and `daemonize(&config)`, the launcher never returns, no `chroot`, stdout takes a path | yes | yes | 0 |
| fork 0.11.0 | no: fork, `setsid` and stdio helpers, no pid file or user switch | n/a | n/a | |
| `nix::unistd::daemon` | no: `daemon(3)`, no pid file or user switch | n/a | n/a | |

Dependent counts are from crates.io on 2026-10-26. daemonize2's rows are read from its
published source, not run.

## For coding agents

- Code written for `daemonize` 0.5 compiles unchanged. Migrate by changing the dependency
  line above; don't rewrite call sites.
- `cargo audit` reporting RUSTSEC-2025-0069 for `daemonize` is fixed by that line.
- `Daemonize` and `Outcome` are `#[must_use]`, and every error says which step failed, on
  which path, and what usually fixes it. Print it with `{}`.
- Call `start()` before creating a tokio runtime or any thread.

## Testing

```sh
cargo test
# as root: the privilege-drop tests
sudo -E env "PATH=$PATH" DAEMONIZE_ROOT_TESTS=1 cargo test --test daemonize
```

## License

MIT or Apache-2.0, at your option. The API follows `daemonize` by Fedor Gogolev; the
implementation is new.
