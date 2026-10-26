# Changelog

## 0.1.0

First release. The `daemonize` 0.5 API, with:

- Supplementary groups replaced with the user's when dropping from root, and the user's
  primary group used when no group is set.
- Errors from every step reach the launcher through a status pipe; `start()` returns them
  there.
- The first child's real exit code instead of the raw `waitpid` status.
- `chown_pid_file` leaves the unset owner field unchanged.
- Names, group lists and paths resolved before forking.
- Refuses to fork a multithreaded process unless `allow_threads(true)`.
- `privileged_action` without `transmute`.
