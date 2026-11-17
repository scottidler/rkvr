# Design Document: rmrf removal honors sudo, and a missing rmrf.cfg is not fatal

**Author:** Scott Idler
**Date:** 2026-11-16
**Status:** Approved
**Review Passes Completed:** 5/5

## Summary

`rkvr rmrf` on a file in a root-owned directory archives the file, then dies with `Permission denied` while removing it. `sudo=yes` in the config makes no difference. Running it under `sudo` instead dies earlier, because `rmrf.cfg` is looked up in `/root` and a missing file is a hard error. This doc makes removal escalate through `sudo` when the kernel refuses the unlink, keeps going past one failed target, and makes the config loader fall back to defaults and resolve the invoking user's home under `sudo`.

## Problem Statement

### Background

- `rkvr rmrf <targets>` = archive each target into a bundle under `rmrf_path` (`/var/tmp/rmrf/<timestamp>-NNN/`), then delete the targets, then prune bundles older than `keep` days
- Archive already escalates: `create_tar_command` (`src/main.rs:303`) prefixes `sudo tar` when the target's owner uid differs from the caller's
- Config lives in `~/.config/rmrf/rmrf.cfg` (INI); every key read from it has a default (`src/main.rs:874-892`)
- Prior fix in the same area: `docs/2026-09-14-dangling-symlink.md`. Its invariant stays: symlinks are acted on, never followed

### Problem

Observed 2026-11-16, `rkvr v0.1.25`, target `/opt/firefox/distribution/tab-switcher-spike.xpi` (file `0644 root`, parent `drwxr-xr-x root`):

```
❯ rkvr rmrf /opt/firefox/distribution/tab-switcher-spike.xpi
/opt/firefox/distribution/tab-switcher-spike.xpi
-> /var/tmp/rmrf/2026-11-16-160443-000/
Error: Permission denied (os error 13)

Location:
    src/main.rs:537:13

❯ sudo -E $(which rkvr) rmrf /opt/firefox/distribution/tab-switcher-spike.xpi
sudo: preserving the entire environment is not supported, '-E' is ignored
Error: Failed to load config

Caused by:
    couldn't read /root/.config/rmrf/rmrf.cfg: No such file or directory (os error 2)

Location:
    src/main.rs:870:22
```

Bug 1, removal ignores `sudo`:
- `remove_targets` (`src/main.rs:532-541`) takes no `sudo` flag; bare `fs::remove_file` / `fs::remove_dir_all`
- Unlink permission comes from the PARENT directory (write + search, plus the sticky-bit owner rule), not from the file's owner. Nothing checks it
- The sudo-aware helpers `remove_file_with_sudo` / `remove_directory_with_sudo` (`src/main.rs:137-185`) exist but only `cleanup` calls them, and they carry three defects:
  - decide by file owner uid (wrong test for unlink)
  - `fs::metadata` follows symlinks, so a dangling link errors `NotFound` before removal
  - no `--` before the path in `sudo rm`
- `remove_targets` stops at the first error with `?`. The bin already holds bundles for every target, so later targets survive with a bin entry that `ls-rmrf` shows as removed, and `cleanup` is skipped
- Reproduces without root: a user-owned `0555` directory gives the same EACCES. Baseline, run 2026-11-16 against the installed `v0.1.25` (= `main` at `b40c5bd`):

```
S=<scratch dir>
mkdir -p $S/home/.config/rmrf $S/bin $S/w/src $S/w/src2
printf 'rmrf_path=%s/bin/rmrf\nbkup_path=%s/bin/bkup\nsudo=no\nkeep=21\n' $S $S > $S/home/.config/rmrf/rmrf.cfg
echo a > $S/w/src/a.txt; echo c > $S/w/src2/c.txt; chmod 0555 $S/w/src
cd $S/w; HOME=$S/home rkvr rmrf src/a.txt src2/c.txt; echo "exit=$?"
ls src src2; ls $S/bin/rmrf | wc -l
```

```
.../w/src/a.txt
-> .../bin/rmrf/2026-11-16-161138-000/
.../w/src2/c.txt
-> .../bin/rmrf/2026-11-16-161138-001/
Error: Permission denied (os error 13)

Location:
    src/main.rs:537:13
exit=1
src:
a.txt

src2:
c.txt
2
```

Bug 2, config under `sudo`:
- `src/main.rs:862-871` builds the path from `dirs::home_dir()`, which reads `$HOME`. `sudo` resets it to `/root`
- `Ini::load` failure is fatal even though no key is required. This breaks every action, not just rmrf
- Defaults alone would still be wrong under `sudo`: the owner's `keep=21` becomes the fallback `Config::cleanup_days` (`src/main.rs:885-888`), which is `30` on this machine (`~/.config/rkvr/rkvr.yml` has no `cleanup_days` key)

Also in the removal path, found in review:
- `cleanup` reads each bin entry with `fs::metadata` (`src/main.rs:208`), which follows symlinks: a dangling link in `rmrf_path` aborts all of cleanup through `?`, and a live link's age comes from its target
- `cleanup` stops at the first bad entry (`?` at :208, :225, :228)

### Goals

- `rkvr rmrf` on a target whose parent the caller cannot write escalates through `sudo rm` when `sudo=yes`, and fails naming the target and its parent when `sudo=no`
- One failed removal does not abandon the rest; the command reports every survivor and exits non-zero
- One removal path for targets and for bundle cleanup, symlink-safe (no `fs::metadata` anywhere on it), with `--`; both walk every entry and aggregate failures
- A missing `rmrf.cfg` gives the documented defaults; an unreadable one, or a dangling `rmrf.cfg` symlink, stays a loud error
- Under `sudo`, `rmrf.cfg` is read from the invoking user's home (`SUDO_USER`)

### Non-Goals

- Excluded: retiring `Config::load` / `~/.config/rkvr/rkvr.yml` or folding `rmrf.cfg` into YAML. `Config::load` is read only for the `keep` fallback (`cleanup_days`); the user's `rkvr.yml` keys (`rmrf:`, `bkup:`) match no struct field and are ignored, `rkvr.cfg` is read by nothing, and both name `/var/tmp/rkvr/...`, disagreeing with `rmrf.cfg`. Merging them is a separate decision with a dotfiles-repo blast radius
- Excluded: making `archive` best-effort. It fails closed today (nothing is removed unless every target archived, `?` at `src/main.rs:593`, :608), and the best-effort removal below relies on exactly that
- Excluded: changing which files `archive` escalates `tar` / `cp` for. Read permission tracks ownership closely enough; not part of either bug
- Parked: a non-root `rkvr rcvr` of a bundle that a `sudo rkvr` run left root-owned (`remove_dir_all(ts_dir)` at `src/main.rs:832`). Not reproduced. Revisit if it is hit; with Bug 1 fixed, running rkvr under `sudo` stops being necessary
- Parked: logs and bundles going root-owned under `sudo`. Same revisit condition
- Parked: two unit tests (`test_categorize_paths_mixed_files_and_directories`, `test_archive_single_file`) fail when `$TMPDIR` is a symlink (canonical vs raw path compare). Green on CI and with a real `TMPDIR`; revisit if CI's tempdir ever becomes a symlink

## Proposed Solution

### Overview

- Removal: try the plain unlink first; let the kernel decide. On `PermissionDenied` (Rust maps both EACCES and EPERM there), escalate to `sudo rm -f -- <path>` (`-rf` only for a directory) when allowed, else fail with the parent named
- A target that is already gone counts as removed: the goal is absence. Covers `rkvr rmrf dir dir/child`, where removing `dir` takes `dir/child` with it (today that errors at `src/main.rs:537` too)
- `remove_targets` and `cleanup` attempt every entry, collect failures, then return one error listing what is not fully removed (`remove_dir_all` may have deleted part of a tree first)
- Config: resolve the home dir (`SUDO_USER`'s passwd entry when running as root under sudo), then read `rmrf.cfg`; a missing file = defaults, where `keep` falls back to `Config::cleanup_days` and the rest to the literals at `src/main.rs:874-892`
- Assumption: single-operator CLI, nothing else mutates the targets between archive and removal

### Architecture

```
archive(...) -> archive groups + dirs (unchanged)
             -> remove_targets(targets, sudo)          # now best-effort, aggregate error
                  for each: remove_path(t, sudo)
             -> cleanup(path, days, sudo)               # only if removal fully succeeded
                  for each entry: symlink_metadata (age from the entry itself)
                  stale -> remove_path(e, sudo); collect failures, aggregate error

remove_path(p, sudo):
  meta = symlink_metadata(p)                            # never follows
  Err(NotFound)              -> Ok (already gone)
  plain = meta.is_dir() ? remove_dir_all(p) : remove_file(p)
  Ok | Err(NotFound)         -> Ok
  Err(PermissionDenied) && escalation_allowed(sudo, uid)
                             -> sudo rm -f -- p      (non-dir, incl. symlink)
                                sudo rm -rf -- p     (real dir)
                                non-zero status -> Err(p)
  Err(PermissionDenied)      -> Err("permission denied removing p (parent: dir)")
  Err(other)                 -> Err(other, with p)

main:
  home  = rmrf_cfg_home(getuid(), $SUDO_USER, passwd_home, dirs::home_dir())
  cfg   = load_rmrf_cfg(home/.config/rmrf/rmrf.cfg)    # absent -> empty Ini -> defaults
                                                        # dangling symlink | unreadable -> error
```

### Data Model

No new types on disk. `metadata.yml` is unchanged.

### API Design

All in `src/main.rs`, private:

```rust
fn escalation_allowed(sudo: bool, uid: u32) -> bool;        // sudo && uid != 0
fn remove_path(path: &Path, sudo: bool) -> Result<()>;
fn remove_targets(targets: &[PathBuf], sudo: bool) -> Result<()>;  // gains `sudo`
fn rmrf_cfg_home(
    uid: u32,
    sudo_user: Option<&str>,
    lookup: impl Fn(&str) -> Result<Option<PathBuf>>,
    fallback: Option<PathBuf>,
) -> Result<PathBuf>;
fn passwd_home(user: &str) -> Result<Option<PathBuf>>;      // libc::getpwnam_r
fn load_rmrf_cfg(path: &Path) -> Result<Ini>;
fn cleanup(dir_path: &Path, days: usize, sudo: bool) -> Result<()>;  // signature unchanged
```

`passwd_home` outcomes, kept distinct:
- entry found -> `Ok(Some(pw_dir))`, copied out as raw bytes (`OsStr`) before the buffer drops; empty or relative `pw_dir` -> `Err`
- no such user (`getpwnam_r` returns 0 with a null result) -> `Ok(None)`
- NSS error, or `ERANGE` past the 1 MiB cap -> `Err` naming the user and errno

`rmrf_cfg_home` rules:
- `uid == 0` and `SUDO_USER` set and not `root` -> `lookup(user)`; `Ok(None)` is an error "SUDO_USER=<user> has no passwd entry"; `Err` propagates
- otherwise -> `fallback` (`dirs::home_dir()`); `None` is the existing "home dir not found!" error

Removed: `remove_file_with_sudo`, `remove_directory_with_sudo`, and their two user-owned tests (replaced by `remove_path` tests).

### Implementation Plan

#### Phase 1: rmrf.cfg loader
**Model:** opus (unsafe `getpwnam_r` FFI)
- Extract `rmrf_cfg_home`, `passwd_home`, `load_rmrf_cfg` from `main` (`src/main.rs:862-871`)
- `load_rmrf_cfg`: `symlink_metadata` says nothing is there -> `Ini::new()` plus an `info!` naming the path; otherwise `fs::read_to_string`, where ANY error (including `NotFound` from a dangling symlink) -> wrapped error naming the path; contents -> `Ini::read`
- `passwd_home` via `libc::getpwnam_r` (re-entrant; `libc` is already a dependency), outcomes per API Design. Start the buffer at `sysconf(_SC_GETPW_R_SIZE_MAX)` (or 1024 when it returns -1); on `ERANGE` double it, capped at 1 MiB
- `main` calls `rmrf_cfg_home(current_uid(), env::var("SUDO_USER").ok().as_deref(), passwd_home, dirs::home_dir())`
- Unit tests, all prefixed `rmrf_cfg_` so one filter selects them:
  - `rmrf_cfg_missing_returns_defaults`
  - `rmrf_cfg_unreadable_is_error` (file `chmod 000`, asserts `getuid() != 0`)
  - `rmrf_cfg_dangling_symlink_is_error`
  - `rmrf_cfg_home_uses_sudo_user_when_root`, `rmrf_cfg_home_ignores_sudo_user_when_not_root`, `rmrf_cfg_home_ignores_sudo_user_root`, `rmrf_cfg_home_unknown_sudo_user_is_error`, `rmrf_cfg_home_lookup_error_propagates`
  - `rmrf_cfg_passwd_home_root` (`root` -> `/root`; present on Linux and on `ubuntu-latest`, no env data involved), `rmrf_cfg_passwd_home_unknown_user_is_none`
- **Success criteria:**
  - `cargo test rmrf_cfg` reports at least `10 passed` and `0 failed`
  - `HOME=$(mktemp -d) RMRF_PAGER=cat target/debug/rkvr ls-rmrf; echo $?` prints `0` as the last line

#### Phase 2: permission-correct removal
**Model:** opus
- Add `escalation_allowed`, `remove_path`; `remove_targets` gains `sudo`, calls `remove_path` for every target, collects `(path, error)`; on any failure prints each to stderr and returns `Err` "`N` of `M` targets were archived but not fully removed"
- `archive` passes `sudo` through (`src/main.rs:615`)
- `cleanup`: `symlink_metadata` instead of `fs::metadata` (:208), `remove_path` for stale entries, collect failures and return one aggregate error instead of stopping at the first; delete the two old helpers and their tests
- Unit tests, prefixed so the filters below select them all:
  - `escalation_allowed_*` (all four `sudo` x root combos)
  - `remove_path_unwritable_parent_without_sudo_errors` (user `0555` parent; error names the parent; file survives)
  - `remove_path_removes_dangling_symlink`
  - `remove_path_missing_target_is_ok`
  - `remove_targets_continues_past_failure`
  - `cleanup_removes_stale_dangling_symlink` (dangling link in the bin with an old `lstat` mtime, set via `libc::utimensat` with `AT_SYMLINK_NOFOLLOW`, is removed; no error)
  - `cleanup_continues_past_failure` (stale bundle in a `0555` sub-dir with `sudo=false` fails; a second stale bundle is still removed; `Err` returned)
- Tests that depend on EACCES assert `getuid() != 0` first, with a message: root bypasses directory permissions, so the test would pass vacuously
- **Success criteria:**
  - `cargo test remove_path` reports at least `3 passed`, `cargo test escalation_allowed` at least `4 passed`, both `0 failed`
  - `cargo test remove_targets_continues_past_failure`, `cargo test cleanup_removes_stale_dangling_symlink`, `cargo test cleanup_continues_past_failure` each report exactly `1 passed`, `0 failed`. Full names, because the bare prefixes `remove_targets` and `cleanup_` already match 1 and 7 existing tests on `main`
  - `cargo test dangling_symlink` reports `0 failed`, and `test_categorize_paths_dangling_symlink`, `test_archive_and_remove_dangling_symlink`, `test_rmrf_dangling_symlink` each print `... ok`
  - `rg -c 'fn remove_(file|directory)_with_sudo' src/main.rs` prints nothing and exits 1

#### Phase 3: end-to-end tests through the binary
**Model:** sonnet
- `tests/integration_tests.rs`, reusing `create_config` (gain a `sudo` argument):
  - Fake `sudo`: a script first on `PATH` that runs `printf '%s\n' "$@" >> "$SUDO_LOG"` (one argv entry per line) and exits `$SUDO_RC`; `SUDO_USER` removed from env. The archive step does not call `sudo` in these tests (targets are user-owned), so any extra lines are a regression
  - `rmrf_escalates_with_sudo_on_eacces`: `sudo = yes`, `SUDO_RC=0`; target is a file `a b.txt` (space in the name) in a `0555` user dir, absolute path. Assert exit 0 and the log is exactly the four lines `rm`, `-f`, `--`, `<target>`
  - `rmrf_escalates_directory_with_rm_rf`: same, target is a directory. Log is exactly `rm`, `-rf`, `--`, `<target>`
  - `rmrf_sudo_failure_reports_survivor`: `sudo = yes`, `SUDO_RC=1`; targets: the `0555`-parent file first, then a removable file. Assert exit 1, first target present, its bundle still in the bin, stderr names it, second target gone
  - `rmrf_continues_past_failure`: `sudo = no`; absolute paths `src/a.txt` then `src2/c.txt`, `src` is `0555`. Assert exit 1, `src2/c.txt` gone, `src/a.txt` present, stderr names `src/a.txt`. CLI order puts the failing target FIRST, so the test proves continuation, not luck
  - Absolute paths because `run_rkvr_command` does not set a working dir
  - Each test restores `0755` before the tempdir drops
- **Success criteria:**
  - `cargo test --test integration_tests rmrf_escalates` reports exactly `2 passed`; `rmrf_sudo_failure_reports_survivor` and `rmrf_continues_past_failure` each exactly `1 passed`
  - `otto ci` exits 0

## Acceptance Criteria

- [ ] With `sudo = no`, `rkvr rmrf src/a.txt src2/c.txt` (`src` is `0555`) exits 1, removes `src2/c.txt`, keeps `src/a.txt`, and names `src/a.txt` on stderr
  - Command: the baseline script under Observed, run against the built binary
  - Observed on main: exit 1, `Error: Permission denied (os error 13)` at `src/main.rs:537:13`; BOTH `src/a.txt` and `src2/c.txt` still present; 2 bundles in the bin (stops at the first target)
- [ ] With `sudo = yes`, the same unwritable-parent target goes through exactly one `sudo rm -f -- <target>`, and a failing `sudo` leaves that target and its bundle in place with exit 1
  - Command: `cargo test --test integration_tests rmrf_escalates` reports exactly `2 passed` (`rmrf_escalates_with_sudo_on_eacces`, `rmrf_escalates_directory_with_rm_rf`); `cargo test --test integration_tests rmrf_sudo_failure_reports_survivor` reports exactly `1 passed`
  - Observed on main: `cargo test <name> -- --list` matches 0 tests for `rmrf_escalates` and `rmrf_sudo_failure` (no collision with existing names). Cannot pass yet; the tests are added in Phase 3, and the behavior they guard is absent (`remove_targets` never calls `sudo`, `src/main.rs:532-541`)
- [ ] A missing `rmrf.cfg` no longer aborts: `HOME=$(mktemp -d) RMRF_PAGER=cat rkvr ls-rmrf; echo $?` ends with `0`
  - Observed on main: `Error: Failed to load config ... couldn't read <tmp>/.config/rmrf/rmrf.cfg: No such file or directory (os error 2)` at `src/main.rs:870:22`, exit 1
- [ ] The dangling-symlink fix survives: `cargo test dangling_symlink` reports `0 failed`, and the three existing tests `test_categorize_paths_dangling_symlink`, `test_archive_and_remove_dangling_symlink`, `test_rmrf_dangling_symlink` each print `... ok` (the count rises once Phase 2 adds `remove_path_removes_dangling_symlink`, so no exact count)
  - Observed on main: all three `... ok`; `0 failed` in every test binary
- [ ] One removal path: `rg -c 'fn remove_(file|directory)_with_sudo' src/main.rs` prints nothing (exit 1), and `otto ci` exits 0
  - Observed on main: `rg` prints `2`; `otto ci` exits 0 with `TMPDIR` set to a non-symlinked dir (exits 1 under this session's symlinked `TMPDIR`, see Non-Goals)

## Resolved Decisions

- 2026-11-16, escalation trigger = the kernel's answer (try, then escalate on `PermissionDenied`), not a prediction. Owner uid is the wrong test; `access(parent, W_OK)` plus a sticky-bit rule is a second model of the kernel that still misses nested root-owned content inside a directory target
- 2026-11-16, partial failure = best-effort removal with an aggregate error, not a preflight. Every target is already archived before removal starts (because `archive` is fail-fast, which stays), so continuing is safe; a preflight would need the prediction rejected above. Both panel seats agreed (round 1)
- 2026-11-16, `cleanup` moves to the same `remove_path`, `symlink_metadata`, and the same best-effort aggregation. Siblings behave identically; the old helpers carry the owner-uid, symlink-follow and missing-`--` defects, and `cleanup`'s own `fs::metadata` (:208) carried the symlink-follow one too (staff seat, round 1)
- 2026-11-16, an already-absent target counts as removed. The goal is absence; reporting `dir/child` as a survivor after `dir` took it would be false (staff seat, round 1)
- 2026-11-16, a dangling `rmrf.cfg` symlink is an error, not defaults. `rmrf.cfg` is normally a dotfiles symlink; a broken one is misconfiguration, and silently running on defaults is the failure this doc exists to stop
- 2026-11-16, panel round 1 dispositions not folded as design changes: `SUDO_USER` trusted when root is the intended boundary (architect, no action); per-phase `otto ci` is already `/plan:build`'s per-phase gate (staff, no doc change); rollback note added
- 2026-11-16, `sudo` runs interactively, not `sudo -n` (architect asked for `-n`; rejected in round 1). The password prompt is the intended experience; with no tty `sudo` exits non-zero, which is a per-target failure
- 2026-11-16, no escalation when already root (`uid == 0`). `sudo` cannot add permission there; an EPERM as root (immutable file, read-only mount) is reported, not retried
- 2026-11-16, Bug 2 includes the `SUDO_USER` home lookup, not just defaults. The owner ran `sudo -E` to keep their config; defaults alone silently change `keep` from 21 to 30
- 2026-11-16, EACCES tests fail loudly as root instead of skipping. CI runs `ubuntu-latest` as the non-root `runner` user (`rust-ci.yml@v2.4.2`, no `container:`), so the guard never trips there

## Alternatives Considered

### Alternative 1: Wire the existing owner-uid helpers into `remove_targets`
- **Description:** pass `sudo` to `remove_targets`, call `remove_file_with_sudo` / `remove_directory_with_sudo`
- **Pros:** smallest diff
- **Cons:** owner uid is the wrong test (user file in a root dir: no escalation; root file in a user dir: needless `sudo`); follows symlinks, regressing `test_rmrf_dangling_symlink`
- **Why not chosen:** fixes the reported case by accident, breaks the prior fix

### Alternative 2: Predictive preflight before archiving
- **Description:** `access(parent, W_OK|X_OK)` + sticky rule per target; with `sudo=no`, refuse before touching the bin
- **Pros:** no bin entries for targets that will survive
- **Cons:** second model of kernel permissions; top-level only (a directory with root-owned contents still fails midway); still needs runtime handling for `sudo` failures (wrong password, cancelled prompt)
- **Why not chosen:** adds a signal that can disagree with the kernel and does not remove the need for the runtime path

### Alternative 3: Bug 2 as "missing file = defaults" only
- **Description:** skip the `SUDO_USER` lookup
- **Pros:** no FFI
- **Cons:** `sudo rkvr` silently ignores the owner's config (`keep`, paths)
- **Why not chosen:** wrong values without an error

## Technical Considerations

### Dependencies

- `libc` (already direct, `getuid` at `src/main.rs:2`): `getpwnam_r`
- External `sudo` and `rm` on `PATH`, same as today's `cleanup` and `create_tar_command`

### Blast Radius

- This repo only. No consumer crates
- Every `archive(..., remove = true, ...)` caller gets the new removal: `rmrf` (`src/main.rs:909`), `bkup-rmrf` (:921) and the default no-subcommand invocation (:925). Same behavior in all three; `bkup-rmrf` passes `keep = None`, so it never reaches `cleanup`
- Dotfiles: none required. The dead `~/.config/rkvr/rkvr.{yml,cfg}` stay untouched (Non-Goals)
- Ship order: single PR, then release, then install

### Performance

- Happy path unchanged: one unlink per target, no extra syscalls before it
- Escalation adds one `sudo` process per refused target

### Security

- Escalation only after the kernel refuses, only with `sudo=yes`, only for paths the user named on the command line (or bundles under `rmrf_path` for cleanup)
- `--` before the path closes the leading-dash option injection in `sudo rm`
- `rm` does not follow a symlink operand, so escalating on a link removes the link, not its target
- `-r` only when `symlink_metadata` saw a real directory; a file swapped for a directory between the check and the escalation makes `rm -f` refuse rather than recurse
- `SUDO_USER` is trusted only when `uid == 0`, which is what `sudo` sets it under

### Testing Strategy

- Unit: pure seams (`escalation_allowed`, `rmrf_cfg_home` with injected lookup) cover branches without root; the `cargo mutants` PR workflow scores them
- Unit + integration: `0555` user-owned dirs reproduce EACCES unprivileged
- Integration: fake `sudo` on `PATH` proves the exact escalation argv without real privilege
- Local runs in this session need `TMPDIR` pointed at a non-symlinked dir (see Non-Goals)

### Rollout Plan

- Release per `bump --gates`; install the binary
- Live proof (operator, needs a root-owned target): `sudo touch /opt/rkvr-probe`, then `rkvr rmrf /opt/rkvr-probe; echo $?` -> `0`, `/opt/rkvr-probe` gone, bundle listed by `RMRF_PAGER=cat rkvr ls-rmrf rkvr-probe`
- Live proof for Bug 2: `sudo RMRF_PAGER=cat $(which rkvr) ls-rmrf; echo $?` -> `0`, then `sudo rg 'keep for days' /root/.local/share/rkvr/logs/rkvr.log | tail -1` -> ends `keep for days: 21, threshold: 70` (the owner's `rmrf.cfg`, not the fallback 30)
- Rollback: reinstall the previous release binary; no on-disk format changed

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `sudo` prompts mid-run, user cancels | Med | Low | non-zero `sudo` status is a per-target failure; other targets still removed; survivors listed |
| Partial `remove_dir_all` before escalation | Med | Low | `sudo rm -rf` finishes whatever is left; content is already archived |
| EACCES tests run as root pass vacuously | Low | Med | `getuid() != 0` assertion fails the test loudly |
| `getpwnam_r` buffer too small (`ERANGE`) | Low | Low | retry with a doubled buffer up to a cap; error names the user |
| `sudo` with no tty (cron, piped) cannot prompt | Low | Low | `sudo` exits non-zero -> per-target failure, survivors listed, exit 1 |
| Not in sudoers | Low | Low | same as above; `sudo = no` in `rmrf.cfg` turns escalation off |

## Open Questions

(none)

## References

- `docs/2026-09-14-dangling-symlink.md`
- `src/main.rs`: `remove_targets`, `archive`, `cleanup`, `create_tar_command`, `main` config block
- `tests/integration_tests.rs`: `create_config`, `run_rkvr_command`, `test_rmrf_dangling_symlink`
- CI: `tatari-tv/github-actions-public/.github/workflows/rust-ci.yml@v2.4.2` (`runs-on: ubuntu-latest`)
