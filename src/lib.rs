//! Turn the current process into a Unix daemon.
//!
//! A drop-in replacement for [`daemonize`](https://crates.io/crates/daemonize) 0.5, which is
//! unmaintained ([RUSTSEC-2025-0069](https://rustsec.org/advisories/RUSTSEC-2025-0069.html)).
//! The library is named `daemonize`, so switching is one line in `Cargo.toml` and no code change:
//!
//! ```toml
//! daemonize = { package = "daemonize-next", version = "0.1" }
//! ```
//!
//! What it fixes over `daemonize` 0.5:
//!
//! - Dropping to `user` also drops root's supplementary groups (`setgroups`), and setting only
//!   `user` also switches to that user's primary group. 0.5 kept both, so a daemon "running as
//!   nobody" was still in group 0.
//! - A failed step makes the launcher's `start()` return the error, so it can exit non-zero.
//!   0.5's launcher exited 0 when the pid file was already locked, the working directory was
//!   missing, or any step after the second fork failed: it passed the raw `waitpid` status to
//!   `exit`, which keeps only the low 8 bits.
//! - `chown_pid_file` with only one of `user` and `group` leaves the other owner field
//!   unchanged. 0.5 set it to id 4294967294.
//! - Refuses to fork a multithreaded process unless you opt in with
//!   [`allow_threads`](Daemonize::allow_threads), since only the forking thread survives.
//! - No `transmute` in [`privileged_action`](Daemonize::privileged_action).
//!
//! ```no_run
//! use std::fs::File;
//!
//! use daemonize::Daemonize;
//!
//! let stdout = File::create("/tmp/daemon.out").unwrap();
//! let stderr = File::create("/tmp/daemon.err").unwrap();
//!
//! let daemonize = Daemonize::new()
//!     .pid_file("/tmp/test.pid") // Every method except `new` and `start`
//!     .chown_pid_file(true)      // is optional, see `Daemonize` documentation
//!     .working_directory("/tmp") // for default behaviour.
//!     .user("nobody")
//!     .group("daemon") // Group name
//!     .group(2)        // or group id.
//!     .umask(0o777)    // Set umask, `0o027` by default.
//!     .stdout(stdout)  // Redirect stdout to `/tmp/daemon.out`.
//!     .stderr(stderr)  // Redirect stderr to `/tmp/daemon.err`.
//!     .privileged_action(|| "Executed before drop privileges");
//!
//! match daemonize.start() {
//!     Ok(_) => println!("Success, daemonized"),
//!     Err(e) => eprintln!("Error, {}", e),
//! }
//! ```
//!
//! `start()` returns an error in the launching process, on its terminal, for any step that
//! fails, including the steps the daemon runs after detaching.

mod error;

use std::ffi::CString;
use std::fmt;
use std::fs::File;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::exit;

pub use self::error::Error;
use self::error::{errno, ErrorKind};

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
enum UserImpl {
    Name(String),
    Id(libc::uid_t),
}

/// A user to switch to, by name or by id.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct User {
    inner: UserImpl,
}

impl From<&str> for User {
    fn from(t: &str) -> User {
        User {
            inner: UserImpl::Name(t.to_owned()),
        }
    }
}

impl From<u32> for User {
    fn from(t: u32) -> User {
        User {
            inner: UserImpl::Id(t as libc::uid_t),
        }
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
enum GroupImpl {
    Name(String),
    Id(libc::gid_t),
}

/// A group to switch to, by name or by id.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct Group {
    inner: GroupImpl,
}

impl From<&str> for Group {
    fn from(t: &str) -> Group {
        Group {
            inner: GroupImpl::Name(t.to_owned()),
        }
    }
}

impl From<u32> for Group {
    fn from(t: u32) -> Group {
        Group {
            inner: GroupImpl::Id(t as libc::gid_t),
        }
    }
}

/// A umask value.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct Mask {
    inner: libc::mode_t,
}

impl From<u32> for Mask {
    fn from(inner: u32) -> Mask {
        Mask {
            inner: inner as libc::mode_t,
        }
    }
}

#[derive(Debug)]
enum StdioImpl {
    Devnull,
    RedirectToFile(File),
    Keep,
}

/// Where a standard stream goes: `/dev/null` (the default), a `File`, or left as it is.
#[derive(Debug)]
pub struct Stdio {
    inner: StdioImpl,
}

impl Stdio {
    pub fn devnull() -> Self {
        Self {
            inner: StdioImpl::Devnull,
        }
    }

    pub fn keep() -> Self {
        Self {
            inner: StdioImpl::Keep,
        }
    }
}

impl From<File> for Stdio {
    fn from(file: File) -> Self {
        Self {
            inner: StdioImpl::RedirectToFile(file),
        }
    }
}

/// Returned in the launching process by [`Daemonize::execute`], once the daemon has started.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub struct Parent {
    pub first_child_exit_code: i32,
}

/// Returned in the daemon by [`Daemonize::execute`].
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub struct Child<T> {
    pub privileged_action_result: T,
}

/// Which process [`Daemonize::execute`] returned in, and whether daemonizing worked.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
#[must_use = "the launching process and the daemon both return here; match on it"]
pub enum Outcome<T> {
    Parent(Result<Parent, Error>),
    Child(Result<Child<T>, Error>),
}

impl<T> Outcome<T> {
    pub fn is_parent(&self) -> bool {
        matches!(self, Outcome::Parent(_))
    }

    pub fn is_child(&self) -> bool {
        matches!(self, Outcome::Child(_))
    }
}

/// Daemonization options.
///
/// Defaults: working directory `/`, umask `0o027`, stdin, stdout and stderr to `/dev/null`,
/// no pid file, no user or group change, no chroot.
///
/// The daemon runs these steps in order: `chdir`, `setsid`, `umask`, second fork, open and
/// lock the pid file, redirect the standard streams, chown the pid file, run the privileged
/// action, `chroot`, drop supplementary groups, `setgid`, `setuid`, write the pid.
#[must_use = "nothing happens until you call .start() or .execute()"]
pub struct Daemonize<T> {
    directory: PathBuf,
    pid_file: Option<PathBuf>,
    chown_pid_file: bool,
    user: Option<User>,
    group: Option<Group>,
    umask: Mask,
    root: Option<PathBuf>,
    privileged_action: Box<dyn FnOnce() -> T>,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
    allow_threads: bool,
}

impl<T> fmt::Debug for Daemonize<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Daemonize")
            .field("directory", &self.directory)
            .field("pid_file", &self.pid_file)
            .field("chown_pid_file", &self.chown_pid_file)
            .field("user", &self.user)
            .field("group", &self.group)
            .field("umask", &self.umask)
            .field("root", &self.root)
            .field("stdin", &self.stdin)
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .field("allow_threads", &self.allow_threads)
            .finish()
    }
}

impl Default for Daemonize<()> {
    fn default() -> Self {
        Self::new()
    }
}

impl Daemonize<()> {
    pub fn new() -> Self {
        Daemonize {
            directory: Path::new("/").to_owned(),
            pid_file: None,
            chown_pid_file: false,
            user: None,
            group: None,
            umask: 0o027.into(),
            privileged_action: Box::new(|| ()),
            root: None,
            stdin: Stdio::devnull(),
            stdout: Stdio::devnull(),
            stderr: Stdio::devnull(),
            allow_threads: false,
        }
    }
}

impl<T> Daemonize<T> {
    /// Create a pid file at `path`, locked with `flock` so a second instance fails.
    pub fn pid_file<F: AsRef<Path>>(mut self, path: F) -> Self {
        self.pid_file = Some(path.as_ref().to_owned());
        self
    }

    /// Give the pid file to `user` and `group`, if set. Off by default.
    pub fn chown_pid_file(mut self, chown: bool) -> Self {
        self.chown_pid_file = chown;
        self
    }

    /// Change the working directory to `path`, `/` by default.
    pub fn working_directory<F: AsRef<Path>>(mut self, path: F) -> Self {
        self.directory = path.as_ref().to_owned();
        self
    }

    /// Switch to `user` after the privileged action. Needs root unless it is the current user.
    ///
    /// When running as root, this also replaces the supplementary groups with the user's, and
    /// if no `group` is set, switches to the user's primary group.
    pub fn user<U: Into<User>>(mut self, user: U) -> Self {
        self.user = Some(user.into());
        self
    }

    /// Switch to `group` after the privileged action.
    pub fn group<G: Into<Group>>(mut self, group: G) -> Self {
        self.group = Some(group.into());
        self
    }

    /// Set the umask, `0o027` by default.
    pub fn umask<M: Into<Mask>>(mut self, mask: M) -> Self {
        self.umask = mask.into();
        self
    }

    /// `chroot` into `path` after the privileged action.
    pub fn chroot<F: AsRef<Path>>(mut self, path: F) -> Self {
        self.root = Some(path.as_ref().to_owned());
        self
    }

    /// Run `action` in the daemon before dropping privileges, for example to bind a low port.
    /// Its result comes back from [`start`](Self::start).
    pub fn privileged_action<N, F: FnOnce() -> N + 'static>(self, action: F) -> Daemonize<N> {
        Daemonize {
            directory: self.directory,
            pid_file: self.pid_file,
            chown_pid_file: self.chown_pid_file,
            user: self.user,
            group: self.group,
            umask: self.umask,
            root: self.root,
            privileged_action: Box::new(action),
            stdin: self.stdin,
            stdout: self.stdout,
            stderr: self.stderr,
            allow_threads: self.allow_threads,
        }
    }

    /// Redirect stdout, to `/dev/null` by default.
    pub fn stdout<S: Into<Stdio>>(mut self, stdio: S) -> Self {
        self.stdout = stdio.into();
        self
    }

    /// Redirect stderr, to `/dev/null` by default.
    pub fn stderr<S: Into<Stdio>>(mut self, stdio: S) -> Self {
        self.stderr = stdio.into();
        self
    }

    /// Fork even when other threads are running. Off by default.
    ///
    /// Only the calling thread exists in the daemon. A lock another thread held at the fork,
    /// including the allocator's, stays locked forever, so the daemon can hang on its next
    /// allocation. Daemonize before starting threads instead when you can.
    pub fn allow_threads(mut self, allow: bool) -> Self {
        self.allow_threads = allow;
        self
    }

    /// Daemonize. The launching process exits with status 0 once the daemon has started, or
    /// gets the error if any step failed. The daemon gets the privileged action's result.
    pub fn start(self) -> Result<T, Error> {
        match self.run(true) {
            Outcome::Parent(Ok(Parent {
                first_child_exit_code,
            })) => exit(first_child_exit_code),
            Outcome::Parent(Err(err)) => Err(err),
            Outcome::Child(Ok(child)) => Ok(child.privileged_action_result),
            Outcome::Child(Err(err)) => Err(err),
        }
    }

    /// Daemonize without exiting the launching process. It returns
    /// [`Outcome::Parent`] once the daemon has started or failed; the daemon returns
    /// [`Outcome::Child`].
    pub fn execute(self) -> Outcome<T> {
        self.run(false)
    }

    fn run(self, exit_on_child_error: bool) -> Outcome<T> {
        let plan = match Plan::new(&self) {
            Ok(plan) => plan,
            Err(err) => return Outcome::Parent(Err(err)),
        };
        let (read_fd, write_fd) = match status_pipe() {
            Ok(fds) => fds,
            Err(kind) => return Outcome::Parent(Err(kind.into())),
        };
        match unsafe { libc::fork() } {
            -1 => {
                let kind = ErrorKind::Fork(errno());
                unsafe {
                    libc::close(read_fd);
                    libc::close(write_fd);
                }
                Outcome::Parent(Err(kind.into()))
            }
            0 => {
                unsafe { libc::close(read_fd) };
                let result = self.run_child(&plan);
                let status = match &result {
                    Ok(_) => [0u8; 8],
                    Err(kind) => kind.to_wire(),
                };
                write_status(write_fd, status);
                unsafe { libc::close(write_fd) };
                match result {
                    Ok(value) => Outcome::Child(Ok(Child {
                        privileged_action_result: value,
                    })),
                    Err(_) if exit_on_child_error => unsafe { libc::_exit(1) },
                    Err(kind) => Outcome::Child(Err(plan.error(kind))),
                }
            }
            pid => {
                unsafe { libc::close(write_fd) };
                let status = read_status(read_fd);
                unsafe { libc::close(read_fd) };
                let exit_code = wait_exit_code(pid);
                Outcome::Parent(match (status, exit_code) {
                    (Err(kind), _) | (Ok(_), Err(kind)) => Err(plan.error(kind)),
                    (Ok(()), Ok(first_child_exit_code)) => Ok(Parent {
                        first_child_exit_code,
                    }),
                })
            }
        }
    }

    fn run_child(self, plan: &Plan) -> Result<T, ErrorKind> {
        unsafe {
            check(
                libc::chdir(plan.directory.as_ptr()),
                ErrorKind::ChangeDirectory,
            )?;
            check(libc::setsid(), ErrorKind::DetachSession)?;
            libc::umask(self.umask.inner);

            match check(libc::fork(), ErrorKind::Fork)? {
                0 => {}
                _ => libc::_exit(0),
            }

            let pid_fd = match &plan.pid_file {
                Some(path) => Some(open_pid_file(path)?),
                None => None,
            };

            redirect(libc::STDIN_FILENO, &self.stdin)?;
            redirect(libc::STDOUT_FILENO, &self.stdout)?;
            redirect(libc::STDERR_FILENO, &self.stderr)?;

            if let (true, Some(fd)) = (self.chown_pid_file, pid_fd) {
                if plan.uid.is_some() || plan.gid.is_some() {
                    // -1 leaves that owner field unchanged.
                    let uid = plan.uid.unwrap_or(libc::uid_t::MAX);
                    let gid = plan.gid.unwrap_or(libc::gid_t::MAX);
                    check(libc::fchown(fd, uid, gid), ErrorKind::ChownPidfile)?;
                }
            }

            let result = (self.privileged_action)();

            if let Some(root) = &plan.root {
                check(libc::chroot(root.as_ptr()), ErrorKind::Chroot)?;
            }
            if let Some(groups) = &plan.groups {
                check(
                    libc::setgroups(groups.len() as _, groups.as_ptr()),
                    ErrorKind::SetGroups,
                )?;
            }
            if let Some(gid) = plan.gid {
                check(libc::setgid(gid), ErrorKind::SetGroup)?;
            }
            if let Some(uid) = plan.uid {
                check(libc::setuid(uid), ErrorKind::SetUser)?;
            }
            if let Some(fd) = pid_fd {
                write_pid(fd)?;
            }
            Ok(result)
        }
    }
}

/// Everything the daemon needs, resolved before forking so lookup errors reach the caller
/// and the daemon does no name service lookups.
struct Plan {
    directory: CString,
    directory_path: PathBuf,
    pid_file: Option<CString>,
    pid_file_path: Option<PathBuf>,
    root: Option<CString>,
    root_path: Option<PathBuf>,
    uid: Option<libc::uid_t>,
    gid: Option<libc::gid_t>,
    groups: Option<Vec<libc::gid_t>>,
}

impl Plan {
    fn new<T>(d: &Daemonize<T>) -> Result<Plan, Error> {
        if !d.allow_threads {
            if let Some(n) = thread_count() {
                if n > 1 {
                    return Err(ErrorKind::Threads(n).into());
                }
            }
        }
        let path_error = |p: &Path| Error::with_context(ErrorKind::PathContainsNul, Some(show(p)));
        let cstr =
            |p: &Path| CString::new(p.as_os_str().to_owned().into_vec()).map_err(|_| path_error(p));

        let user = match &d.user {
            None => None,
            Some(User {
                inner: UserImpl::Id(uid),
            }) => Some(lookup_uid(*uid)),
            Some(User {
                inner: UserImpl::Name(name),
            }) => Some(lookup_user(name)?),
        };
        let gid = match &d.group {
            None => None,
            Some(Group {
                inner: GroupImpl::Id(gid),
            }) => Some(*gid),
            Some(Group {
                inner: GroupImpl::Name(name),
            }) => Some(lookup_group(name)?),
        };
        let root = unsafe { libc::geteuid() } == 0;
        // As root, a user without a group gets the user's primary group, not root's.
        let gid = gid.or(if root {
            user.as_ref().and_then(|u| u.gid)
        } else {
            None
        });
        let groups = match (root, &user, gid) {
            (
                true,
                Some(UserEntry {
                    name: Some(name), ..
                }),
                Some(gid),
            ) => Some(group_list(name, gid)?),
            (true, _, Some(gid)) => Some(vec![gid]),
            _ => None,
        };

        Ok(Plan {
            directory: cstr(&d.directory)?,
            directory_path: d.directory.clone(),
            pid_file: d.pid_file.as_deref().map(cstr).transpose()?,
            pid_file_path: d.pid_file.clone(),
            root: d.root.as_deref().map(cstr).transpose()?,
            root_path: d.root.clone(),
            uid: user.map(|u| u.uid),
            gid,
            groups,
        })
    }

    /// Attach the path a step worked on, so the message says which file.
    fn error(&self, kind: ErrorKind) -> Error {
        let path = match kind {
            ErrorKind::ChangeDirectory(_) => Some(&self.directory_path),
            ErrorKind::OpenPidfile(_)
            | ErrorKind::LockPidfile(_)
            | ErrorKind::ChownPidfile(_)
            | ErrorKind::TruncatePidfile(_)
            | ErrorKind::WritePid(_)
            | ErrorKind::WritePidUnspecifiedError => self.pid_file_path.as_ref(),
            ErrorKind::Chroot(_) => self.root_path.as_ref(),
            _ => None,
        };
        Error::with_context(kind, path.map(|p| show(p)))
    }
}

fn show(p: &Path) -> String {
    format!("{}", p.display())
}

struct UserEntry {
    uid: libc::uid_t,
    gid: Option<libc::gid_t>,
    name: Option<CString>,
}

fn lookup_user(name: &str) -> Result<UserEntry, Error> {
    let c = CString::new(name).map_err(|_| ErrorKind::UserContainsNul)?;
    let (uid, gid, _) =
        passwd(|pwd, buf, len, out| unsafe { libc::getpwnam_r(c.as_ptr(), pwd, buf, len, out) })
            .ok_or_else(|| {
                Error::with_context(ErrorKind::UserNotFound, Some(format!("{name:?}")))
            })?;
    Ok(UserEntry {
        uid,
        gid: Some(gid),
        name: Some(c),
    })
}

/// A numeric user may have no passwd entry; then only the id is known.
fn lookup_uid(uid: libc::uid_t) -> UserEntry {
    match passwd(|pwd, buf, len, out| unsafe { libc::getpwuid_r(uid, pwd, buf, len, out) }) {
        Some((_, gid, name)) => UserEntry {
            uid,
            gid: Some(gid),
            name: Some(name),
        },
        None => UserEntry {
            uid,
            gid: None,
            name: None,
        },
    }
}

/// Calls a `getpw*_r` function, growing the buffer on `ERANGE`.
fn passwd(
    mut f: impl FnMut(
        *mut libc::passwd,
        *mut libc::c_char,
        usize,
        *mut *mut libc::passwd,
    ) -> libc::c_int,
) -> Option<(libc::uid_t, libc::gid_t, CString)> {
    let mut buf = vec![0u8; 4096];
    loop {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        let rc = f(&mut pwd, buf.as_mut_ptr().cast(), buf.len(), &mut out);
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || out.is_null() {
            return None;
        }
        let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }.to_owned();
        return Some((pwd.pw_uid, pwd.pw_gid, name));
    }
}

fn lookup_group(name: &str) -> Result<libc::gid_t, Error> {
    let c = CString::new(name).map_err(|_| ErrorKind::GroupContainsNul)?;
    let mut buf = vec![0u8; 4096];
    loop {
        let mut grp: libc::group = unsafe { std::mem::zeroed() };
        let mut out: *mut libc::group = std::ptr::null_mut();
        let rc = unsafe {
            libc::getgrnam_r(
                c.as_ptr(),
                &mut grp,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut out,
            )
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || out.is_null() {
            return Err(Error::with_context(
                ErrorKind::GroupNotFound,
                Some(format!("{name:?}")),
            ));
        }
        return Ok(grp.gr_gid);
    }
}

#[cfg(not(target_vendor = "apple"))]
type GroupListItem = libc::gid_t;
#[cfg(target_vendor = "apple")]
type GroupListItem = libc::c_int;

/// The user's groups from the group database, with `gid` first.
fn group_list(name: &CString, gid: libc::gid_t) -> Result<Vec<libc::gid_t>, Error> {
    let mut n: libc::c_int = 64;
    loop {
        let mut list: Vec<GroupListItem> = vec![0; n as usize];
        let mut got = n;
        let rc =
            unsafe { libc::getgrouplist(name.as_ptr(), gid as _, list.as_mut_ptr(), &mut got) };
        if rc >= 0 {
            list.truncate(got.max(0) as usize);
            let mut groups: Vec<libc::gid_t> = vec![gid];
            groups.extend(
                list.into_iter()
                    .map(|g| g as libc::gid_t)
                    .filter(|&g| g != gid),
            );
            return Ok(groups);
        }
        if n >= 1 << 16 {
            return Err(ErrorKind::GetGroupList.into());
        }
        n = got.max(n * 2);
    }
}

#[cfg(target_os = "linux")]
fn thread_count() -> Option<usize> {
    std::fs::read_dir("/proc/self/task").ok().map(|d| d.count())
}

#[cfg(not(target_os = "linux"))]
fn thread_count() -> Option<usize> {
    None
}

fn check<N: Into<i64> + Copy>(ret: N, kind: fn(libc::c_int) -> ErrorKind) -> Result<N, ErrorKind> {
    if ret.into() == -1 {
        Err(kind(errno()))
    } else {
        Ok(ret)
    }
}

/// A close-on-exec pipe whose ends are above the standard streams, so redirecting stdio
/// in the daemon cannot overwrite them.
fn status_pipe() -> Result<(RawFd, RawFd), ErrorKind> {
    let mut fds = [0 as RawFd; 2];
    check(unsafe { libc::pipe(fds.as_mut_ptr()) }, ErrorKind::Pipe)?;
    for fd in &mut fds {
        let moved = unsafe { libc::fcntl(*fd, libc::F_DUPFD_CLOEXEC, 3) };
        let err = errno();
        unsafe { libc::close(*fd) };
        if moved == -1 {
            return Err(ErrorKind::Pipe(err));
        }
        *fd = moved;
    }
    Ok((fds[0], fds[1]))
}

fn write_status(fd: RawFd, status: [u8; 8]) {
    loop {
        let n = unsafe { libc::write(fd, status.as_ptr().cast(), status.len()) };
        if n == -1 && errno() == libc::EINTR {
            continue;
        }
        return;
    }
}

/// `Ok` when the daemon reported success, the daemon's error, or `DaemonExited` when the
/// pipe closed with no report.
fn read_status(fd: RawFd) -> Result<(), ErrorKind> {
    let mut buf = [0u8; 8];
    let mut got = 0;
    while got < buf.len() {
        let n = unsafe { libc::read(fd, buf[got..].as_mut_ptr().cast(), buf.len() - got) };
        match n {
            -1 if errno() == libc::EINTR => continue,
            -1 => return Err(ErrorKind::ReadStatus(errno())),
            0 => return Err(ErrorKind::DaemonExited),
            n => got += n as usize,
        }
    }
    if buf == [0u8; 8] {
        Ok(())
    } else {
        Err(ErrorKind::from_wire(buf))
    }
}

fn wait_exit_code(pid: libc::pid_t) -> Result<i32, ErrorKind> {
    let mut status = 0;
    loop {
        if unsafe { libc::waitpid(pid, &mut status, 0) } == -1 {
            let e = errno();
            if e == libc::EINTR {
                continue;
            }
            return Err(ErrorKind::Wait(e));
        }
        return Ok(if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            1
        });
    }
}

unsafe fn open_pid_file(path: &CString) -> Result<RawFd, ErrorKind> {
    let fd = check(
        libc::open(
            path.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC,
            0o666,
        ),
        ErrorKind::OpenPidfile,
    )?;
    check(
        libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB),
        ErrorKind::LockPidfile,
    )?;
    Ok(fd)
}

unsafe fn redirect(target: RawFd, stdio: &Stdio) -> Result<(), ErrorKind> {
    match &stdio.inner {
        StdioImpl::Keep => {}
        StdioImpl::RedirectToFile(file) => {
            check(
                libc::dup2(file.as_raw_fd(), target),
                ErrorKind::RedirectStreams,
            )?;
        }
        StdioImpl::Devnull => {
            let null = check(
                libc::open(c"/dev/null".as_ptr(), libc::O_RDWR),
                ErrorKind::OpenDevnull,
            )?;
            let r = check(libc::dup2(null, target), ErrorKind::RedirectStreams);
            if null != target {
                libc::close(null);
            }
            r?;
        }
    }
    Ok(())
}

/// Writes the pid with a stack buffer.
unsafe fn write_pid(fd: RawFd) -> Result<(), ErrorKind> {
    let mut pid = libc::getpid() as u64;
    let mut buf = [0u8; 24];
    let mut i = buf.len() - 1;
    buf[i] = b'\n';
    loop {
        i -= 1;
        buf[i] = b'0' + (pid % 10) as u8;
        pid /= 10;
        if pid == 0 {
            break;
        }
    }
    let digits = &buf[i..];
    check(libc::ftruncate(fd, 0), ErrorKind::TruncatePidfile)?;
    let written = check(
        libc::write(fd, digits.as_ptr().cast(), digits.len()) as i64,
        ErrorKind::WritePid,
    )?;
    if written < digits.len() as i64 {
        return Err(ErrorKind::WritePidUnspecifiedError);
    }
    Ok(())
}
