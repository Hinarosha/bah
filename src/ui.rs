//! Structured terminal UI for pre-transaction confirmation and tabular previews.
//!
//! Build a [`ConfirmationTable`], pass per-column caps, then [`print_confirmation_table`].
//! Install flows use [`install_confirmation_bundle`] + [`print_install_confirmation_table`].

use std::collections::HashSet;
use std::fs::read_dir;
use std::path::Path;

use crate::config::Config;
use crate::fmt::{old_ver, truncate_to_width};
use crate::info::get_terminal_width;
use alpm::Ver;
use aur_depends::{Actions, Base};
use tr::tr;
use unicode_width::UnicodeWidthStr;

const COL_GAP: &str = "  ";

/// PACMAN-like verb shown in the confirmation grid (system colors via [`Config`](crate::config::Config)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxActionKind {
    Install,
    Upgrade,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxSourceKind {
    Repo,
    Aur,
}

/// One line in the confirmation grid.
#[derive(Debug, Clone, Default)]
pub struct ConfirmationRow {
    pub cells: Vec<String>,
    /// When set, install-style painting applies to this row (action + version column).
    pub tx: Option<TxRowPaint>,
}

/// Extra paint data for transaction rows (plain `cells` still drive column widths).
#[derive(Debug, Clone)]
pub struct TxRowPaint {
    pub action: TxActionKind,
    pub source: TxSourceKind,
    pub old_ver: String,
    pub new_ver: String,
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

fn style_for_action(c: &crate::config::Colors, k: TxActionKind) -> ansiterm::Style {
    match k {
        TxActionKind::Install => c.tx_install,
        TxActionKind::Upgrade => c.tx_upgrade,
    }
}

fn tx_source_title(k: TxSourceKind) -> String {
    match k {
        TxSourceKind::Repo => tr!("Repository packages"),
        TxSourceKind::Aur => tr!("AUR / PKGBUILD packages"),
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

fn format_bytes_i64_signed(n: i64) -> String {
    let sign = if n > 0 { "+" } else { "" };
    format!("{sign}{}", format_bytes_u64(n.unsigned_abs()))
}

fn format_download_cell(n: Option<i64>) -> String {
    match n {
        None => "—".to_string(),
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

fn disk_delta_for_repo_pkg(
    config: &Config,
    sync_pkg: &alpm::Package,
) -> Option<i64> {
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
                        if old.is_empty() { None } else { Some(old.as_str()) },
                        &new,
                    );
                    let ver_plain = version_plain(&old, &new);
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
                        if old.is_empty() { None } else { Some(old.as_str()) },
                        &new,
                    );
                    let ver_plain = version_plain(&old, &new);
                    rows_out.push(ConfirmationRow {
                        cells: vec![
                            name,
                            ver_plain.clone(),
                            format_download_cell(aur_cached_pkg_size(config, &pkg.pkg.pkgname, &new)),
                            pkg.pkg.pkgdesc.clone().unwrap_or_default(),
                        ],
                        tx: Some(TxRowPaint {
                            action,
                            source: TxSourceKind::Aur,
                            old_ver: old,
                            new_ver: new,
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
            if old.is_empty() { None } else { Some(old.as_str()) },
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
        let ver_plain = version_plain(&old, &new);
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
        });
    }

    let mut table = ConfirmationTable::new(tr!("Package changes"));
    table.headers = vec![pkg_h, ver_h, dl_h, detail_h];
    table.rows = rows_out;
    Some((table, totals))
}

fn paint_version_cell(config: &Config, paint: &TxRowPaint) -> String {
    let c = &config.color;
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
    read_first_changelog_line(pkg)
        .unwrap_or_else(|| pkg.desc().unwrap_or_default().to_string())
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
    let label = format!(" {} ", title);
    if term_w <= label.width() + 2 {
        return truncate_to_width(&label, term_w.max(1));
    }
    let side = (term_w - label.width()) / 2;
    let mut out = String::with_capacity(term_w);
    out.push_str(&"─".repeat(side));
    out.push_str(&label);
    let rem = term_w.saturating_sub(out.width());
    out.push_str(&"─".repeat(rem));
    out
}

/// Renders the install preview (structured rows, right-aligned download column) and a compact totals line.
pub fn print_install_confirmation_table(
    config: &Config,
    table: &ConfirmationTable,
    totals: &InstallConfirmTotals,
) {
    if table.headers.is_empty() || table.rows.is_empty() {
        return;
    }
    let ncol = 4usize;
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
    let ver_w = table
        .rows
        .iter()
        .map(|r| r.cells[1].width())
        .chain(Some(table.headers[1].width()))
        .max()
        .unwrap_or(7)
        .min(34);
    let dl_w = table
        .rows
        .iter()
        .map(|r| r.cells[2].width())
        .chain(Some(table.headers[2].width()))
        .max()
        .unwrap_or(8)
        .min(14);
    let fixed = pkg_w + ver_w + dl_w + (col_gap * 3);
    let detail_w = term_w.saturating_sub(fixed).max(12);

    let c = &config.color;
    println!();
    println!("{} {}", c.action.paint("::"), c.bold.paint(&table.title));

    const PKG_COL: usize = 0;
    const VER_COL: usize = 1;
    const DL_COL: usize = 2;
    const DETAIL_COL: usize = 3;

    let hdr_pkg = truncate_to_width(&table.headers[PKG_COL], pkg_w);
    let hdr_ver = truncate_to_width(&table.headers[VER_COL], ver_w);
    let hdr_dl = truncate_to_width(&table.headers[DL_COL], dl_w);
    let hdr_det = truncate_to_width(&table.headers[DETAIL_COL], detail_w);
    let render_row = |row: &ConfirmationRow, txp: &TxRowPaint| {
        let pkg_plain = truncate_to_width(&row.cells[PKG_COL], pkg_w);
        let pkg_vis_w = pkg_plain.width();
        let pkg_cell = format!(
            "{}{}",
            pkg_plain,
            " ".repeat(pkg_w.saturating_sub(pkg_vis_w))
        );

        let ver_raw = version_plain(&txp.old_ver, &txp.new_ver);
        let ver_trunc = truncate_to_width(&ver_raw, ver_w);
        let ver_vis_w = ver_trunc.width();
        let ver_cell = format!(
            "{}{}",
            ver_trunc,
            " ".repeat(ver_w.saturating_sub(ver_vis_w))
        );
        let ver_styled = if ver_trunc == ver_raw {
            paint_version_cell(config, txp)
        } else {
            format!("{}", c.field.paint(&ver_cell))
        };

        let dl_plain = truncate_to_width(&row.cells[DL_COL], dl_w);
        let dl_vis_w = dl_plain.width();
        let dl_cell = format!(
            "{}{}",
            " ".repeat(dl_w.saturating_sub(dl_vis_w)),
            dl_plain
        );

        let detail_plain = truncate_to_width(&row.cells[DETAIL_COL], detail_w);
        let detail_vis_w = detail_plain.width();
        let detail_cell = format!(
            "{}{}",
            detail_plain,
            " ".repeat(detail_w.saturating_sub(detail_vis_w))
        );
        println!(
            "{}{}{}{}{}{}{}",
            style_for_action(c, txp.action).paint(&pkg_cell),
            COL_GAP,
            ver_styled,
            COL_GAP,
            c.stats_value.paint(&dl_cell),
            COL_GAP,
            c.install_version.paint(&detail_cell)
        );
    };

    for source in [TxSourceKind::Aur, TxSourceKind::Repo] {
        let has_rows = table.rows.iter().any(|r| r.tx.as_ref().is_some_and(|t| t.source == source));
        if !has_rows {
            continue;
        }
        println!(
            "{}",
            c.stats_line_separator
                .paint(section_separator_line(term_w, &tx_source_title(source)))
        );
        println!(
            "{}{:pkg_pad$}  {}{:ver_pad$}  {}{:dl_pad$}  {}",
            c.field.paint(&hdr_pkg),
            "",
            c.field.paint(&hdr_ver),
            "",
            c.field.paint(&hdr_dl),
            "",
            c.field.paint(&hdr_det),
            pkg_pad = pkg_w.saturating_sub(hdr_pkg.width()),
            ver_pad = ver_w.saturating_sub(hdr_ver.width()),
            dl_pad = dl_w.saturating_sub(hdr_dl.width()),
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

    print_install_confirmation_summary(config, totals);
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
    let disk_s = if !totals.had_repo_pkg && totals.disk_partial {
        format!("{}{}", tr!("—"), tr!(" *"))
    } else if totals.disk_partial {
        format!(
            "{}{}",
            format_bytes_i64_signed(totals.disk_delta_bytes),
            tr!(" *")
        )
    } else {
        format_bytes_i64_signed(totals.disk_delta_bytes)
    };
    let body = tr!("Total download: {} | Disk impact: {}", dl_s, disk_s);
    println!(
        "{} {}",
        c.action.paint("::"),
        c.bold.paint(&body)
    );
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
        t.headers = vec!["Package".into(), "Version".into(), "Repo".into(), "Notes".into()];
        t.push_row(vec![
            "nano".into(),
            "8.4-1 → 8.5-1".into(),
            "core".into(),
            "minor".into(),
        ]);
        print_confirmation_table(&cfg, &t, &[12, 20, 8, 64]);
    }
}