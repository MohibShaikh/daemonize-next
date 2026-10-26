---
name: daemonize-next
description: Use when Rust code daemonizes a process (fork, setsid, pid file, dropping to a user), when Cargo.toml depends on `daemonize`, or when cargo audit or cargo deny reports RUSTSEC-2025-0069.
---

# daemonize-next

Daemonize a Rust process with the `daemonize` 0.5 API, minus its bugs.

## Migrating from daemonize

1. In `Cargo.toml`, replace the `daemonize` line with
   `daemonize = { package = "daemonize-next", version = "0.1" }`.
2. Leave the code alone. `use daemonize::Daemonize;` and every builder call stay the same.
3. Run the program once. If it now fails with "refusing to fork a multithreaded process",
   move `start()` before the code that starts threads (for tokio: before
   `#[tokio::main]` or `Runtime::new()`, so use a plain `fn main`).

## Writing new code

- Call `Daemonize::new()...start()` first thing in `main`, before threads or a runtime.
- In the launcher, `start()` returns `Err` when any step fails: print it with `{}` and exit
  non-zero. On success the launcher exits 0 by itself.
- Use `.privileged_action(|| ...)` only for short root-only setup such as binding port 80.
  The launcher waits for it.
- `.user(...)` as root also sets the user's primary group and supplementary groups. Don't
  add `setgroups` or `setgid` calls by hand.

## Don't

- Don't depend on `daemonize` 0.5 or `daemonize2` for new code: neither drops supplementary
  groups, and both lose errors from steps after the second fork.
- Don't set `.allow_threads(true)` to silence the thread error. Move the call instead.
