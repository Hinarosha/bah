#![allow(clippy::disallowed_methods)]

use crate::args::Args;
use crate::config::Config;

use std::ffi::OsStr;
use std::fmt::{Debug, Display, Formatter};
use std::path::Path;
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use log::debug;
use nix::libc;
use signal_hook::consts::signal::*;
use signal_hook::flag as signal_flag;
use std::sync::LazyLock;
use tr::tr;

pub static DEFAULT_SIGNALS: LazyLock<Arc<AtomicBool>> = LazyLock::new(|| {
    let arc = Arc::new(AtomicBool::new(true));
    // CRITICAL: Signal registration failures are non-recoverable - exit immediately
    // These unwrap() calls are safe because signal registration only fails for:
    // - Invalid signal numbers (impossible with consts)
    // - Handler installation errors (extremely rare, indicates broken system)
    if signal_flag::register_conditional_default(SIGTERM, Arc::clone(&arc)).is_err() {
        eprintln!("error: failed to register SIGTERM handler");
        std::process::exit(128 + SIGTERM as i32);
    }
    if signal_flag::register_conditional_default(SIGINT, Arc::clone(&arc)).is_err() {
        eprintln!("error: failed to register SIGINT handler");
        std::process::exit(128 + SIGINT as i32);
    }
    if signal_flag::register_conditional_default(SIGQUIT, Arc::clone(&arc)).is_err() {
        eprintln!("error: failed to register SIGQUIT handler");
        std::process::exit(128 + SIGQUIT as i32);
    }
    arc
});

static CAUGHT_SIGNAL: LazyLock<Arc<AtomicUsize>> = LazyLock::new(|| {
    let arc = Arc::new(AtomicUsize::new(0));
    if signal_flag::register_usize(SIGTERM, Arc::clone(&arc), SIGTERM as usize).is_err() {
        eprintln!("error: failed to register SIGTERM counter");
        std::process::exit(128 + SIGTERM as i32);
    }
    if signal_flag::register_usize(SIGINT, Arc::clone(&arc), SIGINT as usize).is_err() {
        eprintln!("error: failed to register SIGINT counter");
        std::process::exit(128 + SIGINT as i32);
    }
    if signal_flag::register_usize(SIGQUIT, Arc::clone(&arc), SIGQUIT as usize).is_err() {
        eprintln!("error: failed to register SIGQUIT counter");
        std::process::exit(128 + SIGQUIT as i32);
    }
    arc
});

pub static INTERRUPTED: LazyLock<Arc<AtomicBool>> = LazyLock::new(|| {
    let arc = Arc::new(AtomicBool::new(false));
    if signal_flag::register(SIGTERM, Arc::clone(&arc)).is_err() {
        eprintln!("error: failed to register SIGTERM interrupt flag");
        std::process::exit(128 + SIGTERM as i32);
    }
    if signal_flag::register(SIGINT, Arc::clone(&arc)).is_err() {
        eprintln!("error: failed to register SIGINT interrupt flag");
        std::process::exit(128 + SIGINT as i32);
    }
    if signal_flag::register(SIGQUIT, Arc::clone(&arc)).is_err() {
        eprintln!("error: failed to register SIGQUIT interrupt flag");
        std::process::exit(128 + SIGQUIT as i32);
    }
    arc
});

pub static RAISE_SIGPIPE: LazyLock<Arc<AtomicBool>> = LazyLock::new(|| {
    let arc = Arc::new(AtomicBool::new(true));
    if signal_flag::register_conditional_default(SIGPIPE, Arc::clone(&arc)).is_err() {
        eprintln!("error: failed to register SIGPIPE handler");
        std::process::exit(128 + SIGPIPE as i32);
    }
    arc
});

#[derive(Debug, Clone, Copy)]
pub struct Status(pub i32);

impl Display for Status {
    fn fmt(&self, _f: &mut Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

impl std::error::Error for Status {}

impl Status {
    pub fn code(self) -> i32 {
        self.0
    }

    pub fn success(self) -> Result<i32, Status> {
        if self.0 == 0 {
            Ok(0)
        } else {
            Err(self)
        }
    }
}

fn command_err(cmd: &Command) -> String {
    format!(
        "{} {} {}",
        tr!("failed to run:"),
        cmd.get_program().to_string_lossy(),
        cmd.get_args()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    )
}

pub fn command_status(cmd: &mut Command) -> Result<Status> {
    debug!("running command: {:?}", cmd);
    let term = &*CAUGHT_SIGNAL;

    DEFAULT_SIGNALS.store(false, Ordering::Relaxed);

    let ret = cmd
        .status()
        .map(|s| Status(s.code().unwrap_or(1)))
        .with_context(|| command_err(cmd));

    DEFAULT_SIGNALS.store(true, Ordering::Relaxed);

    match term.swap(0, Ordering::Relaxed) {
        0 => ret,
        n => std::process::exit(128 + n as i32),
    }
}

pub fn command(cmd: &mut Command) -> Result<()> {
    command_status(cmd)?
        .success()
        .with_context(|| command_err(cmd))?;
    Ok(())
}

pub fn command_output(cmd: &mut Command) -> Result<Output> {
    debug!("running command: {:?}", cmd);
    let term = &*CAUGHT_SIGNAL;

    DEFAULT_SIGNALS.store(false, Ordering::Relaxed);

    let ret = cmd.output().with_context(|| command_err(cmd));

    DEFAULT_SIGNALS.store(true, Ordering::Relaxed);
    let ret = match term.swap(0, Ordering::Relaxed) {
        0 => ret?,
        n => std::process::exit(128 + n as i32),
    };

    if !ret.status.success() {
        bail!(
            "{}: {}",
            command_err(cmd),
            String::from_utf8_lossy(&ret.stderr).trim()
        );
    }

    Ok(ret)
}

pub fn spawn(cmd: &mut Command) -> Result<Child> {
    debug!("running command: {:?}", cmd);
    cmd.spawn().with_context(|| command_err(cmd))
}

pub fn take_caught_signal() -> usize {
    (*CAUGHT_SIGNAL).swap(0, Ordering::SeqCst)
}

pub fn interrupt_received() -> bool {
    (*INTERRUPTED).load(Ordering::SeqCst)
}

#[cfg(unix)]
pub fn forward_sigint_to_pid(pid: u32) {
    unsafe {
        let _ = libc::kill(pid as libc::pid_t, libc::SIGINT);
    }
}

#[cfg(not(unix))]
pub fn forward_sigint_to_pid(_pid: u32) {}

pub fn wait(cmd: &Command, child: &mut Child) -> Result<Status> {
    let status = child
        .wait()
        .map(|s| Status(s.code().unwrap_or(1)))
        .with_context(|| command_err(cmd))?;
    Ok(status)
}

pub fn spawn_sudo(sudo: String, flags: Vec<String>) -> Result<()> {
    update_sudo(&sudo, &flags)?;
    thread::spawn(move || sudo_loop(&sudo, &flags));
    Ok(())
}

fn sudo_loop<S: AsRef<OsStr>>(sudo: &str, flags: &[S]) -> Result<()> {
    loop {
        thread::sleep(Duration::from_secs(250));
        update_sudo(sudo, flags)?;
    }
}

fn update_sudo<S: AsRef<OsStr>>(sudo: &str, flags: &[S]) -> Result<()> {
    let mut cmd = Command::new(sudo);
    cmd.args(flags);
    let status = command_status(&mut cmd)?;
    status.success()?;
    Ok(())
}

fn wait_for_lock(config: &Config) {
    let path = Path::new(config.alpm.dbpath()).join("db.lck");
    let c = config.color;
    if path.exists() {
        println!(
            "{} {}",
            c.error.paint("::"),
            c.bold
                .paint(tr!("Pacman is currently in use, please wait..."))
        );

        while path.exists() {
            std::thread::sleep(Duration::from_secs(3));
        }
    }
}

fn new_pacman<S: AsRef<str> + Display + Debug>(config: &Config, args: &Args<S>) -> Command {
    let mut cmd = if config.need_root {
        wait_for_lock(config);
        let mut cmd = Command::new(&config.sudo_bin);
        cmd.args(&config.sudo_flags).arg(args.bin.as_ref());
        cmd
    } else {
        Command::new(args.bin.as_ref())
    };

    if let Some(config) = &config.pacman_conf {
        cmd.args(["--config", config]);
    }
    cmd.args(args.args());
    cmd
}

pub fn pacman<S: AsRef<str> + Display + Debug>(config: &Config, args: &Args<S>) -> Result<Status> {
    let mut cmd = new_pacman(config, args);
    command_status(&mut cmd)
}

fn new_makepkg<S: AsRef<OsStr>>(
    config: &Config,
    dir: &Path,
    args: &[S],
    pkgdest: Option<&str>,
) -> Command {
    let mut cmd = Command::new(&config.makepkg_bin);
    if let Some(mconf) = &config.makepkg_conf {
        cmd.arg("--config").arg(mconf);
    }
    if let Some(dest) = pkgdest {
        cmd.env("PKGDEST", dest);
    }
    cmd.args(&config.mflags).args(args).current_dir(dir);
    cmd
}

pub fn makepkg_dest<S: AsRef<OsStr>>(
    config: &Config,
    dir: &Path,
    args: &[S],
    pkgdest: Option<&str>,
) -> Result<Status> {
    let mut cmd = new_makepkg(config, dir, args, pkgdest);
    command_status(&mut cmd)
}

pub fn makepkg<S: AsRef<OsStr>>(config: &Config, dir: &Path, args: &[S]) -> Result<Status> {
    makepkg_dest(config, dir, args, None)
}

pub fn makepkg_output_dest<S: AsRef<OsStr>>(
    config: &Config,
    dir: &Path,
    args: &[S],
    pkgdest: Option<&str>,
) -> Result<Output> {
    let mut cmd = new_makepkg(config, dir, args, pkgdest);
    command_output(&mut cmd)
}

pub fn makepkg_output<S: AsRef<OsStr>>(config: &Config, dir: &Path, args: &[S]) -> Result<Output> {
    makepkg_output_dest(config, dir, args, None)
}

pub fn has_command(name: &str) -> bool {
    Command::new(name).arg("--version").output().is_ok()
}
