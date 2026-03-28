//! Maps leading "verb" words (e.g. `search`, `install`) to pacman-style flags so the rest of the
//! CLI stays unchanged. Anything starting with `-` is left as-is for full pacman compatibility.

fn prepend(prefix: &[&str], rest: Vec<String>) -> Vec<String> {
    prefix
        .iter()
        .map(|s| (*s).to_string())
        .chain(rest.into_iter())
        .collect()
}

/// If the first argument is not a flag, it may be a subcommand alias or a bare search term.
/// - `bah vlc` → `bah -Ss vlc` (implicit search)
/// - `bah search vlc` → `bah -Ss vlc`
/// - `bah -Ss vlc` → unchanged
/// - `bah ./pkg` → unchanged (pkgbuild / default op behavior)
pub fn expand_subcommands(args: Vec<String>) -> Vec<String> {
    if args.is_empty() {
        return args;
    }
    let first = &args[0];
    if first.starts_with('-') {
        return args;
    }
    // Let relative PKGBUILD paths and absolute paths use the original Default-op / sync logic.
    if first.starts_with("./") || first.starts_with('/') {
        return args;
    }

    let head = first.clone();
    let cmd = head.to_ascii_lowercase();
    let rest = args[1..].to_vec();

    match cmd.as_str() {
        "search" | "find" => prepend(&["-S", "-s"], rest),

        "install" => prepend(&["-S"], rest),

        // Full system refresh + upgrade (same idea as bare `bah`).
        "update" | "upgrade" | "sync" | "up" => {
            if rest.is_empty() {
                vec!["-Syu".to_string()]
            } else {
                let mut v = vec!["-S".to_string()];
                v.extend(rest);
                v
            }
        }

        "remove" | "uninstall" | "rm" => prepend(&["-R"], rest),

        "query" => prepend(&["-Q"], rest),

        "info" => prepend(&["-S", "-i"], rest),

        "build" => prepend(&["-B"], rest),

        "clone" | "getpkgbuild" | "fetch" => prepend(&["-G"], rest),

        "news" => prepend(&["-P", "-w"], rest),

        "stats" => prepend(&["-P", "--stats"], rest),

        "order" => prepend(&["-P", "--order"], rest),

        "files" => prepend(&["-F"], rest),

        "database" => prepend(&["-D"], rest),

        "deptest" => prepend(&["-T"], rest),

        "repo" | "repolist" => prepend(&["-L"], rest),

        "chroot" => prepend(&["-C"], rest),

        "clean" => prepend(&["-S", "-c"], rest),

        "list" => prepend(&["-S", "-l"], rest),

        "help" => vec!["-h".to_string()],

        "version" => vec!["-V".to_string()],

        // Implicit search: all tokens are search terms (parity with `bah -Ss a b`).
        _ => prepend(&["-S", "-s"], std::iter::once(head).chain(rest.into_iter()).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::expand_subcommands;

    #[test]
    fn passthrough_flags() {
        assert_eq!(
            expand_subcommands(vec!["-Ss".into(), "vlc".into()]),
            vec!["-Ss", "vlc"]
        );
    }

    #[test]
    fn implicit_search() {
        assert_eq!(
            expand_subcommands(vec!["vlc".into()]),
            vec!["-S", "-s", "vlc"]
        );
    }

    #[test]
    fn search_verb() {
        assert_eq!(
            expand_subcommands(vec!["search".into(), "vlc".into()]),
            vec!["-S", "-s", "vlc"]
        );
    }

    #[test]
    fn update_empty_is_syu() {
        assert_eq!(expand_subcommands(vec!["update".into()]), vec!["-Syu"]);
    }

    #[test]
    fn relative_pkgbuild_untouched() {
        assert_eq!(
            expand_subcommands(vec!["./mypkg".into()]),
            vec!["./mypkg"]
        );
    }
}
