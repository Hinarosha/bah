use crate::backend;
use crate::devel::{load_devel_info, save_devel_info};
use crate::print_error;
use crate::repo;
use crate::search::interactive_search_local;
use crate::ui::{confirm_transaction, remove_confirmation_bundle, TxConfirmOp};
use crate::util::{collect_critical_pkgs, pkg_base_or_name};
use crate::Config;

use std::collections::HashMap;

use anyhow::{bail, Result};

pub fn remove(config: &mut Config) -> Result<i32> {
    if config.interactive {
        interactive_search_local(config)?;
    }

    let mut devel_info = load_devel_info(config)?.unwrap_or_default();
    let db = config.alpm.localdb();
    let bases = config
        .targets
        .iter()
        .filter_map(|pkg| db.pkg(pkg.as_str()).ok())
        .map(pkg_base_or_name)
        .collect::<Vec<_>>();

    let mut db_map: HashMap<String, Vec<String>> = HashMap::new();
    let (_, local_repos) = repo::repo_aur_dbs(config);
    for pkg in &config.targets {
        for db in &local_repos {
            if let Ok(pkg) = db.pkg(pkg.as_str()) {
                db_map
                    .entry(db.name().to_string())
                    .or_default()
                    .push(pkg.name().to_string());
            }
        }
    }

    let critical_pkgs = collect_critical_pkgs(config.targets.iter().map(|s| s.as_str()));
    if !critical_pkgs.is_empty() {
        crate::ui::print_critical_pkg_warning(&critical_pkgs);
    }
    if config.args.has_arg("noscriptlet", "noscriptlet")
        && !config.force_noscriptlet
        && !critical_pkgs.is_empty()
    {
        bail!(
            "--noscriptlet cannot be used with critical system packages ({}).\n       Post-install scriptlets are required for these packages to function correctly.",
            critical_pkgs.join(", ")
        );
    }

    if let Some((table, totals)) = remove_confirmation_bundle(config, &config.targets) {
        if !confirm_transaction(config, &table, &totals, TxConfirmOp::Remove) {
            return Ok(1);
        }
    }

    let mut ret = backend::pacman(config, &config.args)?.code();
    if ret != 0 {
        return Ok(ret);
    }

    let (_, dbs) = repo::repo_aur_dbs(config);

    for target in bases {
        devel_info.info.remove(target);
    }

    drop(dbs);

    if let Err(err) = save_devel_info(config, &devel_info) {
        print_error(config.color.error, err);
        ret = 1;
    }

    Ok(ret)
}
