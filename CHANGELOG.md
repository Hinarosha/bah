# Changelog

## bah v2.6.2 (2026-05-18)

### Fixed
- Conflict resolution questions (`:: Resolve conflict by removing '...' ? [Y/n]:`) now work without freezing the application. The helper process held the stdin mutex for the entire transaction lifetime; when the ALPM question callback tried to re-acquire it from the same thread to read the parent's answer, it deadlocked on Linux's non-reentrant pthread mutex. Fixed by scoping the stdin lock to only the plan-reading phase.
- If the IPC write that relays a question to the parent fails, the helper no longer enters an infinite wait for an answer that will never arrive; it falls back to the default answer instead.
- Removal progress is now labeled `Removing` instead of `Installing` in the transaction progress display.

Files: src/tx_helper.rs, src/ui.rs

## bah v2.6.1 (2026-05-13)

### Changed
- Bump version to 2.6.1.
- Always enable colors unless --nocolor; rely on auto TTY detection.
- Standardize hook step output colors and OK marker for consistent UI.

Files: Cargo.toml, src/config.rs, src/ui.rs

## bah v2.5.7 (2026-05-13)

### Added
- Validate pacman RootDir/DBPath/CacheDir before creating the ALPM handle.
- Ensure hook dirs include /usr/share/libalpm/hooks plus pacman.conf HookDir entries.
- Count hook start/done events for audit logs.

### Changed
- Map ALPM commit errors to include hook failure context; disable noscriptlet flags in backend plans.
- Log helper ALPM paths and tighten helper PATH defaults.

### Fixed
- Progress bar tests now validate visible width instead of raw chars.

Files: src/config.rs, src/backend/alpm.rs, src/tx_helper.rs, src/ui.rs

## bah v2.5.6 (2026-05-13)

### Added
- Post-mortem warning when hook failures affect boot-critical hooks.

### Changed
- Fail fast on signal handler registration errors with explicit exit codes.
- Add RAII cleanup guard to always release/unlock ALPM transactions.
- Track hook phase and last running hook for commit error messages.
- Stream scriptlet output directly to the terminal with a stdout lock.

### Fixed
- Avoid panics on empty error chains and provider lists with missing db handles.

Files: src/exec.rs, src/tx_helper.rs, src/lib.rs, src/config.rs

## bah v2.5.5 (2026-05-13)

### Added
- BootMinFree config (MB) with default 100 and documentation.
- Critical package detection list and warning banner for risky transactions.
- --force-noscriptlet CLI override for critical package guard.

### Changed
- Select /boot mountpoint from fstab/mountinfo and open without symlink traversal.
- Enforce noscriptlet and boot checks for sync/remove/upgrade in helper and backend.
- Warn on partial upgrades when not doing sysupgrade.

Files: src/tx_helper.rs, src/install.rs, src/remove.rs, src/util.rs, src/config.rs, src/backend/alpm.rs, src/command_line.rs, bah.conf, man/bah.conf.5

## bah v2.5.4 (2026-05-12)

### Changed
- Confirmation table labels install vs upgrade versions correctly and can split Repo/AUR sections.
- Transaction renderer manages download and install lines as multi-row blocks with active/frozen state.
- Install progress line uses colored prefixes and adaptive bar widths; hook messages use "::" style.

### Fixed
- Fall back to pacman backend for cache cleaning when using ALPM backend.

Files: src/ui.rs, src/tx_helper.rs, src/backend/alpm.rs

## bah v2.5.3 (2026-05-12)

### Changed
- Download progress rendering batches multiple lines, keeps completed downloads visible, and throttles redraws.
- Time remaining formatting handles completed downloads as 0s.

Files: src/ui.rs

## bah v2.5.2 (2026-05-10)

### Fixed
- Track interrupt state across signals and helper exits to avoid stalling on cancel.
- Cleanup pacman lock on interrupts in helper exit path.

### Changed
- Progress UI uses wider colorized bars, compact speed units, and time remaining.
- Env var mutations are explicitly marked as unsafe for chroot/helper sanitization.
- Bump version to 2.5.2 and Rust edition to 2024; update man page version strings.

Files: Cargo.toml, src/exec.rs, src/tx_helper.rs, src/ui.rs, src/config.rs, man/bah.8, man/bah.conf.5, tests/common/mod.rs

## bah v2.4.3 (2026-05-10)

### Fixed
- Interrupting an ALPM transaction now cleans up correctly and returns InterruptedError.
- Stale pacman lock removal is triggered on interrupts during helper commit.

### Changed
- Improve signal coordination between helper, backend, and exec; defer interrupts during commit.
- Update search/subcommand paths to be resilient during interrupted runs.

Files: src/tx_helper.rs, src/backend/alpm.rs, src/backend/mod.rs, src/exec.rs, src/search.rs, src/subcommands.rs

## bah v2.4.2 (2026-05-10)

### Added
- TxConfirmOp enum and confirmation prompts for install/update/remove.
- TransactionRenderController for download/install/hook progress lines.
- Helper IPC events for download progress, install progress, and hook phases.
- Stale pacman lock recovery in helper with user prompt and retry.

### Changed
- Install flow always uses confirmation table; update/remove flows use matching prompts.
- "sync" subcommand maps to -Sy (refresh only); update/upgrade remains -Syu.
- AUR and diff cache cleanup consolidated; repo clean delegated to pacman.
- ALPM event output uses PackageOperationDone messages; progress callback silenced to reduce spam.
- Table headers, section separators, and repo colors updated to ANSI palette.

### Removed
- Verbose install list rendering in fmt (replaced by confirmation tables).

### Fixed
- Helper now errors if it exits without sending a result.

Files: src/ui.rs, src/tx_helper.rs, src/install.rs, src/remove.rs, src/subcommands.rs, src/clean.rs, src/fmt.rs, src/backend/alpm.rs, src/lib.rs, src/config.rs, Cargo.lock

## bah v2.4.1 (2026-05-03)

### Changed
- Bump version to 2.4.1 and refresh project metadata and bug URLs.
- Update help output and man pages to document verb commands and options.
- Update completion scripts for verb UX (bash, fish, zsh).
- Refresh translations and add po/README; fix aurrpcurl typo in help strings.

Files: Cargo.toml, src/help.rs, man/bah.8, man/bah.conf.5, completions/*, po/*, scripts/mkpot, README.md, CONTRIBUTING.md

## bah v2.3.6 (2026-05-03)

### Changed
- Rewrite README to describe verb-first workflow, relation to paru, and UI behavior.

Files: README.md

## bah v2.3.5 (2026-05-03)

### Security
- Sanitize helper environment, bound IPC reads, and harden /var/log/bah.log creation.
- Add safer /boot file descriptor checks and validate IPC framing.

### Changed
- README updates for clearer purpose and usage.

Files: src/tx_helper.rs, README.md

## bah v2.3.4 (2026-05-03)

### Added
- Audit logging for commit lifecycle and /boot checks in the helper.

### Changed
- Defer interrupts during transaction commits; improve signal handling and log context.

Files: src/tx_helper.rs

## bah v2.3.3 (2026-05-03)

### Fixed
- Correct version column width calculation in the install confirmation table.

Files: src/ui.rs

## bah v2.3.2 (2026-05-02)

### Changed
- Bump version to 2.3.2 and update authorship/repo metadata.
- Split upgrade preview by Repo vs AUR and show changelog first line for repo packages.
- Add AUR download size from cache and "Looking for upgrades..." message.
- Tweak sync status output.

Files: Cargo.toml, src/ui.rs, src/upgrade.rs, src/backend/mod.rs

## bah v2.0.4 (2026-05-02)

### Added
- Transaction confirmation UI with action color styles and totals line.
- Install flow now uses confirmation bundle and new log prefixes.

### Changed
- Color palette adjustments for confirmation rows and version diff display.

Files: src/ui.rs, src/install.rs, src/config.rs, src/fmt.rs

## bah v2.0.3 (2026-05-02)

### Added
- New UI module for confirmation tables and transaction previews.

### Changed
- Backend error handling and fallback verbosity improvements.
- Limit ALPM log spam and allow empty transactions for -Sy and -Su.
- Remove unused helper paths.

Files: src/ui.rs, src/backend/mod.rs, src/tx_helper.rs, src/exec.rs, src/fmt.rs, src/lib.rs

## bah v2.0.2 (2026-05-02)

### Fixed
- Harden helper plan validation (path traversal, absolute local package paths, empty transactions).
- Improve error context for helper failures and pager stdin handling.
- Add /boot free-space guard for boot-sensitive transactions.

Files: src/tx_helper.rs, src/install.rs

## bah v2.0.1 (2026-05-02)

### Security
- Isolate helper IPC file descriptors and sanitize log fragments.
- Add /boot read-write checks and signal-safe transaction recovery for hooks.

### Changed
- Add low-level alpm-sys dependency for helper commit handling.

Files: src/tx_helper.rs, Cargo.toml, Cargo.lock

## bah v2.0.0 (2026-05-01)

This release replaces the pacman wrapper with a native libalpm backend and a privileged helper for root transactions.

### Added
- Native ALPM backend with transaction planning and legacy pacman fallback.
- Privileged transaction helper (IPC) for root operations, questions, and commits.
- ALPM callbacks for events, downloads, and log streaming.

### Changed
- Install, sync, query, and upgrade flows now use the backend abstraction.
- New transaction formatting, repo list presentation, and upgrade/install table output.
- Database lock recovery and helper transaction mode for root operations.

Files: src/backend/alpm.rs, src/backend/legacy_pacman.rs, src/tx_helper.rs, src/install.rs, src/sync.rs, src/query.rs, src/upgrade.rs, src/fmt.rs, src/lib.rs

## bah v1.0.3 (2026-03-28)

### Changed
- Refactor search output with SearchRow layout, column sizing, and match highlighting.
- Add TopDown config option and docs; adjust default sort behavior.

Files: src/search.rs, src/config.rs, bah.conf, man/bah.conf.5

## bah v1.0.2 (2026-03-28)

### Changed
- Complete rename pass from paru to bah across docs, config, completions, man pages, and translations.
- Update project URLs, package names, and configuration references.

Files: README.md, bah.conf, man/bah.8, man/bah.conf.5, completions/*, po/*, scripts/*, tests/*

## bah v1.0.1 (2026-03-28)

### Changed
- Rename files and paths from paru to bah in workflows, man pages, translations, tests, and testdata.

Files: man/*, po/*, tests/*, testdata/*, .github/*

## bah v1.0.0 (2026-03-28)

### Added
- Initial bah release with verb-style CLI mapping (install/search/update/remove).
- Subcommands module and argument parsing to expand verbs into pacman args.

Files: src/subcommands.rs, src/lib.rs
