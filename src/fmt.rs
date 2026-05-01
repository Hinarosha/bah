use std::collections::HashSet;

use crate::config::Config;
use crate::repo;

use alpm::Ver;
use aur_depends::{Actions, Base};

use ansiterm::Style;
use chrono::{Local, TimeZone, Utc};
use tr::tr;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn opt(opt: &Option<String>) -> String {
    opt.clone().unwrap_or_else(|| tr!("None"))
}

pub fn date(date: i64) -> String {
    let date = Utc.timestamp_opt(date, 0).unwrap().with_timezone(&Local);
    date.format("%a, %e %b %Y %T").to_string()
}

pub fn ymd(date: i64) -> String {
    let date = Utc.timestamp_opt(date, 0).unwrap().with_timezone(&Local);
    date.format("%Y-%m-%d").to_string()
}

pub fn link_str(enabled: bool, s: &str, url: &str) -> String {
    if enabled {
        format!("\x1b]8;;{url}\x1b\\{s}\x1b]8;;\x1b\\")
    } else {
        s.to_string()
    }
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

use ansiterm::Color;

pub fn color_repo(enabled: bool, name: &str) -> String {
    if !enabled {
        return name.to_string();
    }

    let mut col: u32 = 5;

    for &b in name.as_bytes() {
        col = (b as u32).wrapping_add((col << 4).wrapping_add(col));
    }

    col = (col % 6) + 9;
    let col = Style::from(Color::Fixed(col as u8)).bold();
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
    let name_w = rows.iter().map(|r| r.name.width()).max().unwrap_or(4).min(38);
    let ver_w = rows
        .iter()
        .map(|r| format!("{} {}", r.repository, r.version).width())
        .max()
        .unwrap_or(7)
        .min(34);
    let status_w = rows.iter().map(|r| r.status.width()).max().unwrap_or(6).min(18);
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
            ver_pad = ver_w.saturating_sub(format!("{} {}", row.repository, row.version).width().min(ver_w)),
            status_pad = status_w.saturating_sub(row.status.width().min(status_w)),
        );
    }
}

pub fn print_install(config: &Config, actions: &Actions, devel: &HashSet<String>) {
    let c = config.color;
    let db = config.alpm.localdb();

    struct UpgradeRow {
        name: String,
        old: String,
        new: String,
        summary: String,
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

    fn color_version_diff(config: &Config, old: &str, new: &str) -> String {
        let mut old_iter = old.chars();
        let mut new_iter = new.chars();
        let mut old_split = old_iter.clone();

        while let Some(old_c) = old_iter.next() {
            let new_c = match new_iter.next() {
                Some(c) => c,
                None => break,
            };

            if old_c != new_c {
                break;
            }

            if !old_c.is_alphanumeric() {
                old_split = old_iter.clone();
            }
        }

        let common = old.len().saturating_sub(old_split.as_str().len());
        let old_colored = format!(
            "{}{}",
            &old[..common],
            config.color.old_version.paint(&old[common..])
        );
        let new_colored = format!(
            "{}{}",
            &new[..common],
            config.color.new_version.paint(&new[common..])
        );
        format!("{old_colored} -> {new_colored}")
    }

    println!();
    let mut rows: Vec<UpgradeRow> = Vec::new();

    let mut install = actions.install.clone();
    install.sort_by(|a, b| {
        a.pkg.name()
            .cmp(b.pkg.name())
            .then(a.pkg.db().unwrap().name().cmp(b.pkg.db().unwrap().name()))
    });

    for pkg in &install {
        let name = pkg.pkg.name().to_string();
        let old = db
            .pkg(pkg.pkg.name())
            .map(|p| p.version().as_str().to_string())
            .unwrap_or_default();
        let new = pkg.pkg.version().as_str().to_string();
        let summary = pkg.pkg.desc().unwrap_or_default().to_string();
        rows.push(UpgradeRow {
            name,
            old,
            new,
            summary,
        });
    }

    for base in &actions.build {
        match base {
            Base::Aur(base) => {
                for pkg in &base.pkgs {
                    let new = if devel.contains(&pkg.pkg.name) {
                        "latest-commit".to_string()
                    } else {
                        pkg.pkg.version.clone()
                    };
                    rows.push(UpgradeRow {
                        name: pkg.pkg.name.clone(),
                        old: old_ver(config, &pkg.pkg.name)
                            .map(|v| v.as_str().to_string())
                            .unwrap_or_default(),
                        new,
                        summary: pkg.pkg.description.clone().unwrap_or_default(),
                    });
                }
            }
            Base::Pkgbuild(base) => {
                for pkg in &base.pkgs {
                    let new = if devel.contains(&pkg.pkg.pkgname) {
                        "latest-commit".to_string()
                    } else {
                        base.srcinfo.version().to_string()
                    };
                    rows.push(UpgradeRow {
                        name: pkg.pkg.pkgname.clone(),
                        old: old_ver(config, &pkg.pkg.pkgname)
                            .map(|v| v.as_str().to_string())
                            .unwrap_or_default(),
                        new,
                        summary: pkg.pkg.pkgdesc.clone().unwrap_or_default(),
                    });
                }
            }
        }
    }

    let name_w = rows
        .iter()
        .map(|r| r.name.width())
        .max()
        .unwrap_or_default()
        .max(1);
    let ver_w = rows
        .iter()
        .map(|r| format!("{} -> {}", r.old, r.new).width())
        .max()
        .unwrap_or_default()
        .max(1);
    let term_w = config.cols.unwrap_or(120).max(60);
    let desc_w = term_w.saturating_sub(name_w + 1 + ver_w + 1).max(8);

    for row in rows {
        let name = truncate_to_width(&row.name, name_w);
        let name_pad = " ".repeat(name_w.saturating_sub(name.width()));
        let ver_plain = truncate_to_width(&format!("{} -> {}", row.old, row.new), ver_w);
        let ver_pad = " ".repeat(ver_w.saturating_sub(ver_plain.width()));
        let summary = truncate_to_width(&row.summary, desc_w);
        let ver_colored = color_version_diff(config, &row.old, &row.new);

        println!(
            "{}{} {}{} {}",
            c.bold.paint(name),
            name_pad,
            ver_colored,
            ver_pad,
            c.install_version.paint(summary)
        );
    }

    println!();
}

fn repo<'a>(config: &'a Config, pkg: &str) -> &'a str {
    let (_, dbs) = repo::repo_aur_dbs(config);

    if dbs.is_empty() {
        return "aur";
    }

    let db = dbs
        .iter()
        .find(|db| db.pkg(pkg).is_ok())
        .map(|db| db.name())
        .unwrap_or_else(|| dbs.first().unwrap().name());

    db
}

fn old_ver<'a>(config: &'a Config, pkg: &str) -> Option<&'a Ver> {
    let (_, dbs) = repo::repo_aur_dbs(config);

    if dbs.is_empty() {
        return config.alpm.localdb().pkg(pkg).ok().map(|p| p.version());
    }

    dbs.iter()
        .find_map(|db| db.pkg(pkg).ok())
        .map(|p| p.version())
}

pub fn print_install_verbose(config: &Config, actions: &Actions, devel: &HashSet<String>) {
    let c = config.color;
    let bold = c.bold;
    let db = config.alpm.localdb();

    let package = tr!("Repo ({})", actions.install.len());
    let aur = match (
        actions.iter_aur_pkgs().count(),
        actions.iter_pkgbuilds().count(),
    ) {
        (a, 0) => format!("Aur ({})", a),
        (a, c) => format!("Pkgbuilds ({})", a + c),
    };
    let old = tr!("Old Version");
    let new = tr!("New Version");
    let make = tr!("Make Only");
    let yes = tr!("Yes");
    let no = tr!("No");

    let package_len = actions
        .install
        .iter()
        .map(|pkg| pkg.pkg.db().unwrap().name().len() + 1 + pkg.pkg.name().len())
        .chain(Some(package.width()))
        .max()
        .unwrap_or_default();

    let old_len = actions
        .install
        .iter()
        .filter_map(|pkg| db.pkg(pkg.pkg.name()).ok())
        .map(|pkg| pkg.version().len())
        .chain(Some(old.width()))
        .max()
        .unwrap_or_default();

    let new_len = actions
        .install
        .iter()
        .map(|pkg| pkg.pkg.version().len())
        .chain(Some(new.width()))
        .max()
        .unwrap_or_default();
    let new_len = new_len.max("latest-commit".len());

    let make_len = yes.width().max(no.width()).max(make.width());

    let aur_len = actions
        .build
        .iter()
        .filter_map(|pkg| match pkg {
            Base::Aur(base) => base
                .pkgs
                .iter()
                .map(|pkg| repo(config, &pkg.pkg.name).len() + 1 + pkg.pkg.name.len())
                .max(),
            Base::Pkgbuild(base) => base
                .pkgs
                .iter()
                .map(|pkg| base.repo.len() + 1 + pkg.pkg.pkgname.len())
                .max(),
        })
        .chain(Some(aur.width()))
        .max()
        .unwrap_or_default();

    let aur_old_len = actions
        .build
        .iter()
        .filter_map(|pkg| match pkg {
            Base::Aur(base) => base
                .pkgs
                .iter()
                .filter_map(|pkg| old_ver(config, &pkg.pkg.name))
                .map(|v| v.as_str())
                .max(),
            Base::Pkgbuild(base) => base
                .pkgs
                .iter()
                .filter_map(|pkg| old_ver(config, &pkg.pkg.pkgname))
                .map(|v| v.as_str())
                .max(),
        })
        .map(|v| v.len())
        .chain(Some(old.width()))
        .max()
        .unwrap_or_default();

    let aur_new_len = actions
        .build
        .iter()
        .map(|base| base.version().len())
        .chain(Some(new.width()))
        .max()
        .unwrap_or_default();

    let package_len = package_len.max(aur_len);
    let old_len = old_len.max(aur_old_len);
    let new_len = new_len.max(aur_new_len);

    if let Some(cols) = config.cols {
        if package_len + 2 + old_len + 2 + new_len + 2 + make_len > cols {
            eprintln!(
                "{} {}",
                c.warning.paint("::"),
                tr!("insufficient columns available for table display")
            );

            print_install(config, actions, devel);
            return;
        }
    }

    if !actions.install.is_empty() {
        println!();
        println!(
            "{}{:<package_len$}  {}{:<old_len$}  {}{:<new_len$}  {}",
            bold.paint(&package),
            "",
            bold.paint(&old),
            "",
            bold.paint(&new),
            "",
            bold.paint(&make),
            package_len = package_len - package.width(),
            old_len = old_len - old.width(),
            new_len = new_len - new.width(),
        );

        let mut install = actions.install.clone();
        install.sort_by(|a, b| {
            a.pkg
                .db()
                .unwrap()
                .name()
                .cmp(b.pkg.db().unwrap().name())
                .then(a.pkg.name().cmp(b.pkg.name()))
        });

        for pkg in &install {
            println!(
                "{:<package_len$}  {:<old_len$}  {:<new_len$}  {}",
                format!("{}/{}", pkg.pkg.db().unwrap().name(), pkg.pkg.name()),
                db.pkg(pkg.pkg.name())
                    .map(|pkg| pkg.version().as_str())
                    .unwrap_or(""),
                pkg.pkg.version().as_str(),
                if pkg.make { &yes } else { &no }
            );
        }
    }

    if !actions.build.is_empty() {
        println!();
        println!(
            "{}{:<package_len$}  {}{:<old_len$}  {}{:<new_len$}  {}",
            bold.paint(&aur),
            "",
            bold.paint(&old),
            "",
            bold.paint(&new),
            "",
            bold.paint(&make),
            package_len = package_len - aur.width(),
            old_len = old_len - old.width(),
            new_len = new_len - new.width(),
        );

        for pkg in actions.build.iter() {
            match pkg {
                Base::Aur(base) => {
                    for pkg in &base.pkgs {
                        let ver = if devel.contains(&pkg.pkg.name) {
                            "latest-commit"
                        } else {
                            &pkg.pkg.version
                        };
                        println!(
                            "{:<package_len$}  {:<old_len$}  {:<new_len$}  {}",
                            format!("{}/{}", repo(config, &pkg.pkg.name), pkg.pkg.name),
                            old_ver(config, &pkg.pkg.name)
                                .map(|v| v.as_str())
                                .unwrap_or_default(),
                            ver,
                            if pkg.make { &yes } else { &no }
                        );
                    }
                }
                Base::Pkgbuild(base) => {
                    for pkg in &base.pkgs {
                        let ver = base.srcinfo.version();
                        let ver = if devel.contains(&pkg.pkg.pkgname) {
                            "latest-commit"
                        } else {
                            &ver
                        };
                        println!(
                            "{:<package_len$}  {:<old_len$}  {:<new_len$}  {}",
                            format!("{}/{}", base.repo, pkg.pkg.pkgname),
                            old_ver(config, &pkg.pkg.pkgname)
                                .map(|v| v.as_str())
                                .unwrap_or_default(),
                            ver,
                            if pkg.make { &yes } else { &no }
                        );
                    }
                }
            }
        }
    }

    println!();
}
