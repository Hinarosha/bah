use std::path::{Path, PathBuf};

use crate::config::SortBy;
use crate::config::{Colors, Config, SortMode};
use crate::util::{input, NumberMenu};
use crate::{info, printtr};

use ansiterm::Style;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use anyhow::{ensure, Context, Result};
use flate2::read::GzDecoder;
use raur::{Raur, SearchBy};
use regex::RegexBuilder;
use regex::RegexSet;
use regex::escape as regex_escape;
use reqwest::get;
use srcinfo::Srcinfo;
use tr::tr;

#[derive(Debug)]
pub enum AnyPkg<'a> {
    RepoPkg(&'a alpm::Package),
    AurPkg(&'a raur::Package),
    Custom(&'a str, &'a Srcinfo, &'a srcinfo::Package),
}

/// One search result line; used to compute aligned column widths across all rows.
struct SearchRow {
    name: String,
    status: String,
    version: String,
    repo: String,
    description: String,
    verbose: SearchVerbose,
}

enum SearchVerbose {
    Alpm(Option<String>),
    Aur {
        home: Option<String>,
        package_base: String,
    },
    Pkgbuild(PathBuf),
}

impl SearchRow {
    fn left_plain_width(&self) -> usize {
        if self.status.is_empty() {
            self.name.width()
        } else {
            self.name.width() + SEARCH_COL_GAP + self.status.width()
        }
    }

    fn mid_plain_width(&self) -> usize {
        self.repo.width() + SEARCH_COL_GAP + self.version.width()
    }
}

fn search_row_from_alpm(config: &Config, pkg: &alpm::Package) -> SearchRow {
    let mut status = String::new();
    if let Ok(repo_pkg) = config.alpm.localdb().pkg(pkg.name()) {
        status.push_str(&if repo_pkg.version() != pkg.version() {
            tr!("[installed: {}]", repo_pkg.version())
        } else {
            tr!("[installed]")
        });
    }

    let mut desc = pkg.desc().unwrap_or_default().to_string();
    if !pkg.groups().is_empty() {
        let g = pkg
            .groups()
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",");
        desc.push_str(&format!(" ({})", g));
    }

    let verbose = SearchVerbose::Alpm(pkg.url().map(|u| u.to_string()));

    SearchRow {
        name: pkg.name().to_string(),
        status,
        version: pkg.version().as_str().to_string(),
        repo: pkg.db().unwrap().name().to_string(),
        description: desc,
        verbose,
    }
}

fn search_row_from_aur(config: &Config, pkg: &raur::Package) -> SearchRow {
    let mut status = String::new();
    if let Ok(repo_pkg) = config.alpm.localdb().pkg(&*pkg.name) {
        status.push_str(&if repo_pkg.version().as_str() != pkg.version {
            tr!("[installed: {}]", repo_pkg.version())
        } else {
            tr!("[installed]")
        });
    }
    if let Some(date) = pkg.out_of_date {
        status.push_str(&tr!(" ood:{}", crate::fmt::ymd(date)));
    }
    if pkg.maintainer.is_none() {
        status.push_str(&tr!(" [orphaned]"));
    }

    let none = tr!("None");
    let desc = pkg.description.as_deref().unwrap_or(&none).to_string();

    SearchRow {
        name: pkg.name.clone(),
        status,
        version: pkg.version.clone(),
        repo: "aur".to_string(),
        description: desc,
        verbose: SearchVerbose::Aur {
            home: pkg.url.clone(),
            package_base: pkg.package_base.clone(),
        },
    }
}

fn search_row_from_pkgbuild(
    config: &Config,
    repo: &str,
    srcinfo: &Srcinfo,
    pkg: &srcinfo::Package,
    path: &Path,
) -> SearchRow {
    let mut status = String::new();
    if let Ok(repo_pkg) = config.alpm.localdb().pkg(&*pkg.pkgname) {
        status.push_str(&if repo_pkg.version().as_str() != srcinfo.version() {
            tr!("[installed: {}]", repo_pkg.version())
        } else {
            tr!("[installed]")
        });
    }

    let none = tr!("None");
    let desc = pkg.pkgdesc.as_deref().unwrap_or(&none).to_string();

    SearchRow {
        name: pkg.pkgname.clone(),
        status,
        version: srcinfo.version().to_string(),
        repo: repo.to_string(),
        description: desc,
        verbose: SearchVerbose::Pkgbuild(path.to_path_buf()),
    }
}

/// Widths for name+status, repo+version, and description so every line lines up.
fn compute_search_columns(rows: &[SearchRow], term_w: usize) -> (usize, usize, usize) {
    let gaps = SEARCH_COL_GAP * 2;
    let min_desc = 8usize;
    let max_l = rows.iter().map(SearchRow::left_plain_width).max().unwrap_or(0);
    let max_m = rows.iter().map(SearchRow::mid_plain_width).max().unwrap_or(0);

    let mut w_l = max_l;
    let mut w_m = max_m;

    if w_l + w_m + gaps + min_desc > term_w {
        let budget = term_w.saturating_sub(gaps + min_desc);
        if w_l + w_m > budget && w_l + w_m > 0 {
            let scale = budget as f64 / (w_l + w_m) as f64;
            w_l = ((w_l as f64 * scale).floor() as usize).max(6);
            w_m = ((w_m as f64 * scale).floor() as usize).max(4);
        }
    }

    let desc_max = term_w.saturating_sub(w_l + w_m + gaps);
    (w_l, w_m, desc_max)
}

fn search_row_from_any_pkg<'a>(config: &Config, pkg: &AnyPkg<'a>) -> SearchRow {
    match pkg {
        AnyPkg::RepoPkg(p) => search_row_from_alpm(config, p),
        AnyPkg::AurPkg(p) => search_row_from_aur(config, p),
        AnyPkg::Custom(repo, base, p) => {
            let path = config
                .pkgbuild_repos
                .repo(repo)
                .unwrap()
                .base(config, &base.base.pkgbase)
                .unwrap()
                .path
                .clone();
            search_row_from_pkgbuild(config, repo, base, p, &path)
        }
    }
}

fn print_search_row(
    config: &Config,
    row: &SearchRow,
    w_left: usize,
    w_mid: usize,
    desc_max: usize,
    search_terms: &[String],
) {
    let c = config.color;
    let hi = c.action;
    let color_on = c.enabled;

    let name = &row.name;
    let status = &row.status;
    let repo = &row.repo;
    let version = &row.version;

    let name_cell = if row.left_plain_width() > w_left {
        let comb = if status.is_empty() {
            name.clone()
        } else {
            format!("{} {}", name, status)
        };
        let t = truncate_to_width(&comb, w_left);
        highlight_terms(&t, search_terms, c.ss_name, hi, color_on)
    } else {
        format_name_status_cell(&c, name, status, search_terms)
    };
    let left_vis = row.left_plain_width().min(w_left);
    let pad_l = w_left.saturating_sub(left_vis);
    let left_out = format!("{}{}", name_cell, " ".repeat(pad_l));

    let rv_cell = if row.mid_plain_width() > w_mid {
        let comb = format!("{} {}", repo, version);
        let t = truncate_to_width(&comb, w_mid);
        highlight_terms(&t, search_terms, c.ss_ver, hi, color_on)
    } else {
        format_repo_version_cell(&c, repo, version, search_terms)
    };
    let mid_vis = row.mid_plain_width().min(w_mid);
    let pad_m = w_mid.saturating_sub(mid_vis);
    let mid_out = format!("{}{}", rv_cell, " ".repeat(pad_m));

    let desc_plain = desc_one_line(&row.description);
    let desc_trunc = if desc_max == 0 {
        String::new()
    } else {
        truncate_to_width(&desc_plain, desc_max)
    };
    let desc_cell = highlight_terms(&desc_trunc, search_terms, c.install_version, hi, color_on);

    let sp = " ".repeat(SEARCH_COL_GAP);
    println!("{}{}{}{}{}", left_out, sp, mid_out, sp, desc_cell);
}

fn print_search_verbose(config: &Config, row: &SearchRow) {
    if config.args.count("s", "search") <= 1 {
        return;
    }
    let c = config.color;
    match &row.verbose {
        SearchVerbose::Alpm(Some(url)) => {
            info::print(c, 14, config.cols, "    URL", url);
        }
        SearchVerbose::Alpm(None) => {}
        SearchVerbose::Aur { home, package_base } => {
            if let Some(url) = home {
                info::print(c, 14, config.cols, "    URL", url);
            }
            let aur_url = format!("{}packages/{}", config.aur_url, package_base);
            info::print(c, 14, config.cols, "    AUR URL", aur_url.as_str());
        }
        SearchVerbose::Pkgbuild(path) => {
            info::print(c, 14, config.cols, "    Path", &path.display().to_string());
        }
    }
}

/// Single ASCII space between name block, repo/version block, and description.
const SEARCH_COL_GAP: usize = 1;

fn search_term_width(config: &Config, menu_prefix_len: usize) -> usize {
    config
        .cols
        .unwrap_or(80)
        .saturating_sub(menu_prefix_len)
        .max(48)
}

/// Highlights query terms like `dnf search` (substring matches, case-insensitive).
fn highlight_terms(text: &str, terms: &[String], base: Style, hi: Style, color_on: bool) -> String {
    let alts: Vec<String> = terms
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| regex_escape(t))
        .collect();
    if alts.is_empty() {
        return if color_on {
            base.paint(text).to_string()
        } else {
            text.to_string()
        };
    }
    let Ok(re) = RegexBuilder::new(&format!("({})", alts.join("|")))
        .case_insensitive(true)
        .build()
    else {
        return if color_on {
            base.paint(text).to_string()
        } else {
            text.to_string()
        };
    };

    if !color_on {
        let mut out = String::new();
        let mut last = 0;
        for m in re.find_iter(text) {
            out.push_str(&text[last..m.start()]);
            out.push_str(&Style::new().bold().paint(m.as_str()).to_string());
            last = m.end();
        }
        out.push_str(&text[last..]);
        return out;
    }

    let mut out = String::new();
    let mut last = 0;
    for m in re.find_iter(text) {
        out.push_str(&base.paint(&text[last..m.start()]).to_string());
        out.push_str(&hi.paint(m.as_str()).to_string());
        last = m.end();
    }
    out.push_str(&base.paint(&text[last..]).to_string());
    out
}

fn truncate_to_width(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if s.width() <= max_width {
        return s.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max_width.saturating_sub(3) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push_str("...");
    out
}

fn desc_one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Package name (`ss_name`) and optional status (`ss_installed`); never truncated.
fn format_name_status_cell(c: &Colors, name: &str, status: &str, search_terms: &[String]) -> String {
    let hi = c.action;
    let color_on = c.enabled;
    if status.is_empty() {
        return highlight_terms(name, search_terms, c.ss_name, hi, color_on);
    }
    format!(
        "{} {}",
        highlight_terms(name, search_terms, c.ss_name, hi, color_on),
        highlight_terms(status, search_terms, c.ss_installed, hi, color_on)
    )
}

/// Repository then version (`sl_repo` + `ss_ver`); never truncated.
fn format_repo_version_cell(c: &Colors, repo: &str, version: &str, search_terms: &[String]) -> String {
    let hi = c.action;
    let color_on = c.enabled;
    format!(
        "{} {}",
        highlight_terms(repo, search_terms, c.sl_repo, hi, color_on),
        highlight_terms(version, search_terms, c.ss_ver, hi, color_on)
    )
}

pub async fn search(config: &Config) -> Result<i32> {
    let quiet = config.args.has_arg("q", "quiet");

    let repo_pkgs = search_repos(config, &config.targets)?;

    let targets = config
        .targets
        .iter()
        .map(|t| t.to_lowercase())
        .collect::<Vec<_>>();

    let custom_pkgs = search_pkgbuilds(config, &targets)?;

    let pkgs = search_aur(config, &targets)
        .await
        .context(tr!("aur search failed"))?;

    if quiet {
        if config.sort_mode == SortMode::TopDown {
            for pkg in &repo_pkgs {
                println!("{}", pkg.name());
            }
            for (repo, srcinfo, pkg) in &custom_pkgs {
                let _ = (repo, srcinfo);
                println!("{}", pkg.pkgname);
            }
            for pkg in &pkgs {
                println!("{}", pkg.name);
            }
        } else {
            for pkg in pkgs.iter().rev() {
                println!("{}", pkg.name);
            }
            for (repo, srcinfo, pkg) in &custom_pkgs {
                let _ = (repo, srcinfo);
                println!("{}", pkg.pkgname);
            }
            for pkg in repo_pkgs.iter().rev() {
                println!("{}", pkg.name());
            }
        }
        return Ok((repo_pkgs.is_empty() && pkgs.is_empty()) as i32);
    }

    let mut rows: Vec<SearchRow> = Vec::new();

    if config.sort_mode == SortMode::TopDown {
        for pkg in &repo_pkgs {
            rows.push(search_row_from_alpm(config, pkg));
        }
        for (repo, srcinfo, pkg) in &custom_pkgs {
            let path = config
                .pkgbuild_repos
                .repo(repo)
                .unwrap()
                .base(config, &srcinfo.base.pkgbase)
                .unwrap()
                .path
                .clone();
            rows.push(search_row_from_pkgbuild(config, repo, srcinfo, pkg, &path));
        }
        for pkg in &pkgs {
            rows.push(search_row_from_aur(config, pkg));
        }
    } else {
        for pkg in pkgs.iter().rev() {
            rows.push(search_row_from_aur(config, pkg));
        }
        for (repo, srcinfo, pkg) in &custom_pkgs {
            let path = config
                .pkgbuild_repos
                .repo(repo)
                .unwrap()
                .base(config, &srcinfo.base.pkgbase)
                .unwrap()
                .path
                .clone();
            rows.push(search_row_from_pkgbuild(config, repo, srcinfo, pkg, &path));
        }
        for pkg in repo_pkgs.iter().rev() {
            rows.push(search_row_from_alpm(config, pkg));
        }
    }

    let term_w = search_term_width(config, 0);
    let (w_l, w_m, desc_max) = compute_search_columns(&rows, term_w);

    for row in &rows {
        print_search_row(config, row, w_l, w_m, desc_max, &config.targets);
        print_search_verbose(config, row);
    }

    Ok((repo_pkgs.is_empty() && pkgs.is_empty()) as i32)
}

fn search_pkgbuilds<'a>(
    config: &'a Config,
    targets: &[String],
) -> Result<Vec<(&'a str, &'a Srcinfo, &'a srcinfo::Package)>> {
    if !config.mode.pkgbuild() {
        return Ok(Vec::new());
    }

    let regex = RegexSet::new(targets)?;
    let mut ret = Vec::new();

    for repo in &config.pkgbuild_repos.repos {
        for base in repo.pkgs(config) {
            let base = &base.srcinfo;
            for pkg in &base.pkgs {
                if targets.is_empty()
                    || regex.is_match(&base.base.pkgbase)
                    || regex.is_match(&pkg.pkgname)
                    || pkg.pkgdesc.iter().any(|d| regex.is_match(d))
                    || pkg
                        .provides
                        .iter()
                        .flat_map(|p| p.values())
                        .any(|p| regex.is_match(p))
                    || pkg.groups.iter().any(|g| regex.is_match(g))
                {
                    ret.push((repo.name.as_str(), base, pkg))
                }
            }
        }
    }

    Ok(ret)
}

fn search_local<'a>(config: &'a Config, targets: &[String]) -> Result<Vec<&'a alpm::Package>> {
    let mut ret = Vec::new();

    if targets.is_empty() {
        ret.extend(config.alpm.localdb().pkgs());
    } else {
        let pkgs = config.alpm.localdb().search(targets.iter())?;
        ret.extend(pkgs);
    };

    if config.limit != 0 {
        ret.truncate(config.limit);
    }

    Ok(ret)
}

fn search_repos<'a>(config: &'a Config, targets: &[String]) -> Result<Vec<&'a alpm::Package>> {
    if targets.is_empty() || !config.mode.repo() {
        return Ok(Vec::new());
    }

    let mut ret = Vec::new();

    for db in config.alpm.syncdbs() {
        let pkgs = db.search(targets.iter())?;
        ret.extend(pkgs);
    }

    if config.limit != 0 {
        ret.truncate(config.limit);
    }

    Ok(ret)
}

async fn search_target(config: &Config, targets: &mut Vec<String>) -> Result<Vec<raur::Package>> {
    let by = config.search_by;
    let mut pkgs = Ok(Vec::new());
    let mut index = 0;

    for (i, target) in targets.iter().enumerate() {
        index = i;
        pkgs = config.raur.search_by(target, by).await;
        if !matches!(pkgs, Err(raur::Error::Aur(_))) {
            break;
        }
    }

    if pkgs.is_ok() {
        targets.remove(index);
    }

    Ok(pkgs?)
}

async fn search_aur_regex(config: &Config, targets: &[String]) -> Result<Vec<raur::Package>> {
    let url = config.aur_url.join("packages.gz")?;
    let resp = get(url.clone())
        .await
        .with_context(|| format!("get {}", url))?;
    let success = resp.status().is_success();
    ensure!(success, "get {}: {}", url, resp.status());

    let data = resp.bytes().await?;
    let decoder = GzDecoder::new(&*data);
    let data =
        std::io::read_to_string(decoder).with_context(|| tr!("failed to decode package list"))?;

    let regex = RegexSet::new(targets)?;

    let pkgs = data
        .lines()
        .filter(|pkg| regex.is_match(pkg))
        .collect::<Vec<_>>();
    ensure!(pkgs.len() < 2000, "too many packages");
    let pkgs = config.raur.info(&pkgs).await?;
    Ok(pkgs)
}

async fn search_aur(config: &Config, targets: &[String]) -> Result<Vec<raur::Package>> {
    if targets.is_empty() || !config.mode.aur() {
        return Ok(Vec::new());
    }

    let mut matches = if config.args.has_arg("x", "regex") {
        search_aur_regex(config, targets).await?
    } else {
        let mut targets = targets.iter().map(|t| t.to_lowercase()).collect::<Vec<_>>();
        targets.sort_by_key(|t| t.len());

        let mut matches = Vec::new();

        let by = config.search_by;

        if by == SearchBy::NameDesc {
            let pkgs = search_target(config, &mut targets).await?;
            matches.extend(pkgs);
            matches.retain(|p| {
                let name = p.name.to_lowercase();
                let description = p
                    .description
                    .as_ref()
                    .map(|s| s.to_lowercase())
                    .unwrap_or_default();
                targets
                    .iter()
                    .all(|t| name.contains(t) | description.contains(t))
            });
        } else if by == SearchBy::Name {
            let pkgs = search_target(config, &mut targets).await?;
            matches.extend(pkgs);
            matches.retain(|p| targets.iter().all(|t| p.name.to_lowercase().contains(t)));
        } else {
            for target in targets {
                let pkgs = config.raur.search_by(target, by).await?;
                matches.extend(pkgs);
            }
        }

        matches
    };

    match config.sort_by {
        SortBy::Votes => matches.sort_by(|a, b| b.num_votes.cmp(&a.num_votes)),
        SortBy::Popularity => {
            matches.sort_by(|a, b| b.popularity.partial_cmp(&a.popularity).unwrap())
        }
        SortBy::Id => matches.sort_by_key(|p| p.id),
        SortBy::Name => matches.sort_by(|a, b| a.name.cmp(&b.name)),
        SortBy::Base => matches.sort_by(|a, b| a.package_base.cmp(&b.package_base)),
        SortBy::Submitted => matches.sort_by_key(|p| p.first_submitted),
        SortBy::Modified => matches.sort_by_key(|p| p.last_modified),
        _ => (),
    }

    if config.limit != 0 {
        matches.truncate(config.limit);
    }

    Ok(matches)
}

pub fn interactive_search_local(config: &mut Config) -> Result<()> {
    let mut all_pkgs = Vec::new();
    let repo_pkgs = search_local(config, &config.targets)?;

    for pkg in repo_pkgs {
        all_pkgs.push(AnyPkg::RepoPkg(pkg));
    }

    let was_results = all_pkgs.is_empty();
    let targs = interactive_menu(config, all_pkgs, false)?;
    if targs.is_empty() && !was_results {
        printtr!(" there is nothing to do");
    }
    config.targets = targs.clone();
    config.args.targets = targs;
    Ok(())
}

pub async fn interactive_search(config: &mut Config, install: bool) -> Result<()> {
    let repo_pkgs = search_repos(config, &config.targets)?;
    let custom_pkgs = search_pkgbuilds(config, &config.targets)?;
    let aur_pkgs = search_aur(config, &config.targets).await?;
    let mut all_pkgs = Vec::new();

    for pkg in repo_pkgs {
        all_pkgs.push(AnyPkg::RepoPkg(pkg));
    }
    for (repo, base, pkg) in custom_pkgs {
        all_pkgs.push(AnyPkg::Custom(repo, base, pkg));
    }
    for pkg in &aur_pkgs {
        all_pkgs.push(AnyPkg::AurPkg(pkg));
    }

    let was_results = all_pkgs.is_empty();
    let targs = interactive_menu(config, all_pkgs, install)?;
    if targs.is_empty() && !was_results {
        printtr!(" there is nothing to do");
    }
    config.targets = targs.clone();
    config.args.targets = targs;
    Ok(())
}

pub fn interactive_menu(
    config: &Config,
    mut all_pkgs: Vec<AnyPkg<'_>>,
    install: bool,
) -> Result<Vec<String>> {
    let pad = all_pkgs.len().to_string().len();

    if all_pkgs.is_empty() {
        printtr!("no packages match search");
        return Ok(Vec::new());
    }

    let indexes = all_pkgs
        .iter()
        .enumerate()
        .filter_map(|(n, pkg)| {
            let name = match pkg {
                AnyPkg::RepoPkg(pkg) => pkg.name(),
                AnyPkg::AurPkg(pkg) => pkg.name.as_str(),
                AnyPkg::Custom(_, _, pkg) => pkg.pkgname.as_str(),
            };

            if config.targets.iter().any(|targ| targ == name) {
                Some(n)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    for (i, n) in indexes.iter().rev().enumerate() {
        let pkg = all_pkgs.remove(i + n);
        all_pkgs.insert(0, pkg);
    }

    let rows: Vec<SearchRow> = all_pkgs
        .iter()
        .map(|p| search_row_from_any_pkg(config, p))
        .collect();
    let term_w = search_term_width(config, pad + 1);
    let (w_l, w_m, d_max) = compute_search_columns(&rows, term_w);

    if config.sort_mode == SortMode::TopDown {
        for n in 0..all_pkgs.len() {
            print_any_pkg(config, n, pad, &rows[n], w_l, w_m, d_max);
        }
    } else {
        for n in (0..all_pkgs.len()).rev() {
            print_any_pkg(config, n, pad, &rows[n], w_l, w_m, d_max);
        }
    }

    let input = if install {
        input(config, &tr!("Packages to install (eg: 1 2 3, 1-3):"))
    } else {
        input(config, &tr!("Select packages (eg: 1 2 3, 1-3):"))
    };

    if input.trim().is_empty() {
        return Ok(Vec::new());
    }

    let menu = NumberMenu::new(&input);
    let mut pkgs = Vec::new();

    if config.sort_mode == SortMode::TopDown {
        for (n, pkg) in all_pkgs.iter().enumerate() {
            if menu.contains(n + 1, "") {
                match pkg {
                    AnyPkg::RepoPkg(pkg) => {
                        pkgs.push(format!("{}/{}", pkg.db().unwrap().name(), pkg.name()))
                    }
                    AnyPkg::AurPkg(pkg) => {
                        pkgs.push(format!("{}/{}", config.aur_namespace(), pkg.name))
                    }
                    AnyPkg::Custom(repo, _, pkg) => pkgs.push(format!("{}/{}", repo, pkg.pkgname)),
                }
            }
        }
    } else {
        for (n, pkg) in all_pkgs.iter().enumerate().rev() {
            if menu.contains(n + 1, "") {
                match pkg {
                    AnyPkg::RepoPkg(pkg) => {
                        pkgs.push(format!("{}/{}", pkg.db().unwrap().name(), pkg.name()))
                    }
                    AnyPkg::AurPkg(pkg) => {
                        pkgs.push(format!("{}/{}", config.aur_namespace(), pkg.name))
                    }
                    AnyPkg::Custom(repo, _, pkg) => pkgs.push(format!("{}/{}", repo, pkg.pkgname)),
                }
            }
        }
    }

    Ok(pkgs)
}

fn print_any_pkg(
    config: &Config,
    n: usize,
    pad: usize,
    row: &SearchRow,
    w_l: usize,
    w_m: usize,
    d_max: usize,
) {
    let c = config.color;
    let num = format!("{:>pad$}", n + 1, pad = pad);
    print!("{} ", c.number_menu.paint(num));
    print_search_row(config, row, w_l, w_m, d_max, &config.targets);
    print_search_verbose(config, row);
}
