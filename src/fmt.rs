use crate::config::Config;
use crate::repo;

use alpm::Ver;

use ansiterm::Style;
use chrono::{Local, TimeZone, Utc};
use tr::tr;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn opt(opt: &Option<String>) -> String {
    opt.clone().unwrap_or_else(|| tr!("None"))
}

pub fn date(date: i64) -> String {
    let dt = Utc
        .timestamp_opt(date, 0)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().expect("epoch is valid"))
        .with_timezone(&Local);
    dt.format("%a, %e %b %Y %T").to_string()
}

pub fn ymd(date: i64) -> String {
    let dt = Utc
        .timestamp_opt(date, 0)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().expect("epoch is valid"))
        .with_timezone(&Local);
    dt.format("%Y-%m-%d").to_string()
}

fn word_len(s: &str) -> usize {
    let mut len = 0;
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.by_ref().take_while(|c| c != &'m').count();
        } else {
            len += 1;
        }
    }

    len
}

pub fn print_indent<S: AsRef<str>>(
    color: Style,
    start: usize,
    indent: usize,
    cols: Option<usize>,
    sep: &str,
    value: impl IntoIterator<Item = S>,
) {
    let v = value.into_iter();

    match cols {
        Some(cols) if cols > indent + 2 => {
            let mut pos = start;

            let mut iter = v.peekable();

            if let Some(word) = iter.next() {
                print!("{}", color.paint(word.as_ref()));
                pos += word_len(word.as_ref());
            }

            if iter.peek().is_some() && pos + sep.len() < cols {
                print!("{}", sep);
                pos += sep.len();
            }

            while let Some(word) = iter.next() {
                let word = word.as_ref();
                let len = word_len(word);

                if pos + len > cols {
                    print!("\n{:>padding$}", "", padding = indent);
                    pos = indent;
                }

                print!("{}", color.paint(word));
                pos += len;

                if iter.peek().is_some() && pos + sep.len() < cols {
                    print!("{}", sep);
                    pos += sep.len();
                }
            }
        }
        _ => {
            let mut iter = v;
            if let Some(word) = iter.next() {
                print!("{}", color.paint(word.as_ref()));
            }

            for word in iter {
                print!("{}{}", sep, color.paint(word.as_ref()));
            }
        }
    }
    println!();
}

pub(crate) fn old_ver<'a>(config: &'a Config, pkg: &str) -> Option<&'a Ver> {
    let (_, dbs) = repo::repo_aur_dbs(config);

    if dbs.is_empty() {
        return config.alpm.localdb().pkg(pkg).ok().map(|p| p.version());
    }

    dbs.iter()
        .find_map(|db| db.pkg(pkg).ok())
        .map(|p| p.version())
}

use ansiterm::Color;

pub fn color_repo(enabled: bool, name: &str) -> String {
    if !enabled {
        return name.to_string();
    }

    let mut col: u32 = 5;

    for &b in name.as_bytes() {
        col = (b as u32).wrapping_add((col << 4).wrapping_add(col));
    }

    // Keep repo colors on ANSI TTY palette only (no 256-color fixed codes).
    let tty_palette = [
        Color::Blue,
        Color::Purple,
        Color::Cyan,
        Color::Green,
        Color::Yellow,
        Color::Red,
    ];
    let col = Style::from(tty_palette[(col as usize) % tty_palette.len()]).bold();
    col.paint(name).to_string()
}

pub fn print_target(targ: &str, quiet: bool) {
    if quiet {
        println!("{}", targ.split_once('/').unwrap().1);
    } else {
        println!("{}", targ);
    }
}

#[derive(Debug, Clone)]
pub struct ListRow {
    pub repository: String,
    pub name: String,
    pub version: String,
    pub status: String,
    pub description: String,
}

pub fn truncate_to_width(s: &str, max_width: usize) -> String {
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

pub fn print_section_header(config: &Config, title: &str) {
    let c = config.color;
    println!("{} {}", c.action.paint("::"), c.bold.paint(title));
}

pub fn print_dnf_like_rows(config: &Config, rows: &[ListRow]) {
    if rows.is_empty() {
        return;
    }

    let term_w = config.cols.unwrap_or(120).max(80);
    let col_gap = 2usize;
    let name_w = rows
        .iter()
        .map(|r| r.name.width())
        .max()
        .unwrap_or(4)
        .min(38);
    let ver_w = rows
        .iter()
        .map(|r| format!("{} {}", r.repository, r.version).width())
        .max()
        .unwrap_or(7)
        .min(34);
    let status_w = rows
        .iter()
        .map(|r| r.status.width())
        .max()
        .unwrap_or(6)
        .min(18);
    let fixed = name_w + ver_w + status_w + (col_gap * 3);
    let desc_w = term_w.saturating_sub(fixed).max(12);

    let c = config.color;
    let headers = (
        truncate_to_width(&tr!("Name"), name_w),
        truncate_to_width(&tr!("Repo Version"), ver_w),
        truncate_to_width(&tr!("Status"), status_w),
        truncate_to_width(&tr!("Description"), desc_w),
    );
    println!(
        "{}{:name_pad$}  {}{:ver_pad$}  {}{:status_pad$}  {}",
        c.field.paint(&headers.0),
        "",
        c.field.paint(&headers.1),
        "",
        c.field.paint(&headers.2),
        "",
        c.field.paint(&headers.3),
        name_pad = name_w.saturating_sub(headers.0.width()),
        ver_pad = ver_w.saturating_sub(headers.1.width()),
        status_pad = status_w.saturating_sub(headers.2.width()),
    );

    for row in rows {
        let name = truncate_to_width(&row.name, name_w);
        let rv = truncate_to_width(&format!("{} {}", row.repository, row.version), ver_w);
        let status = truncate_to_width(&row.status, status_w);
        let desc = truncate_to_width(&row.description, desc_w);

        println!(
            "{}{:name_pad$}  {}{:ver_pad$}  {}{:status_pad$}  {}",
            c.sl_pkg.paint(name),
            "",
            c.sl_repo.paint(rv),
            "",
            c.sl_installed.paint(status),
            "",
            c.install_version.paint(desc),
            name_pad = name_w.saturating_sub(row.name.width().min(name_w)),
            ver_pad = ver_w.saturating_sub(
                format!("{} {}", row.repository, row.version)
                    .width()
                    .min(ver_w)
            ),
            status_pad = status_w.saturating_sub(row.status.width().min(status_w)),
        );
    }
}
