# bah

A **readable, conversational package manager** for [Arch Linux](https://archlinux.org/) and the [AUR](https://aur.archlinux.org/), written in Rust. You talk to it in plain words; it keeps **pacman-style flags** (`-Ss`, `-Syu`, …) for anyone who already knows them.

[![bah](https://img.shields.io/aur/version/bah?color=1793d1&label=bah&logo=arch-linux&style=for-the-badge)](https://aur.archlinux.org/packages/bah/)
[![bah-bin](https://img.shields.io/aur/version/bah-bin?color=1793d1&label=bah-bin&logo=arch-linux&style=for-the-badge)](https://aur.archlinux.org/packages/bah-bin/)
[![bah-git](https://img.shields.io/aur/version/bah-git?color=1793d1&label=bah-git&logo=arch-linux&style=for-the-badge)](https://aur.archlinux.org/packages/bah-git/)

**Source:** [github.com/Hinarosha/bah](https://github.com/Hinarosha/bah)

## Relation to paru

**bah is a fork of [paru](https://github.com/Morganamilo/paru)** that has been heavily modified and evolved (UX, verbs, internals, hardening, and more). If you know paru, many concepts carry over; the goal of this fork is a different **interaction model** and a **clearer, nicer terminal UI**, not a pixel-perfect clone.

## The idea: talk to your package manager

Instead of memorising opaque flag combinations for everyday tasks, you can use **verbs**:

| You type | Same idea as |
|----------|----------------|
| `bah search foo` | `bah -Ss foo` |
| `bah install pkg` | `bah -S pkg` |
| `bah remove pkg` | `bah -R pkg` |
| `bah update` | full refresh + upgrade (`-Syu` style) |
| `bah upgrade pkg …` | sync/install those targets |
| `bah info pkg` | query package info (`-Si`) |
| `bah clone pkg` | `-G` / get PKGBUILD |

Anything that **starts with `-`** is left unchanged, so **`bah -Ss`**, **`bah -Syu`**, and the rest of the pacman vocabulary still work exactly as power users expect.

A bare query like **`bah vlc`** is treated as **search terms** (same as feeding `-Ss`), so you rarely need to spell operations out.

## UI and readability

bah puts effort into **presentation**: menus, progress, prompts, and listings are meant to be easier to scan than raw pacman output. Colours follow **`pacman.conf`** when `Color` is enabled there. Optional flows (file manager review, paging, etc.) are tuned in **`bah.conf`** — see **`bah.conf(5)`**.

## How it runs under the hood

bah is **not** “just spawning pacman for everything”. For normal sync/remove/upgrade paths it drives **libalpm** (same library pacman uses) through its **own backend**, dedicated **transaction helper**, resolver, and UI layer — so the heavy lifting (deps, AUR, transactions, prompts) is **bah**, with behaviour anchored in **`pacman.conf`** and databases.

**pacman** is still there as the familiar binary on disk and is used when bah needs to **delegate or fall back** (legacy paths, compatibility, or when the integrated path is not the right tool). Think of bah as the primary control plane; pacman remains the reference implementation Arch ships.

## Requirements

- Arch (or a pacman-based environment with AUR workflows).
- **Rust 1.87+** to build from source (`rust-version` in `Cargo.toml`).
- **`base-devel`** (and friends) for building AUR packages.

## Installation

### AUR

```bash
sudo pacman -S --needed base-devel
git clone https://aur.archlinux.org/bah.git
cd bah
makepkg -si
```

Or use **[bah-bin](https://aur.archlinux.org/packages/bah-bin/)** if you want a prebuilt package.

### From this repository

```bash
cargo build --release
# ./target/release/bah
```

Feature flags and tests: [CONTRIBUTING.md](./CONTRIBUTING.md).

## Quick reference

| Command | Meaning |
|--------|---------|
| `bah` | Default full sync / upgrade (same spirit as `-Syu`). |
| `bah foo bar` | Search `foo bar` in repos + AUR (implicit `-Ss`). |
| `bah install pkg` | Install from repos or AUR. |
| `bah -S pkg` | Same, pacman-style. |
| `bah -Sua` / `bah upgrade` (targets optional) | AUR / sync upgrades depending on flags. |
| `bah -Qua` | Pending AUR updates. |
| `bah clone pkg` | Download PKGBUILD (+ related files). |
| `bah -Bi .` | Build & install PKGBUILD in cwd. |
| `bah --gendb` | Devel DB for tracking `*-git`-style packages (e.g. migrations). |

Full verb list and edge cases live in `src/subcommands.rs`; **`bah(8)`** documents flags and operations.

## Configuration & docs

- **`bah.conf`** — **`bah.conf(5)`**
- **`bah(8)`** — CLI reference

## Contributing & bugs

[CONTRIBUTING.md](./CONTRIBUTING.md) · [Issues](https://github.com/Hinarosha/bah/issues)

If **makepkg** alone fails, that’s usually upstream PKGBUILD territory. If only bah misbehaves, open an issue with logs and repro steps.

## License

GPL-3.0 — see [LICENSE](./LICENSE).
