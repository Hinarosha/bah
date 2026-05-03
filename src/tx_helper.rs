use crate::config::{Colors, Config};
use crate::exec::Status;
use crate::fmt::truncate_to_width;
use crate::util::ask;

use alpm::{
    AnyEvent, AnyQuestion, CommitError, Error as AlpmError, Event, HookWhen, LogLevel, Progress,
    SigLevel, TransFlag,
};
use alpm_utils::DbListExt;
use alpm_sys::alpm_handle_t;
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
#[cfg(target_os = "linux")]
use nix::unistd::{faccessat, AccessFlags};
use nix::unistd::{dup, dup2_stdout, Uid};
use serde::{Deserialize, Serialize};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::fs::{read_to_string, remove_file, File};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Mutex;

const MAX_PLAN_BYTES: usize = 128 * 1024;
const MAX_TARGETS: usize = 4096;
const MAX_TARGET_LEN: usize = 4096;
const MAX_IPC_LINE_BYTES: usize = 16 * 1024;
const MAX_LOG_MESSAGE_BYTES: usize = 8192;

static HELPER_JSON_OUT: Mutex<Option<File>> = Mutex::new(None);

static ACTIVE_COMMIT_HANDLE: AtomicPtr<alpm_handle_t> = AtomicPtr::new(std::ptr::null_mut());
static SIG_THREAD_STARTED: AtomicBool = AtomicBool::new(false);
/// When false, the helper ignores SIGINT/SIGTERM for `alpm_trans_interrupt` so hooks (e.g. mkinitcpio) are not torn down mid-write.
static COMMIT_INTERRUPT_ALLOWED: AtomicBool = AtomicBool::new(true);

/// Persistent audit trail for `/boot` checks and commit lifecycle (helper runs as root).
const BAH_AUDIT_LOG_PATH: &str = "/var/log/bah.log";

/// Hardened PATH and removal of dynamic-loader overrides so pacman hooks (mkinitcpio, grub, etc.)
/// cannot be tricked into executing attacker-controlled code via `LD_PRELOAD` or a user-writable PATH entry.
#[cfg(target_os = "linux")]
fn sanitize_helper_environment() {
    let keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
    for k in keys {
        if k.starts_with("LD_") || k.starts_with("DYLD_") {
            std::env::remove_var(&k);
            continue;
        }
        match k.as_str() {
            "PERL5LIB" | "PYTHONPATH" | "RUBYLIB" | "NODE_PATH" => std::env::remove_var(&k),
            _ => {}
        }
    }
    std::env::set_var(
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    );
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
            let _ = writeln!(
                f,
                "[{}] [bah-helper:{}] {}",
                ts,
                std::process::id(),
                msg
            );
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

/// Wraps `trans_commit`: defers Ctrl+C interrupt requests until commit returns so scriptlets are not killed mid-write.
struct CommitInterruptDefer;

impl CommitInterruptDefer {
    fn new() -> Self {
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
    Progress {
        message: String,
    },
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

/// Makes JSON IPC lines go to the duplicated pipe fd; hooks and children inherit fd 1 as a copy of stderr (terminal), not the pipe.
fn install_ipc_json_sink() -> Result<()> {
    use std::io::{stderr, stdout};
    let ipc_fd = dup(stdout()).context("dup helper stdout for JSON IPC")?;
    dup2_stdout(stderr()).context("dup2 stderr onto stdout so hook children do not corrupt JSON")?;
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
    let buf =
        read_until_newline_limited(&mut locked, MAX_PLAN_BYTES).context("helper transaction plan")?;
    if buf.is_empty() {
        bail!("helper transaction plan is empty");
    }
    let plan: TransactionPlan = serde_json::from_slice(&buf)
        .context("failed to parse helper transaction plan JSON")?;
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
        TransactionPlan::SetInstallReason { reason, packages, .. } => {
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
    let mut child_stdin = child.stdin.take().context("failed to open helper stdin")?;
    let child_stdout = child.stdout.take().context("failed to open helper stdout")?;
    let mut reader = BufReader::new(child_stdout);

    serde_json::to_writer(&mut child_stdin, plan)
        .context("failed to serialize transaction plan to helper stdin")?;
    child_stdin
        .write_all(b"\n")
        .context("failed to write plan newline to helper")?;
    child_stdin
        .flush()
        .context("failed to flush transaction plan to helper")?;

    let mut final_code: Option<i32> = None;

    loop {
        let line = match read_ipc_line_bounded(&mut reader).context("read helper IPC line")? {
            Some(l) => l,
            None => break,
        };

        let msg: HelperToParent = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => {
                continue;
            }
        };

        match msg {
            HelperToParent::Event { message } => println!("{}", message),
            HelperToParent::Progress { message } => println!("{}", message),
            HelperToParent::LogLine { level, message } => {
                print_log_line_for_parent(config, &level, &message);
            }
            HelperToParent::Question {
                id,
                prompt,
                default_yes,
            } => {
                let yes = ask(config, &prompt, default_yes);
                serde_json::to_writer(&mut child_stdin, &ParentToHelper::Answer { id, yes })
                    .context("failed to write answer JSON to helper (broken pipe?)")?;
                child_stdin
                    .write_all(b"\n")
                    .context("failed to write answer newline to helper")?;
                child_stdin
                    .flush()
                    .context("failed to flush answer to helper")?;
            }
            HelperToParent::Result { code } => {
                final_code = Some(code);
                break;
            }
            HelperToParent::Error { message } => {
                bail!("helper error: {}", message);
            }
        }
    }

    let status = child.wait()?;
    // `status.code()` is None when the child was terminated by signal; use a non‑zero sentinel.
    let code = final_code.unwrap_or_else(|| status.code().unwrap_or(101));
    Ok(Status(code))
}

fn print_log_line_for_parent(config: &Config, level: &str, message: &str) {
    let c = &config.color;
    let prefix = match level {
        "error" => c.error.paint("[X]"),
        "warning" => c.warning.paint("[!]"),
        _ => c.field.paint("[·]"),
    };
    println!("{} {}", prefix, message);
}

fn ensure_sig_interrupt_thread_started() {
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
                    bah_audit_log(&format!(
                        "signal {sig}: requesting alpm_trans_interrupt"
                    ));
                    unsafe {
                        let _ = alpm_sys::alpm_trans_interrupt(p);
                    };
                }
            }
        });
}

struct ActiveCommitGuard;

impl ActiveCommitGuard {
    fn arm(alpm: &alpm::Alpm) -> Self {
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
    if code == AlpmError::TransHookFailed {
        s.push_str(
            " (transaction hook failed; see LogLine / ALPM log messages streamed above)",
        );
    } else if code == AlpmError::TransAbort {
        s.push_str(" (transaction aborted, often due to Ctrl+C)");
    }
    anyhow!(s)
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
    alpm.trans_add().iter().any(|p| pkg_requires_writable_boot(p.name()))
        || alpm
            .trans_remove()
            .iter()
            .any(|p| pkg_requires_writable_boot(p.name()))
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
fn ensure_boot_rw_for_transaction(alpm: &alpm::Alpm) -> Result<()> {
    if !transaction_touches_boot_sensitive_pkg(alpm) {
        bah_audit_log("ensure_boot_rw: skipped (no boot-sensitive packages in transaction)");
        return Ok(());
    }
    let boot: PathBuf = Path::new(alpm.root()).join("boot");
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

    let boot_fd = openat(
        &root_fd,
        "boot",
        OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
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
    faccessat(
        &boot_fd,
        ".",
        AccessFlags::W_OK,
        AtFlags::AT_EACCESS,
    )
    .with_context(|| {
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
    let vfs = fstatvfs(&boot_fd).with_context(|| format!("fstatvfs on {} failed", boot.display()))?;
    let fr = vfs.fragment_size() as u64;
    let avail = vfs.blocks_available() as u64;
    let avail_bytes = avail.saturating_mul(fr);
    const MIN_BOOT_FREE: u64 = 8 * 1024 * 1024;
    bah_audit_log(&format!(
        "ensure_boot_rw: statvfs avail_bytes={avail_bytes} (min_hint={MIN_BOOT_FREE})"
    ));
    if avail_bytes < MIN_BOOT_FREE {
        bah_audit_log(&format!(
            "ensure_boot_rw: FAIL low free space on {}",
            boot.display()
        ));
        bail!(
            "{} has critically low free space ({} bytes free; need at least ~{} MiB for kernel/initramfs hooks). Free space before upgrading.",
            boot.display(),
            avail_bytes,
            MIN_BOOT_FREE / (1024 * 1024)
        );
    }

    bah_audit_log(&format!(
        "ensure_boot_rw: OK {}",
        boot.display()
    ));
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn ensure_boot_rw_for_transaction(_alpm: &alpm::Alpm) -> Result<()> {
    Ok(())
}

fn execute_plan_root(config: &Config, plan: &TransactionPlan) -> Result<i32> {
    let mut alpm = config.new_alpm()?;
    // Route ALPM textual log (hooks, scriptlets detail) through JSON LogLine instead of stderr only.
    alpm.set_log_cb((), helper_log_cb);
    alpm.set_event_cb(config.color, helper_event_cb);
    alpm.set_progress_cb(config.color, helper_progress_cb);

    let no_confirm = match plan {
        TransactionPlan::Sync { no_confirm, .. }
        | TransactionPlan::Remove { no_confirm, .. }
        | TransactionPlan::Upgrade { no_confirm, .. }
        | TransactionPlan::SetInstallReason { no_confirm, .. } => *no_confirm,
    };
    let qstate = QuestionIpcState {
        no_confirm,
        next_id: 1,
    };
    alpm.set_question_cb(qstate, helper_question_cb);
    let mut helper_qid = 10_000u64;

    match plan {
        TransactionPlan::SetInstallReason { reason, packages, .. } => {
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
                        handle_db_lock(no_confirm, &mut helper_qid)?;
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
                    handle_db_lock(no_confirm, &mut helper_qid)?;
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
                if let Err(e) = ensure_boot_rw_for_transaction(&alpm) {
                    let _ = alpm.trans_release();
                    return Err(e);
                }
                let commit_err = {
                    let _defer_int = CommitInterruptDefer::new();
                    let _guard = ActiveCommitGuard::arm(&alpm);
                    alpm.trans_commit().err()
                };
                if let Some(e) = commit_err {
                    let _ = alpm.trans_release();
                    let _ = alpm.unlock();
                    return Err(map_commit_error(e, "failed to commit ALPM sync transaction").into());
                }
            }
            let _ = alpm.trans_release();
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
                    handle_db_lock(no_confirm, &mut helper_qid)?;
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
                if let Err(e) = ensure_boot_rw_for_transaction(&alpm) {
                    let _ = alpm.trans_release();
                    return Err(e);
                }
                let commit_err = {
                    let _defer_int = CommitInterruptDefer::new();
                    let _guard = ActiveCommitGuard::arm(&alpm);
                    alpm.trans_commit().err()
                };
                if let Some(e) = commit_err {
                    let _ = alpm.trans_release();
                    let _ = alpm.unlock();
                    return Err(
                        map_commit_error(e, "failed to commit ALPM remove transaction").into(),
                    );
                }
            }
            let _ = alpm.trans_release();
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
                    handle_db_lock(no_confirm, &mut helper_qid)?;
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
                if let Err(e) = ensure_boot_rw_for_transaction(&alpm) {
                    let _ = alpm.trans_release();
                    return Err(e);
                }
                let commit_err = {
                    let _defer_int = CommitInterruptDefer::new();
                    let _guard = ActiveCommitGuard::arm(&alpm);
                    alpm.trans_commit().err()
                };
                if let Some(e) = commit_err {
                    let _ = alpm.trans_release();
                    let _ = alpm.unlock();
                    return Err(map_commit_error(
                        e,
                        "failed to commit ALPM upgrade (local pkg) transaction",
                    )
                    .into());
                }
            }
            let _ = alpm.trans_release();
            Ok(0)
        }
    }
}

fn handle_db_lock(no_confirm: bool, next_id: &mut u64) -> Result<()> {
    let lock = Path::new("/var/lib/pacman/db.lck");
    if !lock.exists() {
        return Ok(());
    }

    let pid = read_to_string(lock).ok().and_then(|s| s.trim().parse::<i32>().ok());
    if let Some(pid) = pid {
        if process_alive(pid) {
            bail!(
                "Pacman is already in use by running process {} (db lock is active)",
                pid
            );
        }
    }

    let prompt = match pid {
        Some(pid) => format!("Stale pacman lock detected (PID {}). Remove it and retry?", pid),
        None => "Stale pacman lock detected. Remove it and retry?".to_string(),
    };
    let yes = ask_helper_question(no_confirm, next_id, &prompt, false)?;
    if !yes {
        bail!("database lock present and removal was declined");
    }
    remove_file(lock).context("failed to remove stale pacman lock")?;
    emit(&HelperToParent::Event {
        message: "Removed stale /var/lib/pacman/db.lck lock; retrying transaction.".to_string(),
    })?;
    Ok(())
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

fn hook_when_label(w: HookWhen) -> &'static str {
    match w {
        HookWhen::PreTransaction => "pre-transaction",
        HookWhen::PostTransaction => "post-transaction",
    }
}

fn helper_event_cb(event: AnyEvent, c: &mut Colors) {
    use Event::*;
    let message_opt: Option<String> = match event.event() {
        ScriptletInfo(s) => {
            let line = sanitize_log_fragment(s.line());
            if !line.is_empty() {
                let _ = emit(&HelperToParent::LogLine {
                    level: "scriptlet".to_string(),
                    message: line,
                });
            }
            None
        }
        HookRunDone(_) => None,
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
            c.bold.paint("Checking package integrity...")
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
        HookStart(he) => Some(format!(
            "{} {} ({})",
            c.action.paint("::"),
            c.bold.paint("ALPM hooks"),
            hook_when_label(he.when())
        )),
        HookDone(he) => Some(format!(
            "{} {} ({}) finished",
            c.action.paint("::"),
            c.bold.paint("ALPM hooks"),
            hook_when_label(he.when())
        )),
        HookRunStart(h) => Some(format!(
            "{} {} [{}/{}] {}",
            c.action.paint("::"),
            c.bold.paint(h.name()),
            h.position(),
            h.total(),
            h.desc().unwrap_or("")
        )),
        PackageOperationStart(_) => Some(format!(
            "{} {}",
            c.action.paint("::"),
            c.bold.paint("Applying package operation...")
        )),
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
    c: &mut Colors,
) {
    if percent == 0 || percent == 50 || percent == 100 {
        let message = format!(
            "{} {} [{}/{}] {}% ({:?})",
            c.action.paint("::"),
            c.bold.paint(pkgname),
            current,
            howmany,
            percent,
            progress
        );
        let _ = emit(&HelperToParent::Progress { message });
    }
}

fn helper_question_cb(question: AnyQuestion, state: &mut QuestionIpcState) {
    let (prompt, default_yes, mut apply_answer): (
        String,
        bool,
        Box<dyn FnMut(bool) + '_>,
    ) = match question.question() {
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
        alpm
            .trans_add_pkg(pkg)
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
