//! Structured terminal UI for pre-transaction confirmation and tabular previews.
//!
//! Build a [`ConfirmationTable`], pass per-column caps, then [`print_confirmation_table`].

use crate::config::Config;
use crate::fmt::truncate_to_width;
use unicode_width::UnicodeWidthStr;

const COL_GAP: &str = "  ";

/// One line in the confirmation grid.
#[derive(Debug, Clone, Default)]
pub struct ConfirmationRow {
    pub cells: Vec<String>,
}

/// Tabular confirmation (install/remove/update list) rendered DNF-like.
///
/// Each row's `cells.len()` must match `headers.len()`.
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
        self.rows.push(ConfirmationRow { cells });
    }
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
