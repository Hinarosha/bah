use crate::config::Config;
use crate::fmt::{print_dnf_like_rows, print_section_header, ListRow};
use crate::pkgbuild::PkgbuildRepos;
use crate::print_error;

use std::io::{Read, Write};

use anyhow::{anyhow, ensure, Context, Result};

use flate2::read::GzDecoder;
use raur::Raur;
use tr::tr;

pub async fn filter(config: &Config) -> Result<i32> {
    let mut cache = raur::Cache::new();
    config.raur.cache_info(&mut cache, &config.targets).await?;

    for targ in config.targets.iter().filter(|t| cache.contains(t.as_str())) {
        println!("{}", targ);
    }

    if cache.len() == config.targets.len() {
        Ok(0)
    } else {
        Ok(127)
    }
}

pub async fn list(config: &Config) -> Result<i32> {
    let c = config.color;
    let args = config.pacman_args();
    let mut ret = 0;

    if args.targets.is_empty() {
        if config.mode.repo() {
            list_repo_dbs(config, None);
        }
        if config.mode.pkgbuild() {
            for repo in &config.pkgbuild_repos.repos {
                list_pkgbuilds(config, &config.pkgbuild_repos, &repo.name);
            }
        }
        if config.mode.aur() {
            if let Err(e) = list_aur(config).await {
                print_error(c.error, e);
                ret = 1
            }
        }
    } else {
        for &target in &args.targets {
            if config.alpm.syncdbs().iter().any(|r| r.name() == target) && config.mode.repo() {
                list_repo_dbs(config, Some(target));
            } else if config.pkgbuild_repos.repo(target).is_some() && config.mode.pkgbuild() {
                list_pkgbuilds(config, &config.pkgbuild_repos, target);
            } else if target == config.aur_namespace() && config.mode.aur() {
                if let Err(e) = list_aur(config).await {
                    print_error(c.error, e);
                    ret = 1;
                }
            } else {
                print_error(c.error, anyhow!("repository \"{}\" was not found", target));
                ret = 1;
            }
        }
    }

    Ok(ret)
}

fn list_repo_dbs(config: &Config, only_repo: Option<&str>) {
    if !config.list {
        for db in config.alpm.syncdbs().iter() {
            if only_repo.is_some_and(|repo| repo != db.name()) {
                continue;
            }
            if config.quiet {
                println!("{}", db.name());
            } else {
                println!(
                    "{} {}",
                    db.name(),
                    db.servers()
                        .first()
                        .unwrap_or("")
                        .trim_start_matches("file://")
                );
            }
        }
        return;
    }

    let mut rows: Vec<ListRow> = Vec::new();

    for db in config.alpm.syncdbs().iter() {
        if only_repo.is_some_and(|repo| repo != db.name()) {
            continue;
        }
        for pkg in db.pkgs().iter() {
            if config.quiet {
                println!("{}", pkg.name());
            } else {
                let status = match config.alpm.localdb().pkg(pkg.name()) {
                    Ok(local_pkg) if local_pkg.version() != pkg.version() => {
                        tr!("installed: {}", local_pkg.version())
                    }
                    Ok(_) => tr!("installed"),
                    Err(_) => String::new(),
                };
                rows.push(ListRow {
                    repository: db.name().to_string(),
                    name: pkg.name().to_string(),
                    version: pkg.version().as_str().to_string(),
                    status,
                    description: pkg.desc().unwrap_or_default().to_string(),
                });
            }
        }
    }

    if !config.quiet {
        print_section_header(config, &tr!("Available repository packages"));
        print_dnf_like_rows(config, &rows);
    }
}

pub fn list_pkgbuilds(config: &Config, repos: &PkgbuildRepos, repo: &str) {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    if let Some(repo) = repos.repo(repo) {
        for pkg in repo.pkgs(config) {
            for name in pkg.srcinfo.pkgnames() {
                print_pkg(
                    config,
                    &mut stdout,
                    name.as_bytes(),
                    &repo.name,
                    &pkg.srcinfo.version(),
                )
            }
        }
    }
}

pub async fn list_aur(config: &Config) -> Result<()> {
    let url = config.aur_url.join("packages.gz")?;
    let client = config.raur.client();
    let resp = client
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("get {}", url))?;
    let success = resp.status().is_success();
    ensure!(success, "get {}: {}", url, resp.status());

    let data = resp.bytes().await?;
    let mut decoder = GzDecoder::new(&*data);
    let mut data = Vec::new();
    decoder
        .read_to_end(&mut data)
        .with_context(|| tr!("failed to decode package list"))?;

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    for line in data.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        print_pkg(config, &mut stdout, line, "aur", "unknown-version");
    }

    Ok(())
}

fn print_pkg(config: &Config, mut stdout: impl Write, line: &[u8], repo: &str, version: &str) {
    let cpkg = config.color.sl_pkg;
    let crepo = config.color.sl_repo;
    let cversion = config.color.sl_version;
    let cinstalled = config.color.sl_installed;

    if config.args.has_arg("q", "quiet") {
        let _ = stdout.write_all(line);
        let _ = stdout.write_all(b"\n");
        return;
    }
    let _ = crepo.paint(repo.as_bytes()).write_to(&mut stdout);
    let _ = stdout.write_all(b" ");
    let _ = cpkg.paint(line).write_to(&mut stdout);
    let _ = stdout.write_all(b" ");
    let _ = cversion.paint(version.as_bytes()).write_to(&mut stdout);

    if config.alpm.localdb().pkg(line).is_ok() {
        let _ = cinstalled
            .paint(tr!(" [installed]").as_bytes())
            .write_to(&mut stdout);
    }

    let _ = stdout.write_all(b"\n");
}
