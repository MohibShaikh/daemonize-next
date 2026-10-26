use std::fmt;
use std::io;

pub type Errno = libc::c_int;

/// Why daemonizing failed.
///
/// `Display` names the step that failed, the path or name involved, the OS
/// error, and for common causes, what to change.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub struct Error {
    kind: ErrorKind,
    context: Option<String>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub(crate) enum ErrorKind {
    Fork(Errno),
    Wait(Errno),
    DetachSession(Errno),
    GroupNotFound,
    GroupContainsNul,
    SetGroup(Errno),
    SetGroups(Errno),
    GetGroupList,
    UserNotFound,
    UserContainsNul,
    SetUser(Errno),
    ChangeDirectory(Errno),
    PathContainsNul,
    OpenPidfile(Errno),
    LockPidfile(Errno),
    ChownPidfile(Errno),
    OpenDevnull(Errno),
    RedirectStreams(Errno),
    TruncatePidfile(Errno),
    WritePid(Errno),
    WritePidUnspecifiedError,
    Chroot(Errno),
    Pipe(Errno),
    ReadStatus(Errno),
    DaemonExited,
    Threads(usize),
}

impl ErrorKind {
    fn description(&self) -> &'static str {
        match self {
            ErrorKind::Fork(_) => "unable to fork",
            ErrorKind::Wait(_) => "wait failed",
            ErrorKind::DetachSession(_) => "unable to create new session",
            ErrorKind::GroupNotFound => "unable to resolve group name to group id",
            ErrorKind::GroupContainsNul => "group option contains NUL",
            ErrorKind::SetGroup(_) => "unable to set group",
            ErrorKind::SetGroups(_) => "unable to set supplementary groups",
            ErrorKind::GetGroupList => "unable to list the user's supplementary groups",
            ErrorKind::UserNotFound => "unable to resolve user name to user id",
            ErrorKind::UserContainsNul => "user option contains NUL",
            ErrorKind::SetUser(_) => "unable to set user",
            ErrorKind::ChangeDirectory(_) => "unable to change directory",
            ErrorKind::PathContainsNul => "path option contains NUL",
            ErrorKind::OpenPidfile(_) => "unable to open pid file",
            ErrorKind::LockPidfile(_) => "unable to lock pid file",
            ErrorKind::ChownPidfile(_) => "unable to chown pid file",
            ErrorKind::OpenDevnull(_) => "unable to open /dev/null",
            ErrorKind::RedirectStreams(_) => "unable to redirect standard streams",
            ErrorKind::TruncatePidfile(_) => "unable to truncate pid file",
            ErrorKind::WritePid(_) => "unable to write self pid to pid file",
            ErrorKind::WritePidUnspecifiedError => "short write of self pid to pid file",
            ErrorKind::Chroot(_) => "unable to chroot into directory",
            ErrorKind::Pipe(_) => "unable to create the status pipe",
            ErrorKind::ReadStatus(_) => "unable to read the daemon's status",
            ErrorKind::DaemonExited => "the daemon exited before it finished starting",
            ErrorKind::Threads(_) => "refusing to fork a multithreaded process",
        }
    }

    pub(crate) fn errno(&self) -> Option<Errno> {
        match *self {
            ErrorKind::Fork(e)
            | ErrorKind::Wait(e)
            | ErrorKind::DetachSession(e)
            | ErrorKind::SetGroup(e)
            | ErrorKind::SetGroups(e)
            | ErrorKind::SetUser(e)
            | ErrorKind::ChangeDirectory(e)
            | ErrorKind::OpenPidfile(e)
            | ErrorKind::LockPidfile(e)
            | ErrorKind::ChownPidfile(e)
            | ErrorKind::OpenDevnull(e)
            | ErrorKind::RedirectStreams(e)
            | ErrorKind::TruncatePidfile(e)
            | ErrorKind::WritePid(e)
            | ErrorKind::Chroot(e)
            | ErrorKind::Pipe(e)
            | ErrorKind::ReadStatus(e) => Some(e),
            _ => None,
        }
    }

    fn hint(&self) -> Option<&'static str> {
        match *self {
            ErrorKind::LockPidfile(e) if e == libc::EWOULDBLOCK => {
                Some("another process holds the lock; is the daemon already running?")
            }
            ErrorKind::SetUser(libc::EPERM)
            | ErrorKind::SetGroup(libc::EPERM)
            | ErrorKind::SetGroups(libc::EPERM)
            | ErrorKind::ChownPidfile(libc::EPERM)
            | ErrorKind::Chroot(libc::EPERM) => {
                Some("this step needs root; start the process as root")
            }
            ErrorKind::OpenPidfile(libc::ENOENT) => Some("the pid file's directory does not exist"),
            ErrorKind::OpenPidfile(libc::EACCES) => {
                Some("the pid file's directory is not writable by this user")
            }
            ErrorKind::Threads(_) => Some(
                "call start() or execute() before spawning threads (for tokio, before building \
                 the runtime), or opt out with .allow_threads(true)",
            ),
            ErrorKind::DaemonExited => {
                Some("the privileged action panicked or the process was killed during startup")
            }
            _ => None,
        }
    }

    /// Steps that run after fork report through the status pipe as a tag and an errno.
    pub(crate) fn to_wire(self) -> [u8; 8] {
        let (tag, errno): (u32, Errno) = match self {
            ErrorKind::Fork(e) => (1, e),
            ErrorKind::DetachSession(e) => (2, e),
            ErrorKind::SetGroup(e) => (3, e),
            ErrorKind::SetGroups(e) => (4, e),
            ErrorKind::SetUser(e) => (5, e),
            ErrorKind::ChangeDirectory(e) => (6, e),
            ErrorKind::OpenPidfile(e) => (7, e),
            ErrorKind::LockPidfile(e) => (8, e),
            ErrorKind::ChownPidfile(e) => (9, e),
            ErrorKind::OpenDevnull(e) => (10, e),
            ErrorKind::RedirectStreams(e) => (11, e),
            ErrorKind::TruncatePidfile(e) => (12, e),
            ErrorKind::WritePid(e) => (13, e),
            ErrorKind::WritePidUnspecifiedError => (14, 0),
            ErrorKind::Chroot(e) => (15, e),
            _ => (u32::MAX, 0),
        };
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&tag.to_ne_bytes());
        out[4..].copy_from_slice(&errno.to_ne_bytes());
        out
    }

    pub(crate) fn from_wire(buf: [u8; 8]) -> ErrorKind {
        let tag = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let e = Errno::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]);
        match tag {
            1 => ErrorKind::Fork(e),
            2 => ErrorKind::DetachSession(e),
            3 => ErrorKind::SetGroup(e),
            4 => ErrorKind::SetGroups(e),
            5 => ErrorKind::SetUser(e),
            6 => ErrorKind::ChangeDirectory(e),
            7 => ErrorKind::OpenPidfile(e),
            8 => ErrorKind::LockPidfile(e),
            9 => ErrorKind::ChownPidfile(e),
            10 => ErrorKind::OpenDevnull(e),
            11 => ErrorKind::RedirectStreams(e),
            12 => ErrorKind::TruncatePidfile(e),
            13 => ErrorKind::WritePid(e),
            14 => ErrorKind::WritePidUnspecifiedError,
            15 => ErrorKind::Chroot(e),
            _ => ErrorKind::DaemonExited,
        }
    }
}

impl Error {
    pub(crate) fn with_context(kind: ErrorKind, context: Option<String>) -> Self {
        Error { kind, context }
    }

    /// The OS error code behind this error, if there is one.
    pub fn raw_os_error(&self) -> Option<i32> {
        self.kind.errno()
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Error {
            kind,
            context: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.description())?;
        if let Some(context) = &self.context {
            write!(f, " {context}")?;
        }
        if let ErrorKind::Threads(n) = self.kind {
            write!(f, ": {n} threads are running")?;
        }
        if let Some(errno) = self.kind.errno() {
            write!(f, ": {}", io::Error::from_raw_os_error(errno))?;
        }
        if let Some(hint) = self.kind.hint() {
            write!(f, " ({hint})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

pub(crate) fn errno() -> Errno {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
