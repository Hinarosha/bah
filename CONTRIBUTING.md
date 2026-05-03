# Contributing to bah

## Formatting

Please format the code using `cargo fmt`

## Building

bah is built with cargo.

To build bah use:

```
cargo build
```

To run bah use:

```
cargo run -- <args>
```

bah has a couple of feature flags which you may want to enable:

- backtrace: does nothing, kept around for backwards compatibility
- git: target the libalpm-git API
- generate: generate the libalpm bindings at build time (requires clang)

### Building Against a Custom libalpm

If you wish to build against a custom libalpm you can specify **ALPM_LIB_DIR** while using the generate
feature. Then running with **LD_LIBRARY_PATH** pointed at the custom libalpm.so.

## Testing

bah's test suite can be run by running:

```
cargo test --features mock
```

## Translating

**bah** is a fork of **[paru](https://github.com/Morganamilo/paru)**. Most locale files under `po/` descend from **paru’s gettext catalogues**; translator credits live in each `*.po` header. Read **[po/README.md](po/README.md)** for a short attribution note and upstream pointers.

For **bah-specific** translation work (new strings after a fork, broken plural forms, typos), open an issue or PR on **[Hinarosha/bah](https://github.com/Hinarosha/bah)** — or reuse paru’s community workflow if you also contribute upstream: **[paru discussions](https://github.com/Morganamilo/paru/discussions)** (search for localization / i18n threads).

### New Languages

When translating to a new language try to stick to languages pacman already supports:
https://gitlab.archlinux.org/pacman/pacman/-/tree/master/src/pacman/po. For example using
`es` over `es_ES`.

To translate bah to a new language, copy the template `.pot` file to the locale you
are translating to.

For example, to translate bah to Japanese you would do:

```
cp po/bah.pot po/jp.po
```

Then fill out the template file with your information and translation.

Alternatively, you can use programs like `poedit` to write the translations.

### Updating existing translations

To update existing translations against new code you must first update the .po
files.

Do this as its own commit.

```
./scripts/updpo
git commit po
```

Then fill in new strings.

### Testing Translations

To test the translations you first must build the translation then run bah
pointing it at the generated files.

```
./scripts/mkmo locale/
LOCALE_DIR=locale/ cargo run -- <args>
```
