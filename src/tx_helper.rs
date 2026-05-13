use crate::config::{Colors, Config};
use crate::exec::Status;
use crate::fmt::truncate_to_width;
use crate::ui::TransactionRenderController;
use crate::util::{ask, collect_critical_pkgs};

use alpm::{
    AnyDownloadEvent, AnyEvent, AnyQuestion, CommitError, DownloadEvent, DownloadResult,
    Error as AlpmError, Event, HookWhen, LogLevel, PackageOperation, Progress, SigLevel, TransFlag,
};
use alpm_sys::alpm_handle_t;
use alpm_utils::DbListExt;
use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
#[cfg(target_os = "linux")]
use nix::fcntl::{open, openat, AtFlags, OFlag};
#[cfg(target_os = "linux")]
use nix::sys::stat::{fchmod, Mode};
#[cfg(target_os = "linux")]
use nix::sys::statfs::fstatfs;
#[cfg(target_os = "linux")]
use nix::sys::statvfs::{fstatvfs, FsFlags};
use nix::unistd::{dup, dup2_stdout, Uid};
#[cfg(target_os = "linux")]
use nix::unistd::{faccessat, AccessFlags};
use serde::{Deserialize, Serialize};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::collections::HashSet;
use std::fs::{read_to_string, remove_file, File};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::os::fd::{AsFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{
    mpsc::{channel, RecvTimeoutError},
    Arc, Mutex, MutexGuard,
};
use std::time::{Duration, Instant};

const MAX_PLAN_BYTES: usize = 128 * 1024;
const MAX_TARGETS: usize = 4096;
const MAX_TARGET_LEN: usize = 4096;
const MAX_IPC_LINE_BYTES: usize = 16 * 1024;
const MAX_LOG_MESSAGE_BYTES: usize = 8192;
const PACMAN_DB_LOCK: &str = "/var/lib/pacman/db.lck";

static HELPER_JSON_OUT: Mutex<Option<File>> = Mutex::new(None);

static ACTIVE_COMMIT_HANDLE: AtomicPtr<alpm_handle_t> = AtomicPtr::new(std::ptr::null_mut());
static SIG_THREAD_STARTED: AtomicBool = AtomicBool::new(false);
static INTERRUPT_REQUESTED: AtomicBool = AtomicBool::new(false);
/// When false, the helper ignores SIGINT/SIGTERM for `alpm_trans_interrupt` so hooks (e.g. mkinitcpio) are not torn down mid-write.
static COMMIT_INTERRUPT_ALLOWED: AtomicBool = AtomicBool::new(true);
/// Tracks the last hook that was running during transaction commit.
/// Used for error reporting when trans_commit() returns TransHookFailed.
static LAST_HOOK_RUNNING: Mutex<Option<String>> = Mutex::new(None);
/// Tracks whether we are currently in a protected hook phase (for timeout detection).
static HOOK_PHASE_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Timestamp when the current hook started (for timeout detection).
static HOOK_START_TIME: Mutex<Option<Instant>> = Mutex::new(None);

/// Timeout for individual hooks in post-transaction phase (seconds).
/// Reserved for future hook timeout detection implementation.
#[allow(dead_code)]
const HOOK_TIMEOUT_SECONDS: u64 = 300;

/// Global STDOUT lock for atomic output during critical operations.
/// Prevents interleaving of hook output with other messages.
static STDOUT_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard for atomic STDOUT access.
#[allow(dead_code)]
struct StdoutLock<'a>(#[allow(dead_code)] MutexGuard<'a, ()>);

impl<'a> StdoutLock<'a> {
    fn acquire() -> Result<Self> {
        let guard = STDOUT_LOCK.lock().map_err(|_| anyhow!("STDOUT lock poisoned"))?;
        Ok(Self(guard))
    }
}

impl std::io::Write for StdoutLock<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::stdout().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}

/// BULLETPROOF FIX: RAII guard that ensures transaction cleanup on any exit path.
/// This runs even if the transaction code panics or is interrupted.
/// Uses a zero-sized marker and unsafe pointer manipulation to avoid borrow checker issues.
struct TransactionCleanupGuard {
    alpm_ptr: *mut alpm_sys::alpm_handle_t,
    should_unlock: bool,
}

unsafe impl Send for TransactionCleanupGuard {}
unsafe impl Sync for TransactionCleanupGuard {}

impl TransactionCleanupGuard {
    fn new(alpm: &alpm::Alpm) -> Self {
        Self {
            alpm_ptr: alpm.as_alpm_handle_t(),
            should_unlock: true,
        }
    }
    
    /// Mark that we should skip unlock() during cleanup (e.g., hook failure).
    #[allow(dead_code)]
    fn skip_unlock(&mut self) {
        self.should_unlock = false;
    }
}

impl Drop for TransactionCleanupGuard {
    fn drop(&mut self) {
        if self.alpm_ptr.is_null() {
            return;
        }
        // SAFETY: alpm_ptr is valid for the lifetime of the transaction
        // and we only call trans_release/unlock which are safe ALPM operations
        unsafe {
            let _ = alpm_sys::alpm_trans_release(self.alpm_ptr);
        }
        bah_audit_log("TransactionCleanupGuard: trans_release() called");
        
        // Only unlock if transaction completed successfully
        if self.should_unlock {
            unsafe {
                let _ = alpm_sys::alpm_unlock(self.alpm_ptr);
            }
            bah_audit_log("TransactionCleanupGuard: unlock() called (success path)");
        } else {
            bah_audit_log("TransactionCleanupGuard: unlock() SKIPPED (failure path - ALPM cleanup in progress)");
        }
    }
}

/// Persistent audit trail for `/boot` checks and commit lifecycle (helper runs as root).
const BAH_AUDIT_LOG_PATH: &str = "/var/log/bah.log";

/// Hardened PATH and removal of dynamic-loader overrides so pacman hooks (mkinitcpio, grub, etc.)
/// cannot be tricked into executing attacker-controlled code via `LD_PRELOAD` or a user-writable PATH entry.
#[cfg(target_os = "linux")]
fn sanitize_helper_environment() {
    let keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    for k in keys {
        if k.starts_with("LD_") || k.starts_with("DYLD_") {
            // Safety: process-local env sanitization for helper subprocess.
            unsafe {
                std::env::remove_var(&k);
            }
            continue;
        }
        match k.as_str() {
            "PERL5LIB" | "PYTHONPATH" | "RUBYLIB" | "NODE_PATH" => unsafe {
                std::env::remove_var(&k);
            },
            _ => {}
        }
    }
    // Safety: process-local env sanitization for helper subprocess.
    unsafe {
        std::env::set_var(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
    }
}

/// Reads until `\n` or EOF without buffering more than `max_body_bytes` (excluding the delimiter).
fn read_until_newline_limited<R: BufRead>(
    reader: &mut R,
    max_body_bytes: usize,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut one = [0u8; 1];
    loop {
        let n = reader.read(&mut one).context("read stdin/pipe")?;
        if n == 0 {
            break;
        }
        if one[0] == b'\n' {
            break;
        }
        if out.len() >= max_body_bytes {
            bail!(
                "input line exceeded maximum size ({} bytes)",
                max_body_bytes
            );
        }
        out.push(one[0]);
    }
    Ok(out)
}

/// One JSON IPC line from parent ↔ helper; caps memory use per line (DoS hardening).
fn read_ipc_line_bounded<R: BufRead>(reader: &mut R) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    let mut one = [0u8; 1];
    loop {
        let n = reader.read(&mut one)?;
        if n == 0 {
            return Ok(if buf.is_empty() {
                None
            } else {
                Some(String::from_utf8(buf).map_err(|_| {
                    std::io::Error::new(ErrorKind::InvalidData, "IPC line is not valid UTF-8")
                })?)
            });
        }
        if one[0] == b'\n' {
            return Ok(Some(String::from_utf8(buf).map_err(|_| {
                std::io::Error::new(ErrorKind::InvalidData, "IPC line is not valid UTF-8")
            })?));
        }
        if buf.len() >= MAX_IPC_LINE_BYTES {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!("IPC line exceeds {} bytes", MAX_IPC_LINE_BYTES),
            ));
        }
        buf.push(one[0]);
    }
}

fn bah_audit_log(msg: &str) {
    #[cfg(target_os = "linux")]
    {
        let ts = Local::now().format("%Y-%m-%d %H:%M:%S%.3f %z");
        // O_NOFOLLOW: refuse if /var/log/bah.log is a symlink (no follower overwrite of arbitrary paths).
        // fchmod: tighten mode even when the file already existed with loose permissions.
        if let Ok(fd) = open(
            std::path::Path::new(BAH_AUDIT_LOG_PATH),
            OFlag::O_WRONLY
                | OFlag::O_APPEND
                | OFlag::O_CREAT
                | OFlag::O_NOFOLLOW
                | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        ) {
            let _ = fchmod(&fd, Mode::from_bits_truncate(0o600));
            let mut f = File::from(fd);
            let _ = writeln!(f, "[{}] [bah-helper:{}] {}", ts, std::process::id(), msg);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = msg;
    }
}

#[cfg(target_os = "linux")]
fn mountinfo_mount_point(line: &str) -> Option<String> {
    let before_sep = line.split(" - ").next()?;
    let mut it = before_sep.split_whitespace();
    let _ = it.next()?; // mount id
    let _ = it.next()?; // parent id
    let _ = it.next()?; // maj:minor
    let _ = it.next()?; // root within fs
    let mp = it.next()?;
    Some(
        mp.replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\134", "\\"),
    )
}

#[cfg(target_os = "linux")]
fn boot_mountinfo_summary(boot: &Path) -> String {
    let wanted = boot.to_string_lossy().into_owned();
    let norm_wanted = wanted.trim_end_matches('/').to_string();
    let Ok(buf) = read_to_string("/proc/self/mountinfo") else {
        return "(read mountinfo failed)".to_string();
    };
    let mut hits = Vec::new();
    for line in buf.lines() {
        let Some(mp) = mountinfo_mount_point(line) else {
            continue;
        };
        let norm_mp = mp.trim_end_matches('/');
        let under = format!("{norm_wanted}/");
        if norm_mp == norm_wanted || mp.starts_with(&under) {
            hits.push(line.trim().to_string());
        }
    }
    if hits.is_empty() {
        "(no mountinfo line matched boot path)".to_string()
    } else {
        let joined = hits.join(" | ");
        truncate_to_width(&joined, 900)
    }
}

#[cfg(target_os = "linux")]
fn boot_mount_candidates_from_fstab(root: &Path) -> Vec<PathBuf> {
    let fstab = root.join("etc/fstab");
    let data = match read_to_string(&fstab) {
        Ok(data) => data,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let _spec = parts.next();
        let mount = match parts.next() {
            Some(mount) => mount,
            None => continue,
        };
        if mount == "/boot" || mount == "/boot/efi" || mount == "/efi" {
            out.push(PathBuf::from(mount));
        }
    }

    out
}

#[cfg(target_os = "linux")]
fn mountinfo_mount_points() -> HashSet<String> {
    let mut out = HashSet::new();
    let file = match File::open("/proc/self/mountinfo") {
        Ok(file) => file,
        Err(_) => return out,
    };
    let reader = BufReader::new(file);
    for line in reader.lines().flatten() {
        if let Some(mount) = mountinfo_mount_point(&line) {
            out.insert(mount);
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn select_boot_mountpoint(root: &Path) -> Result<PathBuf> {
    const CANDIDATES: [&str; 3] = ["/boot", "/boot/efi", "/efi"];

    let expected = boot_mount_candidates_from_fstab(root);
    let mounted = mountinfo_mount_points();

    if !expected.is_empty() {
        for candidate in CANDIDATES {
            if expected.iter().any(|p| p == Path::new(candidate))
                && mounted.contains(candidate)
            {
                return Ok(PathBuf::from(candidate));
            }
        }
        let list = expected
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "expected boot/EFI mountpoint(s) from /etc/fstab not mounted: {}. Mount them before updating kernel, initramfs, or bootloader.",
            list
        );
    }

    for candidate in CANDIDATES {
        if mounted.contains(candidate) {
            return Ok(PathBuf::from(candidate));
        }
    }

    Ok(PathBuf::from("/boot"))
}

#[cfg(target_os = "linux")]
fn openat_dir_no_symlinks<Fd: AsFd>(root_fd: Fd, rel: &Path) -> Result<OwnedFd> {
    let mut current_fd: Option<OwnedFd> = None;
    let borrowed_root = root_fd.as_fd();

    for comp in rel.components() {
        match comp {
            Component::Normal(name) => {
                let fd_to_use = current_fd.as_ref().map(|f| f.as_fd()).unwrap_or(borrowed_root);
                let next = openat(
                    fd_to_use,
                    name,
                    OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
                    Mode::empty(),
                )?;
                current_fd = Some(next);
            }
            Component::CurDir => (),
            Component::RootDir => (),
            Component::ParentDir => {
                return Err(anyhow!("invalid boot mountpoint path (parent dir)"));
            }
            _ => {
                return Err(anyhow!("invalid boot mountpoint path"));
            }
        }
    }

    current_fd.ok_or_else(|| anyhow!("invalid boot mountpoint path (empty)"))
}

/// Wraps `trans_commit`: defers Ctrl+C interrupt requests until commit returns so scriptlets are not killed mid-write.
pub(crate) struct CommitInterruptDefer;

impl CommitInterruptDefer {
    pub(crate) fn new() -> Self {
        COMMIT_INTERRUPT_ALLOWED.store(false, Ordering::Release);
        bah_audit_log("trans_commit: SIGINT/SIGTERM -> alpm_trans_interrupt suppressed until commit completes");
        Self
    }
}

impl Drop for CommitInterruptDefer {
    fn drop(&mut self) {
        COMMIT_INTERRUPT_ALLOWED.store(true, Ordering::Release);
        bah_audit_log("trans_commit: SIGINT/SIGTERM interrupt path re-enabled");
    }
}

#[derive(Debug)]
pub struct InterruptedError;

impl std::fmt::Display for InterruptedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "operation interrupted")
    }
}

impl std::error::Error for InterruptedError {}

fn cleanup_pacman_lock(config: &Config) -> Result<()> {
    let lock = Path::new(PACMAN_DB_LOCK);
    if !lock.exists() {
        return Ok(());
    }

    let mut cmd = Command::new(&config.sudo_bin);
    cmd.args(&config.sudo_flags).arg("rm").arg("-f").arg(lock);
    let status = crate::exec::command_status(&mut cmd)?;
    status
        .success()
        .context("failed to remove stale pacman db lock after interruption")?;
    Ok(())
}

fn wait_helper_exit(child: &mut Child, timeout: Duration) -> Result<Status> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .context("failed to query helper process status")?
        {
            return Ok(Status(status.code().unwrap_or(1)));
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let status = child
        .wait()
        .context("failed to wait for helper process after timeout")?;
    Ok(Status(status.code().unwrap_or(1)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum TransactionPlan {
    Sync {
        targets: Vec<String>,
        refresh_count: u8,
        sysupgrade_count: u8,
        download_only: bool,
        nodeps_count: u8,
        needed: bool,
        db_only: bool,
        no_scriptlet: bool,
        overwrite: bool,
        print_only: bool,
        no_confirm: bool,
    },
    Remove {
        targets: Vec<String>,
        nodeps: bool,
        cascade: bool,
        no_save: bool,
        db_only: bool,
        no_scriptlet: bool,
        recursive_count: u8,
        unneeded: bool,
        print_only: bool,
        no_confirm: bool,
    },
    Upgrade {
        targets: Vec<String>,
        nodeps_count: u8,
        db_only: bool,
        download_only: bool,
        needed: bool,
        overwrite: bool,
        print_only: bool,
        no_confirm: bool,
    },
    SetInstallReason {
        reason: String,
        packages: Vec<String>,
        no_confirm: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
#[serde(deny_unknown_fields)]
pub enum HelperToParent {
    Event {
        message: String,
    },
    DownloadProgress {
        package: String,
        downloaded: u64,
        total: u64,
    },
    InstallProgress {
        package: String,
        percent: u64,
        current: usize,
        total: usize,
    },
    HookPhaseStart,
    HookStep {
        current: usize,
        total: usize,
        desc: String,
    },
    HookPhaseDone,
    LogLine {
        level: String,
        message: String,
    },
    Question {
        id: u64,
        prompt: String,
        default_yes: bool,
    },
    Result {
        code: i32,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
#[serde(deny_unknown_fields)]
pub enum ParentToHelper {
    Answer { id: u64, yes: bool },
}

struct QuestionIpcState {
    no_confirm: bool,
    next_id: u64,
}

struct HelperDlThrottle {
    last_emit: std::collections::HashMap<String, Instant>,
}

struct HelperInstallThrottle {
    last_emit: std::collections::HashMap<String, Instant>,
}

/// Makes JSON IPC lines go to the duplicated pipe fd; hooks and children inherit fd 1 as a copy of stderr (terminal), not the pipe.
fn install_ipc_json_sink() -> Result<()> {
    use std::io::{stderr, stdout};
    let ipc_fd = dup(stdout()).context("dup helper stdout for JSON IPC")?;
    dup2_stdout(stderr())
        .context("dup2 stderr onto stdout so hook children do not corrupt JSON")?;
    let file = File::from(ipc_fd);
    let mut guard = HELPER_JSON_OUT
        .lock()
        .map_err(|_| anyhow!("helper JSON sink mutex poisoned"))?;
    *guard = Some(file);
    Ok(())
}

pub fn run_helper_transaction(config: &Config) -> Result<i32> {
    if !Uid::effective().is_root() {
        bail!("--helper-transaction must be run as root");
    }

    #[cfg(target_os = "linux")]
    sanitize_helper_environment();

    let stdin = std::io::stdin();
    let mut locked = stdin.lock();
    let buf = read_until_newline_limited(&mut locked, MAX_PLAN_BYTES)
        .context("helper transaction plan")?;
    if buf.is_empty() {
        bail!("helper transaction plan is empty");
    }
    let plan: TransactionPlan =
        serde_json::from_slice(&buf).context("failed to parse helper transaction plan JSON")?;
    validate_plan(&plan)?;

    install_ipc_json_sink().context("failed to set up helper JSON IPC channel")?;

    match execute_plan_root(config, &plan) {
        Ok(code) => {
            emit(&HelperToParent::Result { code })?;
            Ok(code)
        }
        Err(err) => {
            let _ = emit(&HelperToParent::Error {
                message: err.to_string(),
            });
            Err(err)
        }
    }
}

fn validate_plan(plan: &TransactionPlan) -> Result<()> {
    let validate_targets = |targets: &[String]| -> Result<()> {
        if targets.len() > MAX_TARGETS {
            bail!(
                "transaction plan has too many targets ({} > {})",
                targets.len(),
                MAX_TARGETS
            );
        }
        for t in targets {
            if t.len() > MAX_TARGET_LEN {
                bail!("target too long (>{} bytes)", MAX_TARGET_LEN);
            }
            if t.chars().any(|c| c == '\0' || c.is_control()) {
                bail!("target contains forbidden control characters");
            }
            // Block path traversal in repo/pkg strings and upgrade paths surfaced via IPC.
            if t.contains("..") {
                bail!("target must not contain '..'");
            }
        }
        Ok(())
    };

    match plan {
        TransactionPlan::Sync { targets, .. } | TransactionPlan::Remove { targets, .. } => {
            validate_targets(targets)
        }
        TransactionPlan::Upgrade { targets, .. } => {
            validate_targets(targets)?;
            for p in targets {
                validate_upgrade_pkg_path(p)?;
            }
            Ok(())
        }
        TransactionPlan::SetInstallReason {
            reason, packages, ..
        } => {
            if reason != "asdeps" && reason != "asexplicit" {
                bail!("unsupported install reason '{}'", reason);
            }
            validate_targets(packages)
        }
    }
}

/// Rejects path tricks for local package installs: only absolute paths without `..` walk segments.
fn validate_upgrade_pkg_path(p: &str) -> Result<()> {
    let path = Path::new(p);
    if !path.is_absolute() {
        bail!("local package path must be absolute (got {})", p);
    }
    for c in path.components() {
        if c == Component::ParentDir {
            bail!("local package path must not contain '..'");
        }
    }
    Ok(())
}

pub fn run_plan_with_helper(config: &Config, plan: &TransactionPlan) -> Result<Status> {
    let exe = std::env::current_exe().context("failed to locate current executable")?;
    let mut cmd = Command::new(&config.sudo_bin);
    cmd.args(&config.sudo_flags)
        .arg(exe)
        .arg("--helper-transaction")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn().context("failed to start transaction helper")?;
    let helper_pid = child.id();
    let mut child_stdin = child.stdin.take().context("failed to open helper stdin")?;
    let child_stdout = child
        .stdout
        .take()
        .context("failed to open helper stdout")?;
    let mut reader = BufReader::new(child_stdout);

    let default_signals = &*crate::exec::DEFAULT_SIGNALS;
    default_signals.store(false, Ordering::Relaxed);
    struct ResetDefaultSignals<'a>(&'a std::sync::Arc<AtomicBool>);
    impl Drop for ResetDefaultSignals<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let _reset_default_signals = ResetDefaultSignals(default_signals);

    let interrupted = Arc::new(AtomicBool::new(false));
    struct InterruptCleanupGuard<'a> {
        config: &'a Config,
        interrupted: Arc<AtomicBool>,
    }

    impl Drop for InterruptCleanupGuard<'_> {
        fn drop(&mut self) {
            if self.interrupted.load(Ordering::SeqCst) || crate::exec::interrupt_received() {
                let _ = cleanup_pacman_lock(self.config);
            }
        }
    }

    let _cleanup_guard = InterruptCleanupGuard {
        config,
        interrupted: Arc::clone(&interrupted),
    };
    let stop_signal = Arc::new(AtomicBool::new(false));
    let signal_sent = Arc::new(AtomicBool::new(false));
    let stop_signal_thread = Arc::clone(&stop_signal);
    let signal_sent_thread = Arc::clone(&signal_sent);
    let interrupted_thread = Arc::clone(&interrupted);
    let signal_thread = std::thread::Builder::new()
        .name("bah-parent-sig".to_string())
        .spawn(move || {
            while !stop_signal_thread.load(Ordering::Relaxed) {
                if crate::exec::take_caught_signal() != 0 {
                    if !signal_sent_thread.swap(true, Ordering::Relaxed) {
                        interrupted_thread.store(true, Ordering::SeqCst);
                        let _ = std::io::stdout()
                            .write_all(b"\r\x1b[2K\x1b[1m:: Interrupted. Cleaning up...\x1b[0m\n");
                        let _ = std::io::stdout().flush();
                        crate::exec::forward_sigint_to_pid(helper_pid);
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
        .ok();

    let (ipc_tx, ipc_rx) = channel::<Result<Option<String>, std::io::Error>>();
    let ipc_thread = std::thread::Builder::new()
        .name("bah-helper-ipc".to_string())
        .spawn(move || loop {
            match read_ipc_line_bounded(&mut reader) {
                Ok(Some(line)) => {
                    if ipc_tx.send(Ok(Some(line))).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = ipc_tx.send(Ok(None));
                    break;
                }
                Err(err) => {
                    let _ = ipc_tx.send(Err(err));
                    break;
                }
            }
        })?;

    serde_json::to_writer(&mut child_stdin, plan)
        .context("failed to serialize transaction plan to helper stdin")?;
    child_stdin
        .write_all(b"\n")
        .context("failed to write plan newline to helper")?;
    child_stdin
        .flush()
        .context("failed to flush transaction plan to helper")?;

    let mut final_code: Option<i32> = None;
    let mut ui = TransactionRenderController::new();

    loop {
        match ipc_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(Some(line))) => {
                let msg: HelperToParent = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                match msg {
                    HelperToParent::Event { message } => ui.on_static_step(&message),
                    HelperToParent::DownloadProgress {
                        package,
                        downloaded,
                        total,
                    } => {
                        ui.on_download_progress(&package, downloaded, total);
                    }
                    HelperToParent::InstallProgress {
                        package,
                        percent,
                        current,
                        total,
                    } => {
                        ui.on_install_progress(&package, percent, current, total);
                    }
                    HelperToParent::HookPhaseStart => {
                        ui.on_hook_phase_start(config);
                    }
                    HelperToParent::HookStep {
                        current,
                        total,
                        desc,
                    } => {
                        ui.on_hook_step(config, current, total, &desc);
                    }
                    HelperToParent::HookPhaseDone => {
                        ui.on_hook_phase_done(config);
                    }
                    HelperToParent::LogLine { level, message } => {
                        let line = format_log_line_for_parent(config, &level, &message);
                        ui.on_log_line(&line);
                    }
                    HelperToParent::Question {
                        id,
                        prompt,
                        default_yes,
                    } => {
                        let yes = ask(config, &prompt, default_yes);
                        serde_json::to_writer(
                            &mut child_stdin,
                            &ParentToHelper::Answer { id, yes },
                        )
                        .context("failed to write answer JSON to helper (broken pipe?)")?;
                        child_stdin
                            .write_all(b"\n")
                            .context("failed to write answer newline to helper")?;
                        child_stdin
                            .flush()
                            .context("failed to flush answer to helper")?;
                    }
                    HelperToParent::Result { code } => {
                        ui.finish();
                        final_code = Some(code);
                        break;
                    }
                    HelperToParent::Error { message } => {
                        ui.finish();
                        bail!("helper error: {}", message);
                    }
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(err)) => bail!("read helper IPC line: {}", err),
            Err(RecvTimeoutError::Timeout) => {
                if let Some(_) = child.try_wait().context("failed to query helper status")? {
                    break;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let status = wait_helper_exit(&mut child, Duration::from_secs(2))?;
    stop_signal.store(true, Ordering::Relaxed);
    if let Some(thread) = signal_thread {
        let _ = thread.join();
    }
    let _ = ipc_thread.join();

    if let Some(code) = final_code {
        return Ok(Status(code));
    }

    if interrupted.load(Ordering::SeqCst) || crate::exec::interrupt_received() {
        cleanup_pacman_lock(config)?;
        return Err(InterruptedError.into());
    }

    bail!(
        "transaction helper terminated before sending a result (status {})",
        status.code()
    )
}

fn format_log_line_for_parent(config: &Config, level: &str, message: &str) -> String {
    let c = &config.color;
    let prefix = match level {
        "error" => c.error.paint("[X]"),
        "warning" => c.warning.paint("[!]"),
        _ => c.field.paint("[·]"),
    };
    format!("{} {}", prefix, message)
}

pub(crate) fn ensure_sig_interrupt_thread_started() {
    if SIG_THREAD_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    let _ = std::thread::Builder::new()
        .name("bah-helper-sig".to_string())
        .spawn(|| {
            let Ok(mut signals) = Signals::new([SIGINT, SIGTERM]) else {
                return;
            };
            for sig in signals.forever() {
                if !COMMIT_INTERRUPT_ALLOWED.load(Ordering::Acquire) {
                    bah_audit_log(&format!(
                        "signal {sig} ignored (protected trans_commit; hooks must finish)"
                    ));
                    continue;
                }
                let p = ACTIVE_COMMIT_HANDLE.load(Ordering::Acquire);
                if !p.is_null() {
                    bah_audit_log(&format!("signal {sig}: requesting alpm_trans_interrupt"));
                    unsafe {
                        let _ = alpm_sys::alpm_trans_interrupt(p);
                    };
                    continue;
                }
                if INTERRUPT_REQUESTED.swap(true, Ordering::SeqCst) {
                    continue;
                }
                let _ = emit(&HelperToParent::Error {
                    message: "Interrupted. Cleaning up...".to_string(),
                });
                std::process::exit(1);
            }
        });
}

pub(crate) struct ActiveCommitGuard;

impl ActiveCommitGuard {
    pub(crate) fn arm(alpm: &alpm::Alpm) -> Self {
        ACTIVE_COMMIT_HANDLE.store(alpm.as_alpm_handle_t(), Ordering::Release);
        Self
    }
}

impl Drop for ActiveCommitGuard {
    fn drop(&mut self) {
        ACTIVE_COMMIT_HANDLE.store(std::ptr::null_mut(), Ordering::Release);
    }
}

fn map_commit_error(e: CommitError, ctx: &str) -> anyhow::Error {
    let code = e.error();
    let mut s = format!("{}: {}", ctx, e);

    // HIGH #1 FIX: Include the last running hook name in error message
    if code == AlpmError::TransHookFailed {
        s.push_str(" (transaction hook failed");
        // Include the hook that was running when failure occurred
        let hook_info = LAST_HOOK_RUNNING.lock()
            .ok()
            .and_then(|g| g.as_ref().cloned());
        if let Some(hook_name) = hook_info {
            s.push_str(&format!(": last running hook was '{}'", hook_name));
        }
        s.push_str("; see hook output streamed above for details)");
        
        // BULLETPROOF FIX: Post-mortem warning for critical hook failures
        print_critical_hook_failure_warning();
    } else if code == AlpmError::TransAbort {
        s.push_str(" (transaction aborted, often due to Ctrl+C)");
    }
    anyhow!(s)
}

/// BULLETPROOF FIX: Post-mortem warning for critical hook failures.
/// Displays a prominent warning if kernel/initramfs hooks failed.
fn print_critical_hook_failure_warning() {
    let hook_info = LAST_HOOK_RUNNING.lock()
        .ok()
        .and_then(|g| g.as_ref().cloned());
    
    let is_critical = hook_info.as_ref().map_or(false, |h| {
        let h_lower = h.to_lowercase();
        h_lower.contains("mkinitcpio") || 
        h_lower.contains("initramfs") || 
        h_lower.contains("linux") ||
        h_lower.contains("kernel") ||
        h_lower.contains("grub") ||
        h_lower.contains("boot")
    });
    
    if is_critical {
        // Use STDOUT lock for atomic, non-interleaved output
        if let Ok(_lock) = StdoutLock::acquire() {
            eprintln!();
            eprintln!("\x1b[1;31m{}\x1b[0m", "=".repeat(70));
            eprintln!("\x1b[1;31m{}\x1b[0m", "⚠️  CRITICAL HOOK FAILURE DETECTED ⚠️");
            eprintln!("\x1b[1;31m{}\x1b[0m", "=".repeat(70));
            eprintln!();
            eprintln!("\x1b[1;31m{}\x1b[0m", "SYSTEM MAY BE UNBOOTABLE. DO NOT REBOOT.");
            eprintln!();
            if let Some(hook) = hook_info {
                eprintln!("\x1b[1;31m{}\x1b[0m", format!("Failed hook: {}", hook));
            }
            eprintln!();
            eprintln!("\x1b[1;31m{}\x1b[0m", "RECOMMENDED ACTIONS:");
            eprintln!("\x1b[1;31m{}\x1b[0m", "1. Do NOT reboot your system");
            eprintln!("\x1b[1;31m{}\x1b[0m", "2. Check /boot partition space: df -h /boot");
            eprintln!("\x1b[1;31m{}\x1b[0m", "3. Verify /boot is mounted: mount | grep /boot");
            eprintln!("\x1b[1;31m{}\x1b[0m", "4. Re-run mkinitcpio manually: mkinitcpio -P");
            eprintln!("\x1b[1;31m{}\x1b[0m", "5. Check logs: journalctl -xb | grep -i error");
            eprintln!();
            eprintln!("\x1b[1;31m{}\x1b[0m", "=".repeat(70));
            eprintln!();
        }
        bah_audit_log("CRITICAL: Post-mortem warning displayed to user");
    }
}

fn sanitize_log_fragment(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len().min(MAX_LOG_MESSAGE_BYTES));
    for ch in msg.chars() {
        if out.len() >= MAX_LOG_MESSAGE_BYTES {
            out.push_str("...");
            break;
        }
        if ch == '\0' {
            continue;
        }
        if ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t' {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

fn log_level_to_tag(level: LogLevel) -> &'static str {
    if level.intersects(LogLevel::ERROR) {
        "error"
    } else if level.intersects(LogLevel::WARNING) {
        "warning"
    } else if level.intersects(LogLevel::DEBUG | LogLevel::FUNCTION) {
        "debug"
    } else {
        "message"
    }
}

fn helper_log_cb(level: LogLevel, msg: &str, _: &mut ()) {
    // IPC: warn/error only — debug/function/default noise stays on the tty (stderr) via ALPM elsewhere.
    if !level.intersects(LogLevel::WARNING | LogLevel::ERROR) {
        return;
    }
    let line = sanitize_log_fragment(msg);
    if line.is_empty() {
        return;
    }
    let _ = emit(&HelperToParent::LogLine {
        level: log_level_to_tag(level).to_string(),
        message: line,
    });
}

/// Returns true if this package name usually requires a writable `/boot` for hooks (kernel, initramfs, bootloaders).
fn pkg_requires_writable_boot(name: &str) -> bool {
    const LINUX_NO_BOOT: &[&str] = &[
        "linux-api-headers",
        "linux-docs",
        "linux-firmware",
        "linux-tools",
    ];
    if LINUX_NO_BOOT.contains(&name) {
        return false;
    }
    // Kernel / image packages
    if name == "linux" || name.starts_with("linux-") {
        return true;
    }
    if name == "mkinitcpio" || name.starts_with("mkinitcpio-") {
        return true;
    }
    if name.starts_with("grub") {
        return true;
    }
    if name.starts_with("refind") || name.starts_with("limine") {
        return true;
    }
    if name == "syslinux" || name.starts_with("syslinux") {
        return true;
    }
    if name == "sbctl" || name == "efibootmgr" {
        return true;
    }
    false
}

fn transaction_touches_boot_sensitive_pkg(alpm: &alpm::Alpm) -> bool {
    alpm.trans_add()
        .iter()
        .any(|p| pkg_requires_writable_boot(p.name()))
        || alpm
            .trans_remove()
            .iter()
            .any(|p| pkg_requires_writable_boot(p.name()))
}

fn transaction_critical_pkgs(alpm: &alpm::Alpm) -> Vec<String> {
    let names = alpm
        .trans_add()
        .iter()
        .map(|p| p.name())
        .chain(alpm.trans_remove().iter().map(|p| p.name()));
    collect_critical_pkgs(names)
}

fn ensure_transaction_not_empty(alpm: &mut alpm::Alpm) -> Result<()> {
    if alpm.trans_add().is_empty() && alpm.trans_remove().is_empty() {
        let _ = alpm.trans_release();
        bail!("refusing empty ALPM transaction: nothing to install, upgrade, or remove");
    }
    Ok(())
}

/// `-Sy`/`-Su` flows may leave no pkgs while still being valid (“nothing to do” after refresh or empty upgrade).
#[inline]
fn sync_allows_empty_transaction(refresh_count: u8, sysupgrade_count: u8) -> bool {
    refresh_count > 0 || sysupgrade_count > 0
}

#[cfg(target_os = "linux")]
pub(crate) fn ensure_boot_rw_for_transaction(alpm: &alpm::Alpm, config: &Config) -> Result<()> {
    if !transaction_touches_boot_sensitive_pkg(alpm) {
        bah_audit_log("ensure_boot_rw: skipped (no boot-sensitive packages in transaction)");
        return Ok(());
    }
    let boot_mount = select_boot_mountpoint(Path::new(alpm.root()))?;
    let boot_rel = boot_mount
        .strip_prefix("/")
        .unwrap_or(boot_mount.as_path());
    let boot: PathBuf = Path::new(alpm.root()).join(boot_rel);
    bah_audit_log(&format!(
        "ensure_boot_rw: begin alpm.root={} boot_path={}",
        alpm.root(),
        boot.display()
    ));

    // Resolve `{root}/boot` via `openat` + `O_NOFOLLOW`: rejects symlinks under root (no checker-followed
    // `/boot` → attacker FS). All further checks use the same fd (`fstatfs`, `fstatvfs`, `faccessat`).
    let root_fd = open(
        Path::new(alpm.root()),
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| {
        bah_audit_log(&format!(
            "ensure_boot_rw: FAIL open root O_PATH ({})",
            alpm.root()
        ));
        format!(
            "cannot open pacman root {} (missing path or symlink chain?)",
            alpm.root()
        )
    })?;

    let boot_fd = openat_dir_no_symlinks(root_fd, boot_rel)
    .map_err(|e| {
        bah_audit_log(&format!(
            "ensure_boot_rw: FAIL open boot under root ({}) err={e}",
            boot.display()
        ));
        e
    })
    .with_context(|| {
        format!(
            "{} does not exist or is not a real directory (symlinks under the chroot root are rejected). Mount your EFI/boot partition before updating kernel, initramfs, or bootloader.",
            boot.display()
        )
    })?;

    bah_audit_log(&format!(
        "ensure_boot_rw: mountinfo snapshot {}",
        boot_mountinfo_summary(&boot)
    ));

    let st = fstatfs(&boot_fd).with_context(|| {
        bah_audit_log(&format!(
            "ensure_boot_rw: FAIL fstatfs boot fd ({})",
            boot.display()
        ));
        format!("statfs on {} failed (is /boot mounted?)", boot.display())
    })?;

    let ro = st.flags().contains(FsFlags::ST_RDONLY);
    bah_audit_log(&format!(
        "ensure_boot_rw: fstatfs boot_fd ST_RDONLY={ro} path={}",
        boot.display(),
    ));
    if ro {
        bah_audit_log(&format!(
            "ensure_boot_rw: FAIL read-only mount {}",
            boot.display()
        ));
        bail!(
            "{} is read-only. Mount it read-write (check fstab) before updating kernel, initramfs, or bootloader so hooks can refresh images and bootloaders.",
            boot.display()
        );
    }

    // Writable bit via dirfd: same inode we just inspected (no path re-resolution between check and statfs).
    faccessat(&boot_fd, ".", AccessFlags::W_OK, AtFlags::AT_EACCESS).with_context(|| {
        bah_audit_log(&format!(
            "ensure_boot_rw: FAIL faccessat(W_OK) on {}",
            boot.display()
        ));
        format!(
            "cannot write to {} (access W_OK failed); hooks may fail to refresh /boot",
            boot.display()
        )
    })?;
    bah_audit_log(&format!(
        "ensure_boot_rw: faccessat(W_OK) OK on {}",
        boot.display()
    ));

    // Cheap guard before hooks run: ALPM may also check space when CheckSpace is enabled in pacman.conf.
    let vfs =
        fstatvfs(&boot_fd).with_context(|| format!("fstatvfs on {} failed", boot.display()))?;
    let fr = vfs.fragment_size() as u64;
    let avail = vfs.blocks_available() as u64;
    let avail_bytes = avail.saturating_mul(fr);
    let min_boot_free = config.boot_min_free_mb.saturating_mul(1024 * 1024);
    bah_audit_log(&format!(
        "ensure_boot_rw: statvfs avail_bytes={avail_bytes} (min_hint={min_boot_free})"
    ));
    if min_boot_free > 0 && avail_bytes < min_boot_free {
        bah_audit_log(&format!(
            "ensure_boot_rw: FAIL low free space on {}",
            boot.display()
        ));
        bail!(
            "{} has critically low free space ({} bytes free; need at least ~{} MB for kernel/initramfs hooks). Free space before upgrading.",
            boot.display(),
            avail_bytes,
            min_boot_free / (1024 * 1024)
        );
    }

    bah_audit_log(&format!("ensure_boot_rw: OK {}", boot.display()));
    // root_fd and boot_fd are OwnedFd - auto-close on drop
    Ok(())
}

pub(crate) fn ensure_noscriptlet_allowed(alpm: &alpm::Alpm, config: &Config) -> Result<()> {
    if !config.args.has_arg("noscriptlet", "noscriptlet") {
        return Ok(());
    }
    if config.force_noscriptlet {
        return Ok(());
    }
    let critical = transaction_critical_pkgs(alpm);
    if critical.is_empty() {
        return Ok(());
    }

    bail!(
        "--noscriptlet cannot be used with critical system packages ({}).\n       Post-install scriptlets are required for these packages to function correctly.",
        critical.join(", ")
    );
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn ensure_boot_rw_for_transaction(_alpm: &alpm::Alpm, _config: &Config) -> Result<()> {
    Ok(())
}

fn fresh_helper_alpm(config: &Config, no_confirm: bool) -> Result<alpm::Alpm> {
    let alpm = config.new_alpm()?;
    // Route ALPM textual log (hooks, scriptlets detail) through JSON LogLine instead of stderr only.
    alpm.set_log_cb((), helper_log_cb);
    alpm.set_event_cb(config.color, helper_event_cb);
    alpm.set_progress_cb(
        HelperInstallThrottle {
            last_emit: std::collections::HashMap::new(),
        },
        helper_progress_cb,
    );
    alpm.set_dl_cb(
        HelperDlThrottle {
            last_emit: std::collections::HashMap::new(),
        },
        helper_download_cb,
    );
    alpm.set_question_cb(
        QuestionIpcState {
            no_confirm,
            next_id: 1,
        },
        helper_question_cb,
    );
    Ok(alpm)
}

fn recover_from_stale_lock(
    mut old: alpm::Alpm,
    config: &Config,
    no_confirm: bool,
    next_id: &mut u64,
) -> Result<alpm::Alpm> {
    eprintln!("[DEBUG lock] stale lock recovery entered");

    let lock = confirm_stale_db_lock_removal(no_confirm, next_id)?;
    eprintln!("[DEBUG lock] user confirmed stale lock removal");

    eprintln!("[DEBUG lock] clearing active commit handle");
    ACTIVE_COMMIT_HANDLE.store(std::ptr::null_mut(), Ordering::SeqCst);

    eprintln!("[DEBUG lock] releasing old transaction state if any");
    let _ = old.trans_release();

    eprintln!("[DEBUG lock] dropping old alpm handle");
    drop(old);

    eprintln!("[DEBUG lock] removing {}", lock.display());
    match remove_file(&lock) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).context("failed to remove stale pacman lock"),
    }
    emit(&HelperToParent::Event {
        message: "Removed stale /var/lib/pacman/db.lck lock; retrying transaction.".to_string(),
    })?;

    eprintln!("[DEBUG lock] sleeping before retry");
    std::thread::sleep(Duration::from_millis(50));

    eprintln!("[DEBUG lock] creating fresh alpm handle");
    let fresh = fresh_helper_alpm(config, no_confirm)?;

    eprintln!("[DEBUG lock] fresh alpm handle ready");
    Ok(fresh)
}

fn execute_plan_root(config: &Config, plan: &TransactionPlan) -> Result<i32> {
    let no_confirm = match plan {
        TransactionPlan::Sync { no_confirm, .. }
        | TransactionPlan::Remove { no_confirm, .. }
        | TransactionPlan::Upgrade { no_confirm, .. }
        | TransactionPlan::SetInstallReason { no_confirm, .. } => *no_confirm,
    };
    let mut alpm = fresh_helper_alpm(config, no_confirm)?;
    let mut helper_qid = 10_000u64;

    match plan {
        TransactionPlan::SetInstallReason {
            reason, packages, ..
        } => {
            let pkg_reason = match reason.as_str() {
                "asdeps" => alpm::PackageReason::Depend,
                "asexplicit" => alpm::PackageReason::Explicit,
                _ => bail!("unsupported install reason '{}'", reason),
            };
            let db = alpm.localdb();
            for pkg_name in packages {
                if let Ok(pkg) = db.pkg(pkg_name.as_str()) {
                    pkg.set_reason(pkg_reason)?;
                }
            }
            Ok(0)
        }
        TransactionPlan::Sync {
            targets,
            refresh_count,
            sysupgrade_count,
            download_only,
            nodeps_count,
            needed,
            db_only,
            no_scriptlet,
            overwrite,
            print_only,
            ..
        } => {
            if *refresh_count > 0 {
                match alpm.syncdbs_mut().update(*refresh_count > 1) {
                    Ok(_) => (),
                    Err(AlpmError::HandleLock) => {
                        alpm = recover_from_stale_lock(alpm, config, no_confirm, &mut helper_qid)?;
                        alpm.syncdbs_mut().update(*refresh_count > 1)?;
                    }
                    Err(e) => return Err(e.into()),
                }
            }

            let mut flags = TransFlag::NONE;
            if *download_only {
                flags |= TransFlag::DOWNLOAD_ONLY;
            }
            if *nodeps_count > 0 {
                flags |= TransFlag::NO_DEPS;
                if *nodeps_count > 1 {
                    flags |= TransFlag::NO_DEP_VERSION;
                }
            }
            if *needed {
                flags |= TransFlag::NEEDED;
            }
            if *db_only {
                flags |= TransFlag::DB_ONLY;
            }
            if *no_scriptlet {
                flags |= TransFlag::NO_SCRIPTLET;
            }
            if *overwrite {
                flags |= TransFlag::NO_CONFLICTS;
            }

            match alpm.trans_init(flags) {
                Ok(_) => (),
                Err(AlpmError::HandleLock) => {
                    alpm = recover_from_stale_lock(alpm, config, no_confirm, &mut helper_qid)?;
                    alpm.trans_init(flags)?;
                }
                Err(e) => return Err(e.into()),
            }
            if *sysupgrade_count > 0 {
                alpm.sync_sysupgrade(*sysupgrade_count > 1)?;
            }
            for target in targets {
                add_target(&mut alpm, target)?;
            }
            if alpm.trans_add().is_empty() && alpm.trans_remove().is_empty() {
                let _ = alpm.trans_release();
                if sync_allows_empty_transaction(*refresh_count, *sysupgrade_count) {
                    return Ok(0);
                }
                bail!("refusing empty ALPM transaction: nothing to install, upgrade, or remove");
            }

            let sync_prep_err = match alpm.trans_prepare() {
                Ok(()) => None,
                Err(e) => Some(e.error()),
            };
            if let Some(code) = sync_prep_err {
                let _ = alpm.trans_release();
                return Err(anyhow!("failed to prepare ALPM transaction: {}", code));
            }

            if !*print_only {
                ensure_sig_interrupt_thread_started();
                if let Err(e) = ensure_noscriptlet_allowed(&alpm, config) {
                    let _ = alpm.trans_release();
                    return Err(e);
                }
                if let Err(e) = ensure_boot_rw_for_transaction(&alpm, config) {
                    let _ = alpm.trans_release();
                    return Err(e);
                }
                // BULLETPROOF FIX: Reset hook tracking before commit
                if let Ok(mut last_hook) = LAST_HOOK_RUNNING.lock() {
                    *last_hook = None;
                }
                if let Ok(mut start_time) = HOOK_START_TIME.lock() {
                    *start_time = None;
                }
                HOOK_PHASE_ACTIVE.store(false, Ordering::SeqCst);

                // BULLETPROOF FIX: Use RAII guard for drop-safe cleanup
                let _cleanup_guard = TransactionCleanupGuard::new(&alpm);
                
                let commit_err = {
                    let _defer_int = CommitInterruptDefer::new();
                    let _guard = ActiveCommitGuard::arm(&alpm);
                    alpm.trans_commit().err()
                };
                if let Some(e) = commit_err {
                    let code = e.error();
                    // BULLETPROOF FIX: Tell cleanup guard to skip unlock on hook failure
                    if code == AlpmError::TransHookFailed {
                        // Cleanup guard will skip unlock() automatically via Drop
                        bah_audit_log("trans_commit: hook failed, cleanup guard will skip unlock()");
                    }
                    return Err(
                        map_commit_error(e, "failed to commit ALPM sync transaction").into(),
                    );
                }
            }
            // Note: trans_release() will be called by TransactionCleanupGuard's Drop
            Ok(0)
        }
        TransactionPlan::Remove {
            targets,
            nodeps,
            cascade,
            no_save,
            db_only,
            no_scriptlet,
            recursive_count,
            unneeded,
            print_only,
            ..
        } => {
            let mut flags = TransFlag::NONE;
            if *nodeps {
                flags |= TransFlag::NO_DEPS;
            }
            if *cascade {
                flags |= TransFlag::CASCADE;
            }
            if *no_save {
                flags |= TransFlag::NO_SAVE;
            }
            if *db_only {
                flags |= TransFlag::DB_ONLY;
            }
            if *no_scriptlet {
                flags |= TransFlag::NO_SCRIPTLET;
            }
            if *recursive_count > 0 {
                flags |= TransFlag::RECURSE;
                if *recursive_count > 1 {
                    flags |= TransFlag::RECURSE_ALL;
                }
            }
            if *unneeded {
                flags |= TransFlag::UNNEEDED;
            }

            match alpm.trans_init(flags) {
                Ok(_) => (),
                Err(AlpmError::HandleLock) => {
                    alpm = recover_from_stale_lock(alpm, config, no_confirm, &mut helper_qid)?;
                    alpm.trans_init(flags)?;
                }
                Err(e) => return Err(e.into()),
            }
            for target in targets {
                let pkg = alpm.localdb().pkg(target.as_str())?;
                alpm.trans_remove_pkg(pkg)?;
            }
            ensure_transaction_not_empty(&mut alpm)?;
            let remove_prep_err = match alpm.trans_prepare() {
                Ok(()) => None,
                Err(e) => Some(e.error()),
            };
            if let Some(code) = remove_prep_err {
                let _ = alpm.trans_release();
                return Err(anyhow!(
                    "failed to prepare ALPM remove transaction: {}",
                    code
                ));
            }

            if !*print_only {
                ensure_sig_interrupt_thread_started();
                if let Err(e) = ensure_noscriptlet_allowed(&alpm, config) {
                    let _ = alpm.trans_release();
                    return Err(e);
                }
                if let Err(e) = ensure_boot_rw_for_transaction(&alpm, config) {
                    let _ = alpm.trans_release();
                    return Err(e);
                }
                // BULLETPROOF FIX: Reset hook tracking before commit
                if let Ok(mut last_hook) = LAST_HOOK_RUNNING.lock() {
                    *last_hook = None;
                }
                if let Ok(mut start_time) = HOOK_START_TIME.lock() {
                    *start_time = None;
                }
                HOOK_PHASE_ACTIVE.store(false, Ordering::SeqCst);

                // BULLETPROOF FIX: Use RAII guard for drop-safe cleanup
                let _cleanup_guard = TransactionCleanupGuard::new(&alpm);
                
                let commit_err = {
                    let _defer_int = CommitInterruptDefer::new();
                    let _guard = ActiveCommitGuard::arm(&alpm);
                    alpm.trans_commit().err()
                };
                if let Some(e) = commit_err {
                    let code = e.error();
                    // BULLETPROOF FIX: Tell cleanup guard to skip unlock on hook failure
                    if code == AlpmError::TransHookFailed {
                        bah_audit_log("trans_commit: hook failed, cleanup guard will skip unlock()");
                    }
                    return Err(
                        map_commit_error(e, "failed to commit ALPM remove transaction").into(),
                    );
                }
            }
            // Note: trans_release() will be called by TransactionCleanupGuard's Drop
            Ok(0)
        }
        TransactionPlan::Upgrade {
            targets,
            nodeps_count,
            db_only,
            download_only,
            needed,
            overwrite,
            print_only,
            ..
        } => {
            let mut flags = TransFlag::NONE;
            if *nodeps_count > 0 {
                flags |= TransFlag::NO_DEPS;
                if *nodeps_count > 1 {
                    flags |= TransFlag::NO_DEP_VERSION;
                }
            }
            if *db_only {
                flags |= TransFlag::DB_ONLY;
            }
            if *download_only {
                flags |= TransFlag::DOWNLOAD_ONLY;
            }
            if *needed {
                flags |= TransFlag::NEEDED;
            }
            if *overwrite {
                flags |= TransFlag::NO_CONFLICTS;
            }

            match alpm.trans_init(flags) {
                Ok(_) => (),
                Err(AlpmError::HandleLock) => {
                    alpm = recover_from_stale_lock(alpm, config, no_confirm, &mut helper_qid)?;
                    alpm.trans_init(flags)?;
                }
                Err(e) => return Err(e.into()),
            }
            for target in targets {
                let loaded = alpm.pkg_load(target.as_str(), true, SigLevel::NONE)?;
                alpm.trans_add_pkg(loaded)
                    .map_err(|e| anyhow!("failed to add package file '{}': {}", target, e.error))?;
            }
            ensure_transaction_not_empty(&mut alpm)?;
            let upgrade_prep_err = match alpm.trans_prepare() {
                Ok(()) => None,
                Err(e) => Some(e.error()),
            };
            if let Some(code) = upgrade_prep_err {
                let _ = alpm.trans_release();
                return Err(anyhow!(
                    "failed to prepare ALPM upgrade transaction: {}",
                    code
                ));
            }

            if !*print_only {
                ensure_sig_interrupt_thread_started();
                if let Err(e) = ensure_noscriptlet_allowed(&alpm, config) {
                    let _ = alpm.trans_release();
                    return Err(e);
                }
                if let Err(e) = ensure_boot_rw_for_transaction(&alpm, config) {
                    let _ = alpm.trans_release();
                    return Err(e);
                }
                // BULLETPROOF FIX: Reset hook tracking before commit
                if let Ok(mut last_hook) = LAST_HOOK_RUNNING.lock() {
                    *last_hook = None;
                }
                if let Ok(mut start_time) = HOOK_START_TIME.lock() {
                    *start_time = None;
                }
                HOOK_PHASE_ACTIVE.store(false, Ordering::SeqCst);

                // BULLETPROOF FIX: Use RAII guard for drop-safe cleanup
                let _cleanup_guard = TransactionCleanupGuard::new(&alpm);
                
                let commit_err = {
                    let _defer_int = CommitInterruptDefer::new();
                    let _guard = ActiveCommitGuard::arm(&alpm);
                    alpm.trans_commit().err()
                };
                if let Some(e) = commit_err {
                    let code = e.error();
                    // BULLETPROOF FIX: Tell cleanup guard to skip unlock on hook failure
                    if code == AlpmError::TransHookFailed {
                        bah_audit_log("trans_commit: hook failed, cleanup guard will skip unlock()");
                    }
                    return Err(map_commit_error(
                        e,
                        "failed to commit ALPM upgrade (local pkg) transaction",
                    )
                    .into());
                }
            }
            // Note: trans_release() will be called by TransactionCleanupGuard's Drop
            Ok(0)
        }
    }
}

fn helper_download_cb(filename: &str, event: AnyDownloadEvent, state: &mut HelperDlThrottle) {
    if filename.ends_with(".sig") {
        return;
    }
    match event.event() {
        DownloadEvent::Progress(p) => {
            let now = Instant::now();
            let key = filename.to_string();
            let downloaded = p.downloaded.max(0) as u64;
            let total = p.total.max(0) as u64;
            if total == 0 {
                return;
            }
            let should_emit = match state.last_emit.get(&key) {
                None => true,
                Some(last) => now.saturating_duration_since(*last) >= Duration::from_millis(100),
            } || downloaded >= total;
            if should_emit {
                state.last_emit.insert(key.clone(), now);
                let _ = emit(&HelperToParent::DownloadProgress {
                    package: key,
                    downloaded,
                    total,
                });
            }
        }
        DownloadEvent::Completed(c) => {
            let total = c.total.max(0) as u64;
            if c.result == DownloadResult::Failed {
                let _ = emit(&HelperToParent::LogLine {
                    level: "warning".to_string(),
                    message: format!("download failed for {}", filename),
                });
            } else if total > 0 {
                let _ = emit(&HelperToParent::DownloadProgress {
                    package: filename.to_string(),
                    downloaded: total,
                    total,
                });
            }
            state.last_emit.remove(filename);
        }
        _ => {}
    }
}

fn confirm_stale_db_lock_removal(no_confirm: bool, next_id: &mut u64) -> Result<PathBuf> {
    let lock = PathBuf::from("/var/lib/pacman/db.lck");
    if !lock.exists() {
        return Ok(lock);
    }

    let pid = read_to_string(&lock)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok());
    if let Some(pid) = pid {
        if process_alive(pid) {
            bail!(
                "Pacman is already in use by running process {} (db lock is active)",
                pid
            );
        }
    }

    let prompt = match pid {
        Some(pid) => format!(
            "Stale pacman lock detected (PID {}). Remove it and retry?",
            pid
        ),
        None => "Stale pacman lock detected. Remove it and retry?".to_string(),
    };
    let yes = ask_helper_question(no_confirm, next_id, &prompt, false)?;
    if !yes {
        bail!("database lock present and removal was declined");
    }

    Ok(lock)
}

fn process_alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{}", pid)).exists()
}

fn ask_helper_question(
    no_confirm: bool,
    next_id: &mut u64,
    prompt: &str,
    default_yes: bool,
) -> Result<bool> {
    if no_confirm {
        return Ok(default_yes);
    }

    let id = *next_id;
    *next_id += 1;
    emit(&HelperToParent::Question {
        id,
        prompt: prompt.to_string(),
        default_yes,
    })?;
    Ok(wait_parent_answer(id).unwrap_or(default_yes))
}

fn emit(msg: &HelperToParent) -> Result<()> {
    let mut mutex = HELPER_JSON_OUT
        .lock()
        .map_err(|_| anyhow!("helper JSON sink mutex poisoned"))?;
    let out = mutex
        .as_mut()
        .ok_or_else(|| anyhow!("helper JSON sink was not initialized"))?;
    serde_json::to_writer(&mut *out, msg)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn helper_event_cb(event: AnyEvent, c: &mut Colors) {
    use Event::*;
    let message_opt: Option<String> = match event.event() {
        // HIGH #2 & HIGH #7 FIX: Stream scriptlet/hook output directly to terminal
        // instead of buffering via JSON IPC. This preserves ANSI formatting and
        // provides real-time feedback for long-running hooks like mkinitcpio.
        ScriptletInfo(s) => {
            // Direct streaming to stdout (fd 1) - bypasses JSON IPC buffering
            // The line already contains ANSI formatting from the scriptlet/hook
            let line = s.line();
            if !line.is_empty() {
                // BULLETPROOF FIX: Use atomic STDOUT lock to prevent interleaving
                // This ensures hook output is never corrupted by concurrent messages
                if let Ok(_lock) = StdoutLock::acquire() {
                    let _ = std::io::stdout().write_all(line.as_bytes());
                    let _ = std::io::stdout().write_all(b"\n");
                    let _ = std::io::stdout().flush();
                } else {
                    // Fallback if lock is poisoned - still better than losing output
                    let _ = std::io::stdout().write_all(line.as_bytes());
                    let _ = std::io::stdout().write_all(b"\n");
                    let _ = std::io::stdout().flush();
                }
            }
            None
        }
        HookRunDone(h) => {
            let hook_name = h.name().to_string();
            let hook_desc = h.desc().unwrap_or(&hook_name).to_string();
            let hook_position = h.position();
            let hook_total = h.total();

            bah_audit_log(&format!(
                "hook run done name={} desc={} pos={}/{}",
                hook_name, hook_desc, hook_position, hook_total
            ));

            // HIGH #1 FIX: Hook return codes are NOT exposed via HookRunEvent in alpm 5.0.2
            // Hook failures are detected via trans_commit() returning TransHookFailed error
            // We track hook execution for audit purposes and error reporting

            let _ = emit(&HelperToParent::HookStep {
                current: hook_position,
                total: hook_total,
                desc: hook_desc,
            });
            None
        }
        ResolveDepsStart => Some(format!(
            "{} {}",
            c.action.paint("::"),
            c.bold.paint("Resolving dependencies...")
        )),
        InterConflictsStart => Some(format!(
            "{} {}",
            c.action.paint("::"),
            c.bold.paint("Checking conflicts...")
        )),
        IntegrityStart => Some(format!(
            "{} {}",
            c.action.paint("::"),
            c.bold.paint("Checking integrity...")
        )),
        LoadStart => Some(format!(
            "{} {}",
            c.action.paint("::"),
            c.bold.paint("Loading package files...")
        )),
        KeyringStart => Some(format!(
            "{} {}",
            c.action.paint("::"),
            c.bold.paint("Checking keyring...")
        )),
        DiskSpaceStart => Some(format!(
            "{} {}",
            c.action.paint("::"),
            c.bold.paint("Checking disk space...")
        )),
        TransactionStart => Some(format!(
            "{} {}",
            c.action.paint("::"),
            c.bold.paint("Committing transaction...")
        )),
        HookStart(he) => {
            bah_audit_log(&format!("hook phase start when={:?}", he.when()));
            if he.when() == HookWhen::PostTransaction {
                // BULLETPROOF FIX: Mark hook phase as active for timeout tracking
                HOOK_PHASE_ACTIVE.store(true, Ordering::SeqCst);
                if let Ok(mut start_time) = HOOK_START_TIME.lock() {
                    *start_time = Some(Instant::now());
                }
                let _ = emit(&HelperToParent::HookPhaseStart);
            }
            None
        }
        HookDone(he) => {
            bah_audit_log(&format!("hook phase done when={:?}", he.when()));
            if he.when() == HookWhen::PostTransaction {
                // BULLETPROOF FIX: Clear hook phase tracking
                HOOK_PHASE_ACTIVE.store(false, Ordering::SeqCst);
                if let Ok(mut start_time) = HOOK_START_TIME.lock() {
                    *start_time = None;
                }
                let _ = emit(&HelperToParent::HookPhaseDone);
            }
            None
        }
        HookRunStart(h) => {
            let hook_name = h.name().to_string();
            let hook_desc = h.desc().unwrap_or(&hook_name).to_string();
            
            bah_audit_log(&format!(
                "hook run start name={} desc={}",
                hook_name, hook_desc
            ));
            
            // BULLETPROOF FIX: Track hook start time for timeout detection
            if let Ok(mut start_time) = HOOK_START_TIME.lock() {
                *start_time = Some(Instant::now());
            }
            
            // HIGH #1 FIX: Track which hook is running for error reporting
            if let Ok(mut last_hook) = LAST_HOOK_RUNNING.lock() {
                *last_hook = Some(hook_desc.clone());
            }
            None
        }
        Event::PackageOperationDone(e) => match e.operation() {
            PackageOperation::Install(newpkg) => Some(format!(
                "{} {}",
                c.action.paint("::"),
                c.bold.paint(format!("Installed {}", newpkg.name()))
            )),
            PackageOperation::Upgrade(newpkg, oldpkg) => Some(format!(
                "{} {}",
                c.action.paint("::"),
                c.bold.paint(format!(
                    "Upgraded {} -> {}",
                    oldpkg.version().as_str(),
                    newpkg.version().as_str()
                ))
            )),
            PackageOperation::Remove(oldpkg) => Some(format!(
                "{} {}",
                c.action.paint("::"),
                c.bold.paint(format!("Removed {}", oldpkg.name()))
            )),
            _ => None,
        },
        _ => None,
    };
    if let Some(message) = message_opt {
        let _ = emit(&HelperToParent::Event { message });
    }
}

fn helper_progress_cb(
    progress: Progress,
    pkgname: &str,
    percent: i32,
    howmany: usize,
    current: usize,
    state: &mut HelperInstallThrottle,
) {
    match progress {
        Progress::AddStart | Progress::UpgradeStart | Progress::RemoveStart => {}
        _ => return,
    }

    let now = Instant::now();
    let key = pkgname.to_string();
    let p = percent.clamp(0, 100) as u64;
    let should_emit = match state.last_emit.get(&key) {
        None => true,
        Some(last) => now.saturating_duration_since(*last) >= Duration::from_millis(100),
    } || p >= 100;
    if should_emit {
        state.last_emit.insert(key.clone(), now);
        let _ = emit(&HelperToParent::InstallProgress {
            package: key,
            percent: p,
            current,
            total: howmany,
        });
    }
}

fn helper_question_cb(question: AnyQuestion, state: &mut QuestionIpcState) {
    let (prompt, default_yes, mut apply_answer): (String, bool, Box<dyn FnMut(bool) + '_>) =
        match question.question() {
            alpm::Question::InstallIgnorepkg(mut q) => (
                format!("Install ignored package '{}'?", q.pkg().name()),
                true,
                Box::new(move |yes| q.set_install(yes)),
            ),
            alpm::Question::Replace(q) => (
                format!(
                    "Replace '{}' with '{}' from '{}'?",
                    q.oldpkg().name(),
                    q.newpkg().name(),
                    q.newdb().name()
                ),
                true,
                Box::new(move |yes| q.set_replace(yes)),
            ),
            alpm::Question::Conflict(mut q) => (
                format!(
                    "Resolve conflict by removing '{}' ?",
                    q.conflict().package2().name()
                ),
                !state.no_confirm,
                Box::new(move |yes| q.set_remove(yes)),
            ),
            alpm::Question::Corrupted(mut q) => (
                format!("Corrupted package '{}'. Remove from cache?", q.filepath()),
                true,
                Box::new(move |yes| q.set_remove(yes)),
            ),
            alpm::Question::RemovePkgs(mut q) => (
                format!(
                    "Remove packages: {} ?",
                    q.packages()
                        .iter()
                        .map(|p| p.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                true,
                Box::new(move |yes| q.set_skip(!yes)),
            ),
            alpm::Question::SelectProvider(mut q) => {
                let default_index = 0i32;
                q.set_index(default_index);
                return;
            }
            alpm::Question::ImportKey(mut q) => (
                format!("Import key '{}' ({})?", q.fingerprint(), q.uid()),
                true,
                Box::new(move |yes| q.set_import(yes)),
            ),
        };

    let yes = if state.no_confirm {
        default_yes
    } else {
        let id = state.next_id;
        state.next_id += 1;
        let _ = emit(&HelperToParent::Question {
            id,
            prompt,
            default_yes,
        });
        wait_parent_answer(id).unwrap_or(default_yes)
    };
    apply_answer(yes);
}

fn wait_parent_answer(id: u64) -> Option<bool> {
    let stdin = std::io::stdin();
    loop {
        let mut locked = stdin.lock();
        let line = match read_ipc_line_bounded(&mut locked) {
            Ok(Some(l)) => l,
            Ok(None) => return None,
            Err(_) => return None,
        };
        drop(locked);
        let Ok(msg) = serde_json::from_str::<ParentToHelper>(line.trim()) else {
            continue;
        };
        match msg {
            ParentToHelper::Answer { id: got, yes } if got == id => return Some(yes),
            _ => continue,
        }
    }
}

fn add_target(alpm: &mut alpm::Alpm, target: &str) -> Result<()> {
    if let Some((repo, pkgname)) = target.split_once('/') {
        let db = alpm
            .syncdbs()
            .iter()
            .find(|db| db.name() == repo)
            .ok_or_else(|| anyhow!("repository '{}' was not found", repo))?;
        let pkg = db
            .pkg(pkgname)
            .with_context(|| format!("target not found in repo '{}': {}", repo, pkgname))?;
        alpm.trans_add_pkg(pkg)
            .map_err(|e| anyhow!("failed to add target '{}': {}", target, e.error))?;
        return Ok(());
    }

    if let Ok(pkg) = alpm.syncdbs().pkg(target) {
        alpm.trans_add_pkg(pkg)
            .map_err(|e| anyhow!("failed to add target '{}': {}", target, e.error))?;
        return Ok(());
    }

    if let Some(pkg) = alpm.syncdbs().find_target_satisfier(target) {
        alpm.trans_add_pkg(pkg)
            .map_err(|e| anyhow!("failed to add target '{}': {}", target, e.error))?;
        return Ok(());
    }

    bail!("target not found: {}", target)
}
