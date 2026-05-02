use crate::config::{Colors, Config};
use crate::exec::Status;
use crate::util::ask;

use alpm::{
    AnyEvent, AnyQuestion, CommitError, Error as AlpmError, Event, HookWhen, LogLevel, Progress,
    SigLevel, TransFlag,
};
use alpm_utils::DbListExt;
use alpm_sys::alpm_handle_t;
use anyhow::{anyhow, bail, Context, Result};
use nix::unistd::{dup, dup2_stdout, Uid};
use serde::{Deserialize, Serialize};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::fs::{read_to_string, remove_file, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
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

    let stdin = std::io::stdin();
    let mut locked = stdin.lock();
    let mut buf = Vec::new();
    locked
        .read_until(b'\n', &mut buf)
        .context("failed to read helper transaction plan from stdin")?;
    if buf.len() > MAX_PLAN_BYTES {
        bail!("helper transaction plan exceeded max size ({} bytes)", MAX_PLAN_BYTES);
    }
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
        }
        Ok(())
    };

    match plan {
        TransactionPlan::Sync { targets, .. }
        | TransactionPlan::Remove { targets, .. }
        | TransactionPlan::Upgrade { targets, .. } => validate_targets(targets),
        TransactionPlan::SetInstallReason { reason, packages, .. } => {
            if reason != "asdeps" && reason != "asexplicit" {
                bail!("unsupported install reason '{}'", reason);
            }
            validate_targets(packages)
        }
    }
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

    serde_json::to_writer(&mut child_stdin, plan)?;
    child_stdin.write_all(b"\n")?;
    child_stdin.flush()?;

    let mut line = String::new();
    let mut final_code: Option<i32> = None;

    while reader.read_line(&mut line)? != 0 {
        let msg: HelperToParent = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => {
                line.clear();
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
                serde_json::to_writer(&mut child_stdin, &ParentToHelper::Answer { id, yes })?;
                child_stdin.write_all(b"\n")?;
                child_stdin.flush()?;
            }
            HelperToParent::Result { code } => {
                final_code = Some(code);
                break;
            }
            HelperToParent::Error { message } => {
                bail!("helper error: {}", message);
            }
        }

        line.clear();
    }

    let status = child.wait()?;
    let code = final_code.unwrap_or_else(|| status.code().unwrap_or(1));
    Ok(Status(code))
}

fn print_log_line_for_parent(config: &Config, level: &str, message: &str) {
    let c = &config.color;
    let prefix = match level {
        "error" => c.error.paint("[alpm]"),
        "warning" => c.warning.paint("[alpm]"),
        _ => c.field.paint("[alpm]"),
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
            for _sig in signals.forever() {
                let p = ACTIVE_COMMIT_HANDLE.load(Ordering::Acquire);
                if !p.is_null() {
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

#[cfg(target_os = "linux")]
fn ensure_boot_rw_for_transaction(alpm: &alpm::Alpm) -> Result<()> {
    if !transaction_touches_boot_sensitive_pkg(alpm) {
        return Ok(());
    }
    let boot: PathBuf = Path::new(alpm.root()).join("boot");
    if !boot.exists() {
        bail!(
            "{} does not exist. Mount your EFI/boot partition before updating kernel, initramfs, or bootloader.",
            boot.display()
        );
    }

    let st = nix::sys::statfs::statfs(boot.as_path())
        .with_context(|| format!("statfs on {} failed (is /boot mounted?)", boot.display()))?;

    use nix::sys::statvfs::FsFlags;
    if st.flags().contains(FsFlags::ST_RDONLY) {
        bail!(
            "{} is read-only. Mount it read-write (check fstab) before updating kernel, initramfs, or bootloader so hooks can refresh images and bootloaders.",
            boot.display()
        );
    }

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
                let _guard = ActiveCommitGuard::arm(&alpm);
                let commit_err = match alpm.trans_commit() {
                    Ok(()) => None,
                    Err(e) => Some(e),
                };
                drop(_guard);
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
                let _guard = ActiveCommitGuard::arm(&alpm);
                let commit_err = match alpm.trans_commit() {
                    Ok(()) => None,
                    Err(e) => Some(e),
                };
                drop(_guard);
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
                let _guard = ActiveCommitGuard::arm(&alpm);
                let commit_err = match alpm.trans_commit() {
                    Ok(()) => None,
                    Err(e) => Some(e),
                };
                drop(_guard);
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
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line.len() > MAX_IPC_LINE_BYTES {
            return None;
        }
        let msg: ParentToHelper = serde_json::from_str(line.trim()).ok()?;
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
