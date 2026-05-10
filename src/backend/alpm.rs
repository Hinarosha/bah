use crate::args::Args;
use crate::backend::PackageBackend;
use crate::config::Config;
use crate::exec::Status;
use crate::tx_helper::{run_plan_with_helper, TransactionPlan};

use alpm::{AnyEvent, Event, PackageOperation, Progress, SigLevel, TransFlag};
use alpm_utils::DbListExt;
use anyhow::{anyhow, bail, Context, Result};
use nix::unistd::Uid;

#[derive(Debug, Default, Clone, Copy)]
pub struct AlpmBackend;

impl PackageBackend for AlpmBackend {
    fn pacman(&self, config: &Config, args: &Args<&str>) -> Result<Status> {
        if config.need_root && !Uid::effective().is_root() {
            let plan = plan_from_args(args, config.no_confirm)?;
            return run_plan_with_helper(config, &plan);
        }

        match args.op {
            "sync" => self.run_sync(config, args),
            "remove" => self.run_remove(config, args),
            "upgrade" => self.run_upgrade(config, args),
            _ => bail!("ALPM backend currently supports sync/remove/upgrade operations"),
        }
    }

}

impl AlpmBackend {
    fn run_sync(&self, config: &Config, args: &Args<&str>) -> Result<Status> {
        let mut alpm = config.new_alpm()?;
        set_runtime_callbacks(&alpm, config);

        if args.has_arg("y", "refresh") {
            let force = args.count("y", "refresh") > 1;
            alpm.syncdbs_mut().update(force)?;

            if !args.has_arg("u", "sysupgrade") && args.targets.is_empty() {
                return Ok(Status(0));
            }
        }

        let flags = trans_flags_for_sync(args);
        alpm.trans_init(flags)?;

        let result = (|| -> Result<Status> {
            if args.has_arg("u", "sysupgrade") {
                let enable_downgrade = args.count("u", "sysupgrade") > 1;
                alpm.sync_sysupgrade(enable_downgrade)?;
            }

            for target in &args.targets {
                add_target(&mut alpm, target)?;
            }

            alpm.trans_prepare()
                .map_err(|e| anyhow!("failed to prepare ALPM transaction: {}", e.error()))?;

            if args.has_arg("p", "print") {
                return Ok(Status(0));
            }

            alpm.trans_commit()
                .map_err(|e| anyhow!("failed to commit ALPM transaction: {}", e))?;
            Ok(Status(0))
        })();

        let _ = alpm.trans_release();
        result
    }

    fn run_remove(&self, config: &Config, args: &Args<&str>) -> Result<Status> {
        let mut alpm = config.new_alpm()?;
        set_runtime_callbacks(&alpm, config);
        let flags = trans_flags_for_remove(args);
        alpm.trans_init(flags)?;

        let result = (|| -> Result<Status> {
            for target in &args.targets {
                let pkg = alpm
                    .localdb()
                    .pkg(*target)
                    .with_context(|| format!("target not found in local db: {}", target))?;
                alpm.trans_remove_pkg(pkg)?;
            }

            alpm.trans_prepare()
                .map_err(|e| anyhow!("failed to prepare ALPM remove transaction: {}", e.error()))?;

            if args.has_arg("p", "print") {
                return Ok(Status(0));
            }

            alpm.trans_commit()
                .map_err(|e| anyhow!("failed to commit ALPM remove transaction: {}", e))?;
            Ok(Status(0))
        })();

        let _ = alpm.trans_release();
        result
    }

    fn run_upgrade(&self, config: &Config, args: &Args<&str>) -> Result<Status> {
        let mut alpm = config.new_alpm()?;
        set_runtime_callbacks(&alpm, config);
        let flags = trans_flags_for_upgrade(args);
        alpm.trans_init(flags)?;

        let result = (|| -> Result<Status> {
            for target in &args.targets {
                let loaded = alpm
                    .pkg_load(*target, true, SigLevel::NONE)
                    .with_context(|| format!("failed to load local package file '{}'", target))?;
                alpm.trans_add_pkg(loaded)
                    .map_err(|e| anyhow!("failed to add package file '{}': {}", target, e.error))?;
            }

            alpm.trans_prepare()
                .map_err(|e| anyhow!("failed to prepare ALPM upgrade transaction: {}", e.error()))?;

            if args.has_arg("p", "print") {
                return Ok(Status(0));
            }

            alpm.trans_commit()
                .map_err(|e| anyhow!("failed to commit ALPM upgrade transaction: {}", e))?;
            Ok(Status(0))
        })();

        let _ = alpm.trans_release();
        result
    }
}

fn set_runtime_callbacks(alpm: &alpm::Alpm, config: &Config) {
    // Keep question callback from Config (security prompts),
    // and add runtime event/progress callbacks for bah-controlled UI.
    alpm.set_event_cb(config.color, event_cb);
    alpm.set_progress_cb(config.color, progress_cb);
}

fn trans_flags_for_sync(args: &Args<&str>) -> TransFlag {
    let mut flags = TransFlag::NONE;
    if args.has_arg("w", "downloadonly") {
        flags |= TransFlag::DOWNLOAD_ONLY;
    }
    if args.has_arg("d", "nodeps") {
        flags |= TransFlag::NO_DEPS;
        if args.count("d", "nodeps") > 1 {
            flags |= TransFlag::NO_DEP_VERSION;
        }
    }
    if args.has_arg("needed", "needed") {
        flags |= TransFlag::NEEDED;
    }
    if args.has_arg("dbonly", "dbonly") {
        flags |= TransFlag::DB_ONLY;
    }
    if args.has_arg("noscriptlet", "noscriptlet") {
        flags |= TransFlag::NO_SCRIPTLET;
    }
    if args.has_arg("overwrite", "overwrite") {
        flags |= TransFlag::NO_CONFLICTS;
    }
    flags
}

fn trans_flags_for_remove(args: &Args<&str>) -> TransFlag {
    let mut flags = TransFlag::NONE;
    if args.has_arg("d", "nodeps") {
        flags |= TransFlag::NO_DEPS;
    }
    if args.has_arg("cascade", "cascade") {
        flags |= TransFlag::CASCADE;
    }
    if args.has_arg("nosave", "nosave") {
        flags |= TransFlag::NO_SAVE;
    }
    if args.has_arg("dbonly", "dbonly") {
        flags |= TransFlag::DB_ONLY;
    }
    if args.has_arg("noscriptlet", "noscriptlet") {
        flags |= TransFlag::NO_SCRIPTLET;
    }
    if args.has_arg("recursive", "recursive") {
        flags |= TransFlag::RECURSE;
        if args.count("recursive", "recursive") > 1 {
            flags |= TransFlag::RECURSE_ALL;
        }
    }
    if args.has_arg("unneeded", "unneeded") {
        flags |= TransFlag::UNNEEDED;
    }
    flags
}

fn trans_flags_for_upgrade(args: &Args<&str>) -> TransFlag {
    let mut flags = TransFlag::NONE;
    if args.has_arg("d", "nodeps") {
        flags |= TransFlag::NO_DEPS;
        if args.count("d", "nodeps") > 1 {
            flags |= TransFlag::NO_DEP_VERSION;
        }
    }
    if args.has_arg("dbonly", "dbonly") {
        flags |= TransFlag::DB_ONLY;
    }
    if args.has_arg("w", "downloadonly") {
        flags |= TransFlag::DOWNLOAD_ONLY;
    }
    if args.has_arg("needed", "needed") {
        flags |= TransFlag::NEEDED;
    }
    if args.has_arg("overwrite", "overwrite") {
        flags |= TransFlag::NO_CONFLICTS;
    }
    flags
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
        alpm
            .trans_add_pkg(pkg)
            .map_err(|e| anyhow!("failed to add target '{}': {}", target, e.error))?;
        return Ok(());
    }

    if let Some(pkg) = alpm.syncdbs().find_target_satisfier(target) {
        alpm
            .trans_add_pkg(pkg)
            .map_err(|e| anyhow!("failed to add target '{}': {}", target, e.error))?;
        return Ok(());
    }

    bail!("target not found: {}", target)
}

fn plan_from_args(args: &Args<&str>, no_confirm: bool) -> Result<TransactionPlan> {
    let plan = match args.op {
        "sync" => TransactionPlan::Sync {
            targets: args.targets.iter().map(|s| s.to_string()).collect(),
            refresh_count: args.count("y", "refresh") as u8,
            sysupgrade_count: args.count("u", "sysupgrade") as u8,
            download_only: args.has_arg("w", "downloadonly"),
            nodeps_count: args.count("d", "nodeps") as u8,
            needed: args.has_arg("needed", "needed"),
            db_only: args.has_arg("dbonly", "dbonly"),
            no_scriptlet: args.has_arg("noscriptlet", "noscriptlet"),
            overwrite: args.has_arg("overwrite", "overwrite"),
            print_only: args.has_arg("p", "print"),
            no_confirm,
        },
        "remove" => TransactionPlan::Remove {
            targets: args.targets.iter().map(|s| s.to_string()).collect(),
            nodeps: args.has_arg("d", "nodeps"),
            cascade: args.has_arg("cascade", "cascade"),
            no_save: args.has_arg("nosave", "nosave"),
            db_only: args.has_arg("dbonly", "dbonly"),
            no_scriptlet: args.has_arg("noscriptlet", "noscriptlet"),
            recursive_count: args.count("recursive", "recursive") as u8,
            unneeded: args.has_arg("unneeded", "unneeded"),
            print_only: args.has_arg("p", "print"),
            no_confirm,
        },
        "upgrade" => TransactionPlan::Upgrade {
            targets: args.targets.iter().map(|s| s.to_string()).collect(),
            nodeps_count: args.count("d", "nodeps") as u8,
            db_only: args.has_arg("dbonly", "dbonly"),
            download_only: args.has_arg("w", "downloadonly"),
            needed: args.has_arg("needed", "needed"),
            overwrite: args.has_arg("overwrite", "overwrite"),
            print_only: args.has_arg("p", "print"),
            no_confirm,
        },
        op => bail!("ALPM helper plan unsupported op '{}'", op),
    };
    Ok(plan)
}

fn event_cb(event: AnyEvent, c: &mut crate::config::Colors) {
    let msg: Option<String> = match event.event() {
        Event::ResolveDepsStart => Some("Resolving dependencies...".to_string()),
        Event::InterConflictsStart => Some("Checking conflicts...".to_string()),
        Event::IntegrityStart => Some("Checking integrity...".to_string()),
        Event::LoadStart => Some("Loading package files...".to_string()),
        Event::KeyringStart => Some("Checking keyring...".to_string()),
        Event::DiskSpaceStart => Some("Checking disk space...".to_string()),
        Event::TransactionStart => Some("Committing transaction...".to_string()),
        Event::HookStart(_) => Some("Running hooks...".to_string()),
        Event::PackageOperationDone(e) => match e.operation() {
            PackageOperation::Install(newpkg) => Some(format!(
                "{} Installed {}",
                c.tx_install.paint("[OK]"),
                newpkg.name()
            )),
            PackageOperation::Upgrade(newpkg, oldpkg) => Some(format!(
                "{} Upgraded {} -> {}",
                c.tx_install.paint("[OK]"),
                oldpkg.version().as_str(),
                newpkg.version().as_str()
            )),
            PackageOperation::Remove(oldpkg) => Some(format!(
                "{} Removed {}",
                c.tx_install.paint("[OK]"),
                oldpkg.name()
            )),
            _ => None,
        },
        _ => None,
    };

    if let Some(msg) = msg {
        println!("{} {}", c.action.paint("::"), c.bold.paint(msg));
    }
}

fn progress_cb(
    _progress: Progress,
    _pkgname: &str,
    _percent: i32,
    _howmany: usize,
    _current: usize,
    _c: &mut crate::config::Colors,
) {
    // Intentionally silent. event_cb handles all user-visible output.
    // progress_cb firing for Integrity, Keyring, Diskspace etc. caused repeated messages.
    let _ = _progress;
}
