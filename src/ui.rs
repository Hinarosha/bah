//! Structured terminal UI for pre-transaction confirmation and tabular previews.
//!
//! Build a [`ConfirmationTable`], pass per-column caps, then [`print_confirmation_table`].
//! Install flows use [`install_confirmation_bundle`] + [`print_install_confirmation_table`].

use std::collections::{HashMap, HashSet};
use std::fs::read_dir;
use std::io::{stdout, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::fmt::{old_ver, truncate_to_width};
use crate::info::get_terminal_width;
use crate::util::ask;
use alpm::Ver;
use aur_depends::{Actions, Base};
use tr::tr;
use unicode_width::UnicodeWidthStr;

const COL_GAP: &str = "  ";
const ANSI_CLEAR_LINE: &str = "\r\x1b[2K";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_GREY: &str = "\x1b[90m";
const ANSI_BOLD_BLUE: &str = "\x1b[1;34m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_RESET: &str = "\x1b[0m";
const PROGRESS_BAR_WIDTH: usize = 52;

/// PACMAN-like verb shown in the confirmation grid (system colors via [`Config`](crate::config::Config)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxActionKind {
    Install,
    Upgrade,
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxSourceKind {
    Repo,
    Aur,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxConfirmOp {
    Install,
    Update,
    Remove,
}

/// One line in the confirmation grid.
#[derive(Debug, Clone, Default)]
pub struct ConfirmationRow {
    pub cells: Vec<String>,
    /// When set, install-style painting applies to this row (action + version column).
    pub tx: Option<TxRowPaint>,
    /// Logical row group used for pre-transaction rendering.
    pub group: Option<TxRowGroup>,
}

/// Extra paint data for transaction rows (plain `cells` still drive column widths).
#[derive(Debug, Clone)]
pub struct TxRowPaint {
    pub action: TxActionKind,
    pub source: TxSourceKind,
    pub old_ver: String,
    pub new_ver: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxRowGroup {
    Target,
    Dependency,
}

/// Tabular confirmation (install/remove/update list) rendered DNF-like.
#[derive(Debug, Clone, Default)]
pub struct ConfirmationTable {
    pub title: String,
    pub headers: Vec<String>,
    pub rows: Vec<ConfirmationRow>,
}

impl ConfirmationTable {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Default::default()
        }
    }

    pub fn push_row(&mut self, cells: Vec<String>) {
        self.rows.push(ConfirmationRow {
            cells,
            tx: None,
            group: None,
        });
    }
}

/// Size totals for the summary line under the install preview table.
#[derive(Debug, Clone, Default)]
pub struct InstallConfirmTotals {
    /// Sum of known package download sizes (repo sync packages only).
    pub download_bytes: u64,
    /// True when AUR/pkgbuild rows are present (download total excludes those sources).
    pub download_excludes_aur: bool,
    /// Best-effort sum of installed-size delta for rows where ALPM reports `isize`.
    pub disk_delta_bytes: i64,
    /// True when at least one build row skipped disk delta (sizes unknown until build).
    pub disk_partial: bool,
    /// At least one repository package participates in disk delta maths.
    pub had_repo_pkg: bool,
}

fn pad_plain(s: &str, width: usize) -> String {
    let t = truncate_to_width(s, width);
    let wplain = t.width();
    if wplain >= width {
        return t;
    }
    format!("{}{}", t, " ".repeat(width.saturating_sub(wplain)))
}

fn base_widths(table: &ConfirmationTable, caps: &[usize]) -> Vec<usize> {
    let ncol = table.headers.len();
    let mut w = vec![4usize; ncol];
    for (i, h) in table.headers.iter().enumerate() {
        let cap = caps.get(i).copied().unwrap_or(56);
        w[i] = w[i].max(h.width()).min(cap);
    }
    for row in &table.rows {
        for (i, cell) in row.cells.iter().enumerate().take(ncol) {
            let cap = caps.get(i).copied().unwrap_or(56);
            w[i] = w[i].max(cell.width()).min(cap);
        }
    }
    w
}

/// `caps[i]` caps natural width; last column expands up to remaining terminal width (respecting `caps[last]` if set).
pub fn print_confirmation_table(config: &Config, table: &ConfirmationTable, caps: &[usize]) {
    if table.headers.is_empty() || table.rows.is_empty() {
        return;
    }
    let ncol = table.headers.len();
    if table.rows.iter().any(|r| r.cells.len() != ncol) {
        return;
    }

    let mut w = base_widths(table, caps);
    let term_w = config.cols.unwrap_or(120).max(72);
    let gaps = COL_GAP.len() * ncol.saturating_sub(1);
    let last_i = ncol.saturating_sub(1);
    let prefix_sum: usize = if ncol > 1 {
        w[..last_i].iter().copied().sum()
    } else {
        0
    };
    let slack = term_w.saturating_sub(prefix_sum + gaps).max(w[last_i]);
    let max_last = caps.get(last_i).copied().unwrap_or(256).max(w[last_i]);
    w[last_i] = slack.min(max_last);

    let c = &config.color;
    println!("{} {}", c.action.paint("::"), c.bold.paint(&table.title));

    let hdr: String = (0..ncol)
        .map(|i| {
            let cell = pad_plain(&table.headers[i], w[i]);
            if i + 1 < ncol {
                format!("{}{}", cell, COL_GAP)
            } else {
                cell
            }
        })
        .collect();
    println!("{}", c.field.paint(hdr));

    for row in &table.rows {
        let line: String = (0..ncol)
            .map(|i| {
                let cell = pad_plain(&row.cells[i], w[i]);
                let painted = match i {
                    0 => format!("{}", c.sl_pkg.paint(&cell)),
                    1 => format!("{}", c.old_version.paint(&cell)),
                    2 => format!("{}", c.new_version.paint(&cell)),
                    3 => format!("{}", c.install_version.paint(&cell)),
                    _ => format!("{}", c.field.paint(&cell)),
                };
                if i + 1 < ncol {
                    format!("{}{}", painted, COL_GAP)
                } else {
                    painted
                }
            })
            .collect();
        println!("{}", line.trim_end());
    }
}

fn v_label(ver: &str) -> String {
    let t = ver.trim();
    if t.is_empty() {
        return String::new();
    }
    if t.starts_with('v') {
        t.to_string()
    } else {
        format!("v{t}")
    }
}

fn version_plain(old: &str, new: &str) -> String {
    format!("{} -> {}", v_label(old), v_label(new))
}

fn version_plain_for_tx(txp: &TxRowPaint) -> String {
    match txp.action {
        TxActionKind::Remove => v_label(&txp.old_ver),
        TxActionKind::Install => v_label(&txp.new_ver),
        TxActionKind::Upgrade => version_plain(&txp.old_ver, &txp.new_ver),
    }
}

fn style_for_action(c: &crate::config::Colors, k: TxActionKind) -> ansiterm::Style {
    match k {
        TxActionKind::Install => c.tx_install,
        TxActionKind::Upgrade => c.tx_upgrade,
        TxActionKind::Remove => c.tx_remove,
    }
}

fn tx_group_title(k: TxRowGroup) -> String {
    match k {
        TxRowGroup::Target => tr!("Target Packages"),
        TxRowGroup::Dependency => tr!("Dependencies"),
    }
}

fn tx_source_title(k: TxSourceKind) -> String {
    match k {
        TxSourceKind::Aur => tr!("AUR Packages"),
        TxSourceKind::Repo => tr!("Repo Packages"),
    }
}

fn format_bytes_u64(n: u64) -> String {
    const K: u128 = 1024;
    let n = n as u128;
    if n < K {
        return format!("{n} B");
    }
    if n < K * K {
        return format!("{:.1} KiB", n as f64 / K as f64);
    }
    if n < K * K * K {
        return format!("{:.1} MiB", n as f64 / (K * K) as f64);
    }
    format!("{:.2} GiB", n as f64 / (K * K * K) as f64)
}

fn format_download_cell(n: Option<i64>) -> String {
    match n {
        None => tr!("Build"),
        Some(x) if x <= 0 => "—".to_string(),
        Some(x) => format_bytes_u64(x as u64),
    }
}

fn repo_tx_action(old: Option<&str>, new: &str) -> TxActionKind {
    match old {
        None | Some("") => TxActionKind::Install,
        Some(o) if o == new => TxActionKind::Install,
        Some(_) => TxActionKind::Upgrade,
    }
}

fn disk_delta_for_repo_pkg(config: &Config, sync_pkg: &alpm::Package) -> Option<i64> {
    let new_isize = sync_pkg.isize();
    match config.alpm.localdb().pkg(sync_pkg.name()) {
        Ok(local) => Some(new_isize - local.isize()),
        Err(_) => Some(new_isize),
    }
}

/// Builds a table + totals from resolved [`Actions`] (AUR/pkgbuild rows first, then repo packages).
pub fn install_confirmation_bundle<'a>(
    config: &Config,
    actions: &Actions<'a>,
    devel: &HashSet<String>,
) -> Option<(ConfirmationTable, InstallConfirmTotals)> {
    if actions.install.is_empty() && actions.build.is_empty() {
        return None;
    }

    let mut totals = InstallConfirmTotals::default();
    let mut rows_out: Vec<ConfirmationRow> = Vec::new();

    let pkg_h = tr!("Package");
    let ver_h = tr!("Version");
    let dl_h = tr!("Download");
    let detail_h = tr!("Details");

    for base in &actions.build {
        totals.download_excludes_aur = true;
        totals.disk_partial = true;
        match base {
            Base::Aur(base) => {
                for pkg in &base.pkgs {
                    let name = pkg.pkg.name.clone();
                    let new = if devel.contains(&name) {
                        "latest-commit".to_string()
                    } else {
                        pkg.pkg.version.clone()
                    };
                    let old = old_ver(config, &name)
                        .map(|v: &Ver| v.as_str().to_string())
                        .unwrap_or_default();
                    let action = repo_tx_action(
                        if old.is_empty() {
                            None
                        } else {
                            Some(old.as_str())
                        },
                        &new,
                    );
                    let ver_plain = match action {
                        TxActionKind::Install => v_label(&new),
                        _ => version_plain(&old, &new),
                    };
                    rows_out.push(ConfirmationRow {
                        cells: vec![
                            name,
                            ver_plain.clone(),
                            format_download_cell(aur_cached_pkg_size(config, &pkg.pkg.name, &new)),
                            pkg.pkg.description.clone().unwrap_or_default(),
                        ],
                        tx: Some(TxRowPaint {
                            action,
                            source: TxSourceKind::Aur,
                            old_ver: old,
                            new_ver: new,
                        }),
                        group: Some(if pkg.target {
                            TxRowGroup::Target
                        } else {
                            TxRowGroup::Dependency
                        }),
                    });
                }
            }
            Base::Pkgbuild(base) => {
                for pkg in &base.pkgs {
                    let name = pkg.pkg.pkgname.clone();
                    let new = if devel.contains(&name) {
                        "latest-commit".to_string()
                    } else {
                        base.srcinfo.version().to_string()
                    };
                    let old = old_ver(config, &name)
                        .map(|v: &Ver| v.as_str().to_string())
                        .unwrap_or_default();
                    let action = repo_tx_action(
                        if old.is_empty() {
                            None
                        } else {
                            Some(old.as_str())
                        },
                        &new,
                    );
                    let ver_plain = match action {
                        TxActionKind::Install => v_label(&new),
                        _ => version_plain(&old, &new),
                    };
                    rows_out.push(ConfirmationRow {
                        cells: vec![
                            name,
                            ver_plain.clone(),
                            format_download_cell(aur_cached_pkg_size(
                                config,
                                &pkg.pkg.pkgname,
                                &new,
                            )),
                            pkg.pkg.pkgdesc.clone().unwrap_or_default(),
                        ],
                        tx: Some(TxRowPaint {
                            action,
                            source: TxSourceKind::Aur,
                            old_ver: old,
                            new_ver: new,
                        }),
                        group: Some(if pkg.target {
                            TxRowGroup::Target
                        } else {
                            TxRowGroup::Dependency
                        }),
                    });
                }
            }
        }
    }

    let mut install = actions.install.clone();
    install.sort_by(|a, b| {
        a.pkg
            .name()
            .cmp(b.pkg.name())
            .then(a.pkg.db().unwrap().name().cmp(b.pkg.db().unwrap().name()))
    });

    for pkg in &install {
        let sync_pkg = pkg.pkg;
        let name = sync_pkg.name();
        let new = sync_pkg.version().as_str().to_string();
        let old = config
            .alpm
            .localdb()
            .pkg(name)
            .map(|p| p.version().as_str().to_string())
            .unwrap_or_default();
        let action = repo_tx_action(
            if old.is_empty() {
                None
            } else {
                Some(old.as_str())
            },
            &new,
        );
        let dsz = sync_pkg.download_size();
        let dl_cell = format_download_cell(Some(dsz));
        if dsz > 0 {
            totals.download_bytes = totals.download_bytes.saturating_add(dsz as u64);
        }
        totals.had_repo_pkg = true;
        if let Some(d) = disk_delta_for_repo_pkg(config, sync_pkg) {
            totals.disk_delta_bytes += d;
        }
        let ver_plain = match action {
            TxActionKind::Install => v_label(&new),
            _ => version_plain(&old, &new),
        };
        rows_out.push(ConfirmationRow {
            cells: vec![
                name.to_string(),
                ver_plain.clone(),
                dl_cell,
                latest_changelog_or_desc(sync_pkg),
            ],
            tx: Some(TxRowPaint {
                action,
                source: TxSourceKind::Repo,
                old_ver: old,
                new_ver: new,
            }),
            group: Some(if pkg.target {
                TxRowGroup::Target
            } else {
                TxRowGroup::Dependency
            }),
        });
    }

    let mut table = ConfirmationTable::new(tr!("Package changes"));
    table.headers = vec![pkg_h, ver_h, dl_h, detail_h];
    table.rows = rows_out;
    Some((table, totals))
}

pub fn remove_confirmation_bundle(
    config: &Config,
    targets: &[String],
) -> Option<(ConfirmationTable, InstallConfirmTotals)> {
    if targets.is_empty() {
        return None;
    }
    let db = config.alpm.localdb();
    let mut table = ConfirmationTable::new(tr!("Package changes"));
    table.headers = vec![tr!("Package"), tr!("Version"), tr!("Description")];
    let mut totals = InstallConfirmTotals::default();
    for target in targets {
        let Ok(pkg) = db.pkg(target.as_str()) else {
            continue;
        };
        totals.had_repo_pkg = true;
        totals.disk_delta_bytes -= pkg.isize();
        let old_ver = pkg.version().as_str().to_string();
        table.rows.push(ConfirmationRow {
            cells: vec![
                pkg.name().to_string(),
                v_label(&old_ver),
                pkg.desc().unwrap_or_default().to_string(),
            ],
            tx: Some(TxRowPaint {
                action: TxActionKind::Remove,
                source: TxSourceKind::Repo,
                old_ver,
                new_ver: String::new(),
            }),
            group: Some(TxRowGroup::Target),
        });
    }
    if table.rows.is_empty() {
        None
    } else {
        Some((table, totals))
    }
}

fn paint_version_cell(config: &Config, paint: &TxRowPaint) -> String {
    let c = &config.color;
    if paint.action == TxActionKind::Remove {
        return format!("{}", c.old_version.paint(v_label(&paint.old_ver)));
    }
    if paint.action == TxActionKind::Install {
        return format!("{}", c.new_version.paint(v_label(&paint.new_ver)));
    }
    let o = v_label(&paint.old_ver);
    let n = v_label(&paint.new_ver);
    format!(
        "{}{}{}",
        c.old_version.paint(&o),
        c.tx_arrow.paint(" -> "),
        c.new_version.paint(&n),
    )
}

fn read_first_changelog_line(pkg: &alpm::Package) -> Option<String> {
    let mut cl = pkg.changelog().ok()?;
    let mut buf = vec![0u8; 2048];
    let n = std::io::Read::read(&mut cl, &mut buf).ok()?;
    if n == 0 {
        return None;
    }
    let text = String::from_utf8_lossy(&buf[..n]);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.to_string())
}

fn latest_changelog_or_desc(pkg: &alpm::Package) -> String {
    read_first_changelog_line(pkg).unwrap_or_else(|| pkg.desc().unwrap_or_default().to_string())
}

fn aur_cached_pkg_size(config: &Config, name: &str, ver: &str) -> Option<i64> {
    fn scan_dir(dir: &Path, prefix: &str) -> Option<i64> {
        let mut best = None;
        for entry in read_dir(dir).ok()? {
            let entry = entry.ok()?;
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if !file_name.starts_with(prefix) || !file_name.contains(".pkg.tar") {
                continue;
            }
            let len = entry.metadata().ok()?.len() as i64;
            best = Some(best.unwrap_or(0).max(len));
        }
        best
    }

    let prefix = format!("{name}-{ver}-");
    for d in &config.pacman.cache_dir {
        if let Some(sz) = scan_dir(Path::new(d), &prefix) {
            return Some(sz);
        }
    }
    None
}

fn section_separator_line(term_w: usize, title: &str) -> String {
    // Keep section headers visually grouped but move title toward the left with a small bar inset.
    const LEFT_INSET: &str = " --";
    let label = format!("{LEFT_INSET}{title}");
    if term_w <= label.width() {
        return truncate_to_width(&label, term_w.max(1));
    }
    let mut out = String::with_capacity(term_w);
    out.push_str(&label);
    let rem = term_w.saturating_sub(out.width());
    out.push_str(&"-".repeat(rem));
    out
}

fn tx_table_title(op: TxConfirmOp, count: usize) -> String {
    match op {
        TxConfirmOp::Install => tr!("Packages to install ({}):", count),
        TxConfirmOp::Update => tr!("Packages to update ({}):", count),
        TxConfirmOp::Remove => tr!("Packages to remove ({}):", count),
    }
}

fn paint_section_header(line: String) -> String {
    format!("{ANSI_BOLD_BLUE}{line}{ANSI_RESET}")
}

/// Renders the install preview (structured rows, right-aligned download column) and a compact totals line.
pub fn print_install_confirmation_table(
    config: &Config,
    table: &ConfirmationTable,
    totals: &InstallConfirmTotals,
    op: TxConfirmOp,
) {
    if table.headers.is_empty() || table.rows.is_empty() {
        return;
    }
    let ncol = table.headers.len();
    let has_size_col = ncol >= 4;
    if table.rows.iter().any(|r| r.cells.len() != ncol) {
        return;
    }

    let term_w = config
        .cols
        .or_else(get_terminal_width)
        .unwrap_or(120)
        .max(72)
        .saturating_sub(1);
    let col_gap = COL_GAP.len();

    let pkg_w = table
        .rows
        .iter()
        .map(|r| r.cells[0].width())
        .chain(Some(table.headers[0].width()))
        .max()
        .unwrap_or(7)
        .min(40);
    // Widest plain `v_old -> v_new` across rows (same idea as longest name column).
    let ver_natural = table
        .rows
        .iter()
        .filter_map(|r| r.tx.as_ref())
        .map(|txp| version_plain_for_tx(txp).width())
        .chain(Some(table.headers[1].width()))
        .max()
        .unwrap_or(7);
    let dl_w = if has_size_col {
        table
            .rows
            .iter()
            .map(|r| r.cells[2].width())
            .chain(Some(table.headers[2].width()))
            .max()
            .unwrap_or(8)
            .min(14)
    } else {
        0
    };

    // Horizontal budget for Version + Detail after Package, Download and gaps — same spirit as search.
    const MIN_DETAIL: usize = 12;
    let gaps_count = if has_size_col { 3 } else { 2 };
    let remainder = term_w.saturating_sub(pkg_w + dl_w + col_gap * gaps_count);
    let hdr_ver_width = table.headers[1].width();
    let mut ver_w = ver_natural.max(hdr_ver_width);
    if ver_w + MIN_DETAIL > remainder {
        ver_w = remainder.saturating_sub(MIN_DETAIL).max(hdr_ver_width);
    }
    let detail_w = remainder.saturating_sub(ver_w).max(MIN_DETAIL);

    let c = &config.color;
    // Description column should stay readable but visually secondary ("dark gray" effect) on TTY colors.
    let detail_style = ansiterm::Style::new().fg(ansiterm::Color::White).dimmed();
    println!();
    println!(
        "{} {}",
        c.action.paint("::"),
        c.bold.paint(tx_table_title(op, table.rows.len()))
    );

    const PKG_COL: usize = 0;
    let dl_col = if has_size_col { Some(2usize) } else { None };
    let detail_col = if has_size_col { 3usize } else { 2usize };

    let render_row = |row: &ConfirmationRow, txp: &TxRowPaint| {
        let pkg_plain = truncate_to_width(&row.cells[PKG_COL], pkg_w);
        let pkg_vis_w = pkg_plain.width();
        let pkg_cell = format!(
            "{}{}",
            pkg_plain,
            " ".repeat(pkg_w.saturating_sub(pkg_vis_w))
        );

        let ver_raw = version_plain_for_tx(txp);
        let ver_trunc = truncate_to_width(&ver_raw, ver_w);
        let ver_vis_w = ver_trunc.width();
        let ver_cell = format!(
            "{}{}",
            ver_trunc,
            " ".repeat(ver_w.saturating_sub(ver_vis_w))
        );
        let ver_styled = if ver_trunc == ver_raw {
            let painted = paint_version_cell(config, txp);
            let pad = ver_w.saturating_sub(ver_raw.width());
            format!("{}{}", painted, " ".repeat(pad))
        } else {
            format!("{}", c.field.paint(&ver_cell))
        };

        let dl_cell = dl_col.map(|idx| {
            let dl_plain = truncate_to_width(&row.cells[idx], dl_w);
            let dl_vis_w = dl_plain.width();
            format!("{}{}", " ".repeat(dl_w.saturating_sub(dl_vis_w)), dl_plain)
        });

        let detail_plain = truncate_to_width(&row.cells[detail_col], detail_w);
        let detail_vis_w = detail_plain.width();
        let detail_cell = format!(
            "{}{}",
            detail_plain,
            " ".repeat(detail_w.saturating_sub(detail_vis_w))
        );
        if let Some(dl) = dl_cell {
            println!(
                "{}{}{}{}{}{}{}",
                style_for_action(c, txp.action).paint(&pkg_cell),
                COL_GAP,
                ver_styled,
                COL_GAP,
                c.number_menu.paint(&dl),
                COL_GAP,
                detail_style.paint(&detail_cell)
            );
        } else {
            println!(
                "{}{}{}{}{}",
                style_for_action(c, txp.action).paint(&pkg_cell),
                COL_GAP,
                ver_styled,
                COL_GAP,
                detail_style.paint(&detail_cell)
            );
        }
    };

    let has_aur = table
        .rows
        .iter()
        .filter_map(|r| r.tx.as_ref())
        .any(|txp| txp.source == TxSourceKind::Aur);
    let has_repo = table
        .rows
        .iter()
        .filter_map(|r| r.tx.as_ref())
        .any(|txp| txp.source == TxSourceKind::Repo);
    let use_source_sections = has_aur && has_repo;

    if op == TxConfirmOp::Install {
        for group in [TxRowGroup::Target, TxRowGroup::Dependency] {
            let has_group_rows = table
                .rows
                .iter()
                .any(|r| r.group.is_some_and(|g| g == group));
            if !has_group_rows {
                continue;
            }
            println!(
                "{}",
                paint_section_header(section_separator_line(term_w, &tx_group_title(group)))
            );

            let group_has_aur = table.rows.iter().any(|r| {
                r.group.is_some_and(|g| g == group)
                    && r.tx.as_ref().is_some_and(|txp| txp.source == TxSourceKind::Aur)
            });
            let group_has_repo = table.rows.iter().any(|r| {
                r.group.is_some_and(|g| g == group)
                    && r.tx.as_ref().is_some_and(|txp| txp.source == TxSourceKind::Repo)
            });

            if use_source_sections && group_has_aur && group_has_repo {
                for source in [TxSourceKind::Aur, TxSourceKind::Repo] {
                    println!(
                        "{}",
                        paint_section_header(section_separator_line(term_w, &tx_source_title(source)))
                    );
                    for row in &table.rows {
                        let Some(txp) = row.tx.as_ref() else {
                            continue;
                        };
                        if row.group.is_some_and(|g| g == group) && txp.source == source {
                            render_row(row, txp);
                        }
                    }
                }
            } else {
                for row in &table.rows {
                    let Some(txp) = row.tx.as_ref() else {
                        continue;
                    };
                    if row.group.is_some_and(|g| g == group) {
                        render_row(row, txp);
                    }
                }
            }
        }
    } else if use_source_sections {
        for source in [TxSourceKind::Aur, TxSourceKind::Repo] {
            println!(
                "{}",
                paint_section_header(section_separator_line(term_w, &tx_source_title(source)))
            );
            for row in &table.rows {
                let Some(txp) = row.tx.as_ref() else {
                    continue;
                };
                if txp.source == source {
                    render_row(row, txp);
                }
            }
        }
    } else {
        for row in &table.rows {
            let Some(txp) = row.tx.as_ref() else {
                continue;
            };
            render_row(row, txp);
        }
    }

    if op == TxConfirmOp::Install || op == TxConfirmOp::Update {
        print_install_confirmation_summary(config, totals);
    }
}

/// Second line after the grid: download + disk deltas, compact and easy to scan.
pub fn print_install_confirmation_summary(config: &Config, totals: &InstallConfirmTotals) {
    let c = &config.color;
    println!();
    let dl_s = if totals.download_bytes == 0 && !totals.download_excludes_aur {
        tr!("0 B")
    } else if totals.download_bytes == 0 && totals.download_excludes_aur {
        tr!("— (AUR)")
    } else {
        let mut s = format_bytes_u64(totals.download_bytes);
        if totals.download_excludes_aur {
            s.push('*');
        }
        s
    };
    let body = tr!("Total download size: {}", dl_s);
    println!("{} {}", c.action.paint("::"), c.bold.paint(&body));
    if totals.download_excludes_aur || totals.disk_partial {
        println!(
            "   {}",
            c.install_version.paint(tr!(
                "* AUR / local build sizes are applied when packages are built and installed."
            ))
        );
    }
    println!();
}

pub fn tx_confirm_prompt(op: TxConfirmOp) -> &'static str {
    match op {
        TxConfirmOp::Remove => "Proceed with removal?",
        TxConfirmOp::Update => "Proceed with update?",
        TxConfirmOp::Install => "Proceed with installation?",
    }
}

pub fn confirm_transaction(
    config: &Config,
    table: &ConfirmationTable,
    totals: &InstallConfirmTotals,
    op: TxConfirmOp,
) -> bool {
    print_install_confirmation_table(config, table, totals, op);
    ask(config, tx_confirm_prompt(op), true)
}

pub fn print_warning_block(lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    for line in lines {
        println!("{ANSI_YELLOW}{line}{ANSI_RESET}");
    }
    println!();
}

pub fn print_critical_pkg_warning(pkgs: &[String]) {
    if pkgs.is_empty() {
        return;
    }
    let list = pkgs.join(", ");
    let lines = vec![
        ":: Warning: This transaction includes critical system packages:".to_string(),
        format!("   {list}"),
        "   Interrupting this operation may leave your system unbootable.".to_string(),
        "   Ensure /boot is mounted before proceeding.".to_string(),
    ];
    print_warning_block(&lines);
}

fn ansi_ok() -> String {
    format!("{ANSI_GREEN}::{ANSI_RESET}")
}

fn format_speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1_000_000.0 {
        format!("{:.1}MB/s", bytes_per_sec / 1_000_000.0)
    } else if bytes_per_sec >= 1_000.0 {
        format!("{:.1}KB/s", bytes_per_sec / 1_000.0)
    } else {
        format!("{:.0}B/s", bytes_per_sec.max(0.0))
    }
}

fn format_time_remaining(bytes_remaining: u64, speed: f64) -> String {
    if bytes_remaining == 0 {
        return "0s".to_string();
    }
    if speed <= 0.0 {
        return "--".to_string();
    }
    let secs = (bytes_remaining as f64 / speed).ceil() as u64;
    if secs < 60 {
        format!("{}s", secs)
    } else {
        let mins = secs / 60;
        let secs = secs % 60;
        format!("{}m{}s", mins, secs)
    }
}

fn visible_width(s: &str) -> usize {
    let mut plain = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            let _ = chars.next();
            for code_ch in chars.by_ref() {
                if code_ch.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        plain.push(ch);
    }
    plain.as_str().width()
}

fn render_progress_bar(percent: u64, width: usize) -> String {
    let width = width.clamp(1, PROGRESS_BAR_WIDTH);
    let pct = percent.min(100) as usize;
    if pct >= 100 {
        let body = "=".repeat(width);
        return format!("[{ANSI_BOLD_BLUE}{body}{ANSI_RESET}]");
    }
    let pos = ((pct * width) / 100).min(width.saturating_sub(1));
    let filled = "=".repeat(pos);
    let empty = "-".repeat(width.saturating_sub(pos + 1));
    format!(
        "[{ANSI_BOLD_BLUE}{filled}➔{ANSI_GREY}{empty}{ANSI_RESET}]"
    )
}

fn split_download_filename(filename: &str) -> (String, String) {
    let plain = filename
        .trim_end_matches(".sig")
        .split('/')
        .next_back()
        .unwrap_or(filename);
    let stem = plain.split(".pkg.tar").next().unwrap_or(plain);
    let mut parts = stem.rsplitn(4, '-').collect::<Vec<_>>();
    if parts.len() == 4 {
        let name = parts.pop().unwrap_or(stem).to_string();
        return (name, plain.to_string());
    }
    (stem.to_string(), plain.to_string())
}

fn download_percent(downloaded: u64, total: u64) -> u64 {
    if total == 0 {
        0
    } else {
        downloaded.saturating_mul(100) / total
    }
    .min(100)
}

fn right_align_bar(left: &str, right: &str, terminal_width: usize) -> Option<String> {
    let left_width = visible_width(left);
    let right_width = visible_width(right);
    if terminal_width < left_width + 1 + right_width {
        return None;
    }
    let padding = terminal_width.saturating_sub(left_width + right_width);
    Some(format!("{left}{}{right}", " ".repeat(padding)))
}

fn truncate_filename_to_width(filename: &str, max_width: usize) -> String {
    if visible_width(filename) <= max_width {
        return filename.to_string();
    }
    truncate_to_width(filename, max_width)
}

fn render_download_line(
    filename: &str,
    downloaded: u64,
    total: u64,
    speed: f64,
    terminal_width: usize,
) -> String {
    let (name, file) = split_download_filename(filename);
    let percent = download_percent(downloaded, total);
    let remaining_bytes = total.saturating_sub(downloaded);
    let remaining = format_bytes_u64(remaining_bytes);
    let time = format_time_remaining(remaining_bytes, speed);
    let speed = format_speed(speed);

    let left_part = format!(
        "{ANSI_CYAN}Downloading{ANSI_RESET} {ANSI_BOLD}{name}{ANSI_RESET}..."
    );
    let mut file_part = format!("({file})");
    let percent_part = format!("{ANSI_BOLD}{percent}%{ANSI_RESET}");
    let right_base = format!(
        " {ANSI_YELLOW}[{remaining} left]{ANSI_RESET} {ANSI_YELLOW}{time}{ANSI_RESET} {ANSI_GREEN}{speed}{ANSI_RESET} {percent_part}",
    );

    let mut bar_width = PROGRESS_BAR_WIDTH;
    let mut right_part = format!(
        "{file_part}{right_base} {}",
        render_progress_bar(percent, bar_width)
    );
    while visible_width(&format!("{left_part}{right_part}")) > terminal_width {
        if bar_width > 20 {
            bar_width -= 1;
        } else {
            let fixed_width = visible_width(&format!(
                "{right_base} {}",
                render_progress_bar(percent, bar_width)
            ));
            let available_file = terminal_width.saturating_sub(visible_width(&left_part) + fixed_width + 1);
            if available_file == 0 {
                return format!("{} {}", left_part, render_progress_bar(percent, bar_width));
            }
            file_part = format!("({})", truncate_filename_to_width(&file, available_file.saturating_sub(2)));
            right_part = format!(
                "{file_part}{right_base} {}",
                render_progress_bar(percent, bar_width)
            );
            break;
        }
        right_part = format!(
            "{file_part}{right_base} {}",
            render_progress_bar(percent, bar_width)
        );
    }

    if let Some(line) = right_align_bar(&left_part, &right_part, terminal_width.max(1)) {
        return line;
    }

    format!("{} {}", left_part, render_progress_bar(percent, bar_width))
}

fn render_install_line(
    package: &str,
    percent: u64,
    current: usize,
    total: usize,
    terminal_width: usize,
) -> String {
    let percent = percent.min(100);
    let left = format!(
        "{ANSI_CYAN}Installing{ANSI_RESET} {ANSI_BOLD}{package}{ANSI_RESET}..."
    );
    let right_base = format!("{ANSI_GREY}{current}/{total}{ANSI_RESET} {percent}%");
    let mut bar_width = PROGRESS_BAR_WIDTH;

    loop {
        let right = format!("{right_base} {}", render_progress_bar(percent, bar_width));
        if let Some(line) = right_align_bar(&left, &right, terminal_width.max(1)) {
            return line;
        }
        if bar_width <= 12 {
            return format!("{left} {right}");
        }
        bar_width = bar_width.saturating_sub(1);
    }
}

/// Single-line ANSI transaction renderer.
pub struct TransactionRenderController {
    download_state: HashMap<String, DownloadState>,
    download_active_order: Vec<String>,
    download_frozen_order: Vec<String>,
    download_active_rendered: usize,
    download_frozen_rendered: usize,
    last_download_render: Option<Instant>,
    install_state: HashMap<String, InstallState>,
    install_active_order: Vec<String>,
    install_frozen_order: Vec<String>,
    install_active_rendered: usize,
    install_frozen_rendered: usize,
    last_install_render: Option<Instant>,
}

#[derive(Debug, Clone)]
struct DownloadState {
    downloaded: u64,
    total: u64,
    speed: f64,
    last_at: Instant,
    last_bytes: u64,
    frozen: bool,
}

#[derive(Debug, Clone)]
struct InstallState {
    percent: u64,
    current: usize,
    total: usize,
    frozen: bool,
}

impl TransactionRenderController {
    pub fn new() -> Self {
        Self {
            download_state: HashMap::new(),
            download_active_order: Vec::new(),
            download_frozen_order: Vec::new(),
            download_active_rendered: 0,
            download_frozen_rendered: 0,
            last_download_render: None,
            install_state: HashMap::new(),
            install_active_order: Vec::new(),
            install_frozen_order: Vec::new(),
            install_active_rendered: 0,
            install_frozen_rendered: 0,
            last_install_render: None,
        }
    }

    fn println(&mut self, msg: impl AsRef<str>) {
        println!("{}", msg.as_ref());
    }

    fn render_download_block(&mut self, force_render: bool) {
        let now = Instant::now();
        if !force_render {
            if let Some(last) = self.last_download_render {
                if now.saturating_duration_since(last) < Duration::from_millis(100) {
                    return;
                }
            }
        }

        let new_frozen = self
            .download_frozen_order
            .len()
            .saturating_sub(self.download_frozen_rendered);
        if new_frozen == 0 && self.download_active_order.is_empty() {
            return;
        }

        let term_w = get_terminal_width().unwrap_or(120).max(1);
        let mut out = String::new();
        if self.download_active_rendered > 0 {
            out.push_str(&format!("\x1b[{}A", self.download_active_rendered));
        }

        for key in self
            .download_frozen_order
            .iter()
            .skip(self.download_frozen_rendered)
        {
            if let Some(state) = self.download_state.get(key) {
                let line = render_download_line(
                    key,
                    state.downloaded,
                    state.total,
                    state.speed,
                    term_w,
                );
                out.push_str(ANSI_CLEAR_LINE);
                out.push_str(&line);
                out.push('\n');
            }
        }

        for key in &self.download_active_order {
            if let Some(state) = self.download_state.get(key) {
                let line = render_download_line(
                    key,
                    state.downloaded,
                    state.total,
                    state.speed,
                    term_w,
                );
                out.push_str(ANSI_CLEAR_LINE);
                out.push_str(&line);
                out.push('\n');
            }
        }

        if !out.is_empty() {
            print!("{out}");
            let _ = stdout().lock().flush();
        }

        self.download_frozen_rendered = self.download_frozen_order.len();
        self.download_active_rendered = self.download_active_order.len();
        self.last_download_render = Some(now);
    }

    fn render_install_block(&mut self, force_render: bool) {
        let now = Instant::now();
        if !force_render {
            if let Some(last) = self.last_install_render {
                if now.saturating_duration_since(last) < Duration::from_millis(100) {
                    return;
                }
            }
        }

        let new_frozen = self
            .install_frozen_order
            .len()
            .saturating_sub(self.install_frozen_rendered);
        if new_frozen == 0 && self.install_active_order.is_empty() {
            return;
        }

        let term_w = get_terminal_width().unwrap_or(120).max(1);
        let mut out = String::new();
        if self.install_active_rendered > 0 {
            out.push_str(&format!("\x1b[{}A", self.install_active_rendered));
        }

        for key in self
            .install_frozen_order
            .iter()
            .skip(self.install_frozen_rendered)
        {
            if let Some(state) = self.install_state.get(key) {
                let line = render_install_line(
                    key,
                    state.percent,
                    state.current,
                    state.total,
                    term_w,
                );
                out.push_str(ANSI_CLEAR_LINE);
                out.push_str(&line);
                out.push('\n');
            }
        }

        for key in &self.install_active_order {
            if let Some(state) = self.install_state.get(key) {
                let line = render_install_line(
                    key,
                    state.percent,
                    state.current,
                    state.total,
                    term_w,
                );
                out.push_str(ANSI_CLEAR_LINE);
                out.push_str(&line);
                out.push('\n');
            }
        }

        if !out.is_empty() {
            print!("{out}");
            let _ = stdout().lock().flush();
        }

        self.install_frozen_rendered = self.install_frozen_order.len();
        self.install_active_rendered = self.install_active_order.len();
        self.last_install_render = Some(now);
    }

    pub fn on_static_step(&mut self, msg: &str) {
        self.println(msg);
    }

    pub fn on_download_progress(&mut self, package: &str, downloaded: u64, total: u64) {
        if total == 0 || package.ends_with(".sig") {
            return;
        }
        let now = Instant::now();
        let key = package.to_string();
        let is_new = !self.download_state.contains_key(&key);
        if is_new {
            self.download_active_order.push(key.clone());
        }

        let state = self.download_state.entry(key.clone()).or_insert_with(|| DownloadState {
            downloaded: 0,
            total,
            speed: 0.0,
            last_at: now,
            last_bytes: downloaded,
            frozen: false,
        });

        let dt = now.saturating_duration_since(state.last_at).as_secs_f64();
        if dt > 0.0 && downloaded >= state.last_bytes {
            state.speed = (downloaded - state.last_bytes) as f64 / dt;
        }
        state.last_at = now;
        state.last_bytes = downloaded;
        state.downloaded = downloaded.min(total);
        state.total = total;

        let mut force_render = is_new;
        if downloaded >= total && !state.frozen {
            state.downloaded = total;
            state.speed = 0.0;
            state.frozen = true;
            if let Some(pos) = self
                .download_active_order
                .iter()
                .position(|k| k == &key)
            {
                self.download_active_order.remove(pos);
            }
            if !self.download_frozen_order.contains(&key) {
                self.download_frozen_order.push(key.clone());
            }
            force_render = true;
        }

        self.render_download_block(force_render);
    }

    pub fn on_install_progress(
        &mut self,
        package: &str,
        percent: u64,
        current: usize,
        total: usize,
    ) {
        let key = package.to_string();
        let is_new = !self.install_state.contains_key(&key);
        if is_new {
            self.install_active_order.push(key.clone());
        }

        let state = self.install_state.entry(key.clone()).or_insert_with(|| InstallState {
            percent: 0,
            current,
            total,
            frozen: false,
        });

        state.percent = percent.min(100);
        state.current = current;
        state.total = total;

        let mut force_render = is_new;
        if state.percent >= 100 && !state.frozen {
            state.frozen = true;
            if let Some(pos) = self
                .install_active_order
                .iter()
                .position(|k| k == &key)
            {
                self.install_active_order.remove(pos);
            }
            if !self.install_frozen_order.contains(&key) {
                self.install_frozen_order.push(key.clone());
            }
            force_render = true;
        }

        self.render_install_block(force_render);
    }

    pub fn on_log_line(&mut self, msg: &str) {
        self.println(msg);
    }

    pub fn on_hook_phase_start(&mut self, config: &Config) {
        let c = &config.color;
        self.on_log_line(&format!(
            "{} {}",
            c.action.paint("::"),
            c.bold.paint("Running post-transaction hooks...")
        ));
    }

    pub fn on_hook_step(&mut self, _config: &Config, current: usize, total: usize, desc: &str) {
        self.on_log_line(&format!("({}/{}) {} {}", current, total, desc, ansi_ok()));
    }

    pub fn on_hook_phase_done(&mut self, config: &Config) {
        let c = &config.color;
        self.on_log_line(&format!(
            "{} {}",
            c.action.paint("::"),
            c.bold.paint("Post-transaction hooks completed.")
        ));
    }

    pub fn finish(&mut self) {
    }
}

/// Phase 4 hooks/post-install text output (no progress bar).
pub fn print_hook_phase_start(config: &Config) {
    let c = &config.color;
    println!(
        "{} {}",
        c.action.paint("::"),
        c.bold.paint("Running post-transaction hooks...")
    );
}

pub fn print_hook_step(config: &Config, current: usize, total: usize, desc: &str) {
    let c = &config.color;
    println!(
        "{} {}",
        c.field.paint(format!("[{}/{}]", current, total)),
        c.install_version.paint(desc)
    );
}

pub fn print_hook_phase_done(config: &Config) {
    let c = &config.color;
    println!(
        "{} {}",
        c.action.paint("::"),
        c.bold.paint("Post-transaction hooks completed.")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_mismatched_columns() {
        let cfg = Config::new().expect("config");
        let mut t = ConfirmationTable::new("Test");
        t.headers = vec!["A".into(), "B".into()];
        t.push_row(vec!["only_one".into()]);
        print_confirmation_table(&cfg, &t, &[8, 8]);
    }

    #[test]
    fn prints_small_table() {
        let cfg = Config::new().expect("config");
        let mut t = ConfirmationTable::new("Proceed with installation?");
        t.headers = vec![
            "Package".into(),
            "Version".into(),
            "Repo".into(),
            "Notes".into(),
        ];
        t.push_row(vec![
            "nano".into(),
            "8.4-1 → 8.5-1".into(),
            "core".into(),
            "minor".into(),
        ]);
        print_confirmation_table(&cfg, &t, &[12, 20, 8, 64]);
    }

    #[test]
    fn transaction_prompts_match_operation() {
        assert_eq!(
            tx_confirm_prompt(TxConfirmOp::Install),
            "Proceed with installation?"
        );
        assert_eq!(
            tx_confirm_prompt(TxConfirmOp::Update),
            "Proceed with update?"
        );
        assert_eq!(
            tx_confirm_prompt(TxConfirmOp::Remove),
            "Proceed with removal?"
        );
    }

    #[test]
    fn progress_bar_body_never_exceeds_fixed_width() {
        for percent in [0, 1, 50, 67, 99, 100, 250] {
            let bar = render_progress_bar(percent, PROGRESS_BAR_WIDTH);
            let body = bar.trim_start_matches('[').trim_end_matches(']');
            assert_eq!(body.chars().count(), PROGRESS_BAR_WIDTH);
        }
    }

    #[test]
    fn download_progress_line_keeps_info_left_of_bar() {
        let line =
            render_download_line("vlc-3.0.21-1-x86_64.pkg.tar.zst", 67, 100, 1_200_000.0, 120);
        assert!(line.starts_with("Downloading \x1b[1mvlc\x1b[0m..."));
        assert!(!line.contains("[vlc]"));
        assert!(line.contains("(vlc-3.0.21-1-x86_64.pkg.tar.zst)"));
        assert!(line.contains("[33 B left]"));
        assert!(line.contains("1.2 MB/s  67%"));
        assert_eq!(visible_width(&line), 120);
        let bar = line.split_whitespace().last().expect("bar");
        let body = bar.trim_start_matches('[').trim_end_matches(']');
        assert_eq!(body.chars().count(), PROGRESS_BAR_WIDTH);
    }

    #[test]
    fn download_progress_line_compacts_on_narrow_terminal() {
        let line =
            render_download_line("cuda-13.2.1-2-x86_64.pkg.tar.zst", 1, 100, 1_200_000.0, 42);
        assert!(line.starts_with("\x1b[1mcuda\x1b[0m..."));
        assert!(!line.contains("(cuda-13.2.1-2-x86_64.pkg.tar.zst)"));
        assert!(visible_width(&line) <= 42);
        let bar = line.split_whitespace().last().expect("bar");
        let body = bar.trim_start_matches('[').trim_end_matches(']');
        assert!(body.chars().count() <= PROGRESS_BAR_WIDTH);
    }
}
