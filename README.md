# bah

Feature packed AUR helper

[![bah](https://img.shields.io/aur/version/bah?color=1793d1&label=bah&logo=arch-linux&style=for-the-badge)](https://aur.archlinux.org/packages/bah/)
[![bah-bin](https://img.shields.io/aur/version/bah-bin?color=1793d1&label=bah-bin&logo=arch-linux&style=for-the-badge)](https://aur.archlinux.org/packages/bah-bin/)
[![bah-git](https://img.shields.io/aur/version/bah-git?color=1793d1&label=bah-git&logo=arch-linux&style=for-the-badge)](https://aur.archlinux.org/packages/bah-git/)

## Description

bah is your standard pacman wrapping AUR helper with lots of features and minimal interaction.

[![asciicast](https://asciinema.org/a/sEh1ZpZZUgXUsgqKxuDdhpdEE.svg)](https://asciinema.org/a/sEh1ZpZZUgXUsgqKxuDdhpdEE)

## Installation

```
sudo pacman -S --needed base-devel
git clone https://aur.archlinux.org/bah.git
cd bah
makepkg -si
```

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md).

## General Tips

- **Man pages**: For documentation on bah's options and config file see `bah(8)` and `bah.conf(5)` respectively.

- **Color**: bah only enables color if color is enabled in pacman. Enable `color` in your `pacman.conf`.

- **File based review**: To get a more advanced review process enable `FileManager` with your file manager of choice in `bah.conf`.

- **Flip search order**: To get search results to start at the bottom and go upwards, enable `BottomUp` in `bah.conf`.

- **Editing PKGBUILDs**: When editing PKGBUILDs, you can commit your changes to make them permanent. When the package is upgraded, `git` will try to merge your changes with upstream's.

- **PKGBUILD syntax highlighting**: You can install [`bat`](https://github.com/sharkdp/bat) to enable syntax highlighting during PKGBUILD review.

- **Tracking -git packages**: bah tracks -git package by monitoring the upstream repository. bah can only do this for packages that bah itself installed. `bah --gendb` will make bah aware of packages it did not install.

## Examples

`bah <target>` -- Interactively search and install `<target>`.

`bah` -- Alias for `bah -Syu`.

`bah -S <target>` -- Install a specific package.

`bah -Sua` -- Upgrade AUR packages.

`bah -Qua` -- Print available AUR updates.

`bah -G <target>` -- Download the PKGBUILD and related files of `<target>`.

`bah -Gp <target>` -- Print the PKGBUILD of `<target>`.

`bah -Gc <target>` -- Print the AUR comments  of `<target>`.

`bah --gendb` -- Generate the devel database for tracking `*-git` packages. This is only needed when you initially start using bah.

`bah -Bi .` -- Build and install a PKGBUILD in the current directory.

## IRC

bah now has an IRC. #bah on [Libera Chat](https://libera.chat/). Feel free to join for discussion and help with bah.

## Debugging

bah is not an official tool. If bah can't build a package, you should first check if makepkg can successfully build the package. If it can't, then you should report the issue to the maintainer. Otherwise, it is likely an issue with bah and should be reported here.
