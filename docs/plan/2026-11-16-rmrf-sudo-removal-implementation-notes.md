# Implementation Notes: rmrf removal honors sudo, and a missing rmrf.cfg is not fatal

Design doc: `docs/plan/2026-11-16-rmrf-sudo-removal.md`

## Phase 1: rmrf.cfg loader

### Design decisions
- `load_rmrf_cfg` treats only `NotFound` from `symlink_metadata` as "nothing there" (defaults + `info!` naming the path). Any other `symlink_metadata` error (`EACCES` on `~/.config/rmrf`, `ENOTDIR`) is a loud error naming the path: `src/main.rs:load_rmrf_cfg`, fail closed. The doc only specified the `NotFound` branch.
- `load_rmrf_cfg` wraps read and parse failures separately ("Failed to read rmrf config <path>" | "Failed to parse rmrf config <path>"), so the error says which step broke: `src/main.rs:load_rmrf_cfg`.
- `passwd_home` rejects a user name containing a NUL byte before calling libc (`CString::new` cannot carry it): `src/main.rs:passwd_home`.
- `passwd_home` clamps the `sysconf(_SC_GETPW_R_SIZE_MAX)` starting size to the 1 MiB cap and doubles up to the cap before erroring on `ERANGE`: `src/main.rs:PASSWD_BUF_MAX`.
- A `getpwnam_r` that returns 0 with a non-null result but a null `pw_dir`, or 0 with a null result outside the not-found set, is an error.

### Deviations
- `passwd_home` maps `ENOENT`, `ESRCH`, `EBADF`, `EPERM` (with a null result) to `Ok(None)`, not only `rc == 0`. The doc says "no such user (`getpwnam_r` returns 0 with a null result) -> `Ok(None)`".
  - Measured on this machine: `/etc/nsswitch.conf` has `passwd: files systemd sss`, `sssd` is inactive, `/var/lib/sss/pipes/nss` is absent. glibc returns the trailing `sss` module's `ENOENT` for an unknown user, and `rmrf_cfg_passwd_home_unknown_user_is_none` failed with `getpwnam_r(rkvr-no-such-user-zz9) failed: No such file or directory (os error 2)`.
  - getpwnam_r(3) ERRORS documents exactly "0 or ENOENT or ESRCH or EBADF or EPERM or ... The given name or uid was not found."
  - Other errnos (`EIO`, `EINTR`, `EMFILE`, `ENFILE`, `ENOMEM`, `ERANGE` past the cap) stay errors.
  - In `rmrf_cfg_home` an unknown `SUDO_USER` is an error either way; only the message differs ("has no passwd entry" vs an errno).
- Three tests beyond the ten listed, all `rmrf_cfg_` prefixed: `rmrf_cfg_present_file_is_read` (happy path for `load_rmrf_cfg`, missing from the list), `rmrf_cfg_home_without_sudo_user_and_no_fallback_is_error` (the "home dir not found!" branch), `rmrf_cfg_passwd_home_nul_in_name_is_error`. `cargo test rmrf_cfg` reports 13 passed.

### Tradeoffs
- Injected `lookup` in `rmrf_cfg_home` vs calling `passwd_home` directly: injected, per API Design, so every branch is unit-tested without root or a live `SUDO_USER`.
- `rmrf_cfg_unreadable_is_error` asserts `getuid() != 0` and fails loudly as root instead of skipping, matching the doc's rule for EACCES tests.

### Open questions
- None.

## Phase 2: permission-correct removal

### Design decisions
- `remove_path` errors always name the path, so `remove_targets` and `cleanup` print each failure as-is (`{:#}`) instead of prefixing the path again: `src/main.rs:remove_targets`, `src/main.rs:cleanup`. A survivor prints as `permission denied removing src/a.txt (parent: src)`.
- `sudo rm` is split out as `sudo_rm(path, is_dir)` and passes the path as an `OsStr` argument (`.arg(path)`), not `to_string_lossy`, so a non-UTF-8 name reaches `rm` byte-exact: `src/main.rs:sudo_rm`.
- A relative target with no parent component reports `(parent: .)`: `src/main.rs:parent_display`.
- `cleanup`'s per-entry body moved to `cleanup_entry`, so every per-entry failure (unreadable `read_dir` entry, `lstat` error, mtime error, removal error) lands in the aggregate instead of `?`-aborting the loop. An entry that vanished before its `lstat` is skipped (same "absence is the goal" rule). A failed `read_dir` of the bin itself stays fatal: there is nothing to walk. The old `entries.flatten()` silently dropped unreadable entries; those are now counted.
- `cleanup`'s aggregate error reads "`N` stale entries in `<bin>` were not fully removed", mirroring `remove_targets`'s "`N` of `M` targets were archived but not fully removed".

### Deviations
- Three tests beyond the list: `remove_path_removes_symlink_not_its_target_dir` (the link goes, the target dir stays), `remove_path_removes_directory_tree` (happy path for the directory branch), `remove_targets_dir_and_its_child_is_ok` (the doc's `rkvr rmrf dir dir/child` case). `cargo test remove_path` reports 5 passed, not 3.
- `test_remove_targets` (existing) now passes `sudo = false`; behavior unchanged.
- The escalation branch (`sudo rm` invoked) has no unit test: it needs a fake `sudo` on `PATH`, which is Phase 3's integration harness. `escalation_allowed` covers the gate in all four combos.

### Tradeoffs
- `cleanup_continues_past_failure` makes the stale bundle unremovable with a `0555` directory INSIDE it (`<bundle>/locked/file.txt`), not by making the bin `0555`: a `0555` bin would block the second bundle too and prove nothing about continuation. Both stale entries are back-dated 10 days with `utimensat(AT_SYMLINK_NOFOLLOW)` and `cleanup` runs with `days = 1`, so the test does not lean on the `days = 0` "anything older than now" edge.
- Bite check done by hand: reverting `cleanup_entry` to `fs::metadata` fails `cleanup_removes_stale_dangling_symlink`; a fail-fast `?` loop in `remove_targets` fails `remove_targets_continues_past_failure`.

### Open questions
- None.

## Phase 3: end-to-end tests through the binary

### Design decisions
- Fake `sudo` lives in a `FakeSudo` helper (script in `<tmp>/fakebin`, log in `<tmp>/sudo.log`): `tests/integration_tests.rs:FakeSudo`. It prepends `fakebin` to the child's `PATH`, sets `SUDO_LOG` and `SUDO_RC`, and `env_remove("SUDO_USER")`. `run_rkvr_command` is untouched, so the existing tests keep their exact behavior.
- `create_config` gained `sudo: bool` and writes `yes`/`no`; the 7 existing callers pass `false`, so their config is byte-identical to before.
- EACCES tests call `assert_not_root` first, failing loudly with a message instead of passing vacuously as root.
- Every test restores `0755` on the locked directory right after the binary returns, before any assertion can panic, so the tempdir always drops cleanly.
- `rmrf_sudo_failure_reports_survivor` finds the survivor's bundle by scanning bundle `metadata.yml` for `a.txt`, rather than assuming a bundle index.

### Deviations
- None.

### Tradeoffs
- A dedicated `FakeSudo::run` vs adding env parameters to `run_rkvr_command`: dedicated, so the shared helper's signature and the existing tests do not change.

### Bite check
- Dropping `.arg("--")` from `sudo_rm` fails `rmrf_escalates_with_sudo_on_eacces` and `rmrf_escalates_directory_with_rm_rf`.
- Replacing `Command::new("sudo")` with `Command::new("true")` fails both escalation tests and `rmrf_sudo_failure_reports_survivor`. `rmrf_continues_past_failure` (sudo = no) correctly stays green, as it never escalates.
- `src/main.rs` restored byte-exact (`git diff` empty).

### Open questions
- None.

## Implementation audit (panel round 2, Mode 2)

### Design decisions
- `cleanup_continues_past_failure` (`src/main.rs` tests) hardened per the audit's one cheap win: two refused stale bundles around one removable one, asserting `2 stale entries` and the removable bundle gone. With only one refused bundle, a `cleanup` that records the first error then breaks passed whenever `read_dir` yielded the removable bundle first. Bite check: adding `break` after `failures.push(e)` in `cleanup` fails the test (`0 passed; 1 failed`); restored.

### Deviations
None.

### Tradeoffs
- Two refused bundles vs. sorting `read_dir` output in `cleanup`: the test change fixes the order dependence without changing production behavior.

### Open questions
None. Deferred audit item: two untouched tests fail under a symlinked `TMPDIR`, already parked in the doc's Non-Goals.
