# bah

User-friendly [Arch Linux](https://archlinux.org/) / [AUR](https://aur.archlinux.org/) helper and **pacman** wrapper, written in Rust.

[![bah](https://img.shields.io/aur/version/bah?color=1793d1&label=bah&logo=arch-linux&style=for-the-badge)](https://aur.archlinux.org/packages/bah/)
[![bah-bin](https://img.shields.io/aur/version/bah-bin?color=1793d1&label=bah-bin&logo=arch-linux&style=for-the-badge)](https://aur.archlinux.org/packages/bah-bin/)
[![bah-git](https://img.shields.io/aur/version/bah-git?color=1793d1&label=bah-git&logo=arch-linux&style=for-the-badge)](https://aur.archlinux.org/packages/bah-git/)

**Homepage / source:** [github.com/Hinarosha/bah](https://github.com/Hinarosha/bah)

## What it does

- Install, upgrade, search, and manage **AUR** packages together with **official repos**, using familiar **pacman-style** commands.
- **Minimal prompts** by default; optional richer flows (review, file manager, colours) via config.
- Extended operations for `-S`, `-R`, `-Ss`, `-Su`, `-Qu`, `-Sc`, and more so they understand AUR packages where relevant.

bah is **not** an official Arch tool. It wraps **pacman** / **libalpm** and **makepkg**; behaviour follows your `pacman.conf` and `bah.conf`.

## Requirements

- **Arch Linux** (or derivative using pacman/AUR).
- **Rust 1.87+** if you build from source (see `rust-version` in `Cargo.toml`).
- Toolchain for building AUR packages: `base-devel`, and a running **pacman** database.

## Installation

### From the AUR

```bash
sudo pacman -S --needed base-devel
git clone https://aur.archlinux.org/bah.git
cd bah
makepkg -si
```

Or install a prebuilt/binary package from AUR ([bah-bin](https://aur.archlinux.org/packages/bah-bin/)) if you prefer.

### From a Git checkout (developers)

```bash
cargo build --release
# Binary: target/release/bah
```

Optional Cargo features are documented in [CONTRIBUTING.md](./CONTRIBUTING.md) (`git`, `generate`, tests with `mock`, etc.).

## Quick examples

| Command | Meaning |
|--------|---------|
| `bah` | Same idea as `bah -Syu` (sync & upgrade). |
| `bah foo` | Interactive search/install around `foo`. |
| `bah -S pkg` | Install `pkg` from repos or AUR. |
| `bah -Sua` | Upgrade AUR packages. |
| `bah -Qua` | Show pending AUR updates. |
| `bah -G pkg` | Fetch PKGBUILD (+ related files) for `pkg`. |
| `bah -Bi .` | Build and install from PKGBUILD in the current directory. |
| `bah --gendb` | (Re)build devel DB when migrating or tracking `*-git` style packages. |

## Configuration

- User config: **`bah.conf`** (see **`bah.conf(5)`** once installed).
- Colours follow **pacman**: enable `Color` in **`pacman.conf`** if you want colour in the terminal.

Useful options (see man page for the full list):

- **`FileManager`** — open PKGBUILDs or trees in your file manager for review.
- **`BottomUp`** — menu / search order from the bottom up.

## Documentation

- **`bah(8)`** — CLI operations and bah-specific flags.
- **`bah.conf(5)`** — configuration file.

After installing from the AUR package, man pages are usually available immediately.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) (formatting with `cargo fmt`, `cargo test --features mock`, feature flags).

Bug reports and patches are welcome via [GitHub Issues](https://github.com/Hinarosha/bah/issues) and pull requests.

## Troubleshooting builds

If a package fails to build in bah, try **`makepkg`** alone in a clean directory. If **makepkg** fails, report to the **PKGBUILD maintainer**. If only bah misbehaves, open an issue on **bah** with logs and steps to reproduce.

## License

GPL-3.0 — see [LICENSE](./LICENSE).
