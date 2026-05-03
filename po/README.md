# Translations (`po/`)

This directory holds **gettext** message catalogs (`.po`) for bah’s user-visible strings.

## Credit and relationship to paru

**[bah](https://github.com/Hinarosha/bah)** is a fork of **[paru](https://github.com/Morganamilo/paru)**. Much of the translatable text and many of the `*.po` files **come from paru’s history and from paru’s translators**. Each language file still records who worked on it in the **header** (`Last-Translator`, language team, and any `Previous translators` comments).

Upstream translation history:

- paru repository: <https://github.com/Morganamilo/paru>
- paru’s catalogs: the `po/` tree in that repository.

Changes that are specific to **bah** (new prompts, reworded UI) flow from **`bah.pot`**; maintainers update their `xx.po` accordingly.

## Reporting a translation issue for bah

Use the **`Report-Msgid-Bugs-To`** URL in the `.po` header where it still applies, or open an issue on **[Hinarosha/bah](https://github.com/Hinarosha/bah)** and mention the locale (`fr`, `uk`, …) plus the English `msgid` or the on-screen string.

## Building / testing `.mo` files

See [CONTRIBUTING.md](../CONTRIBUTING.md) (section **Translating**) and the `./scripts/mkmo` and `./scripts/mkpot` helpers.
