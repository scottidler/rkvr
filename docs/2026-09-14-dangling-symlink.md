# rkvr rmrf fails on a dangling symlink

Found 2026-09-14 while building chunk B of the claude setup-audit program, which left
`~/.claude/hooks/git-no-dash-c.sh` as a symlink whose target the branch deleted.

## Symptom

```
$ rkvr rmrf ~/.claude/hooks/git-no-dash-c.sh
Error: /home/saidler/.claude/hooks/git-no-dash-c.sh: No such file or directory

Location:
    src/main.rs:468:17
```

- rkvr v0.1.22-1-g02049d3
- The link exists (`test -L` is true); only its target is gone.

## Cause

- `categorize_paths` (`src/main.rs:466`) runs `fs::canonicalize(target)` on every target.
- `canonicalize` follows symlinks, so a dangling link resolves to a path that does not exist and comes back `NotFound`.
- The error is then reported as if the target path itself were missing.

## Fix shape

- Stat the link, not what it points to: `fs::symlink_metadata(target)` before `canonicalize`, and treat `file_type().is_symlink()` as a first-class case.
- For a symlink, archive the link itself (its name and its `read_link` target string) and `remove_file` the link. Never follow it: `rkvr rmrf <link>` on a live link should also archive the link, not the directory behind it, or a link into `~/repos` would archive a whole checkout.
- Canonicalize the link's parent for the relative-path bookkeeping, then append the link's file name.
- Same shape for any link handed to `bkup`: it backs up the link itself, never what it points to (`categorize_paths` is shared by both commands).

## Archive layout

- A link is not a loose copy. Like any non-archive file, it goes into the bundle's `<parent>.tar.gz` (`archive_group` partition).
- `tar` stores it as a symlink entry (`name -> target`) because rkvr never passes `-h`.

## Tests to add

- Dangling symlink: `rkvr rmrf link` exits 0, the link is gone, `<parent>.tar.gz` holds a symlink entry with the original target string, the (missing) target is untouched.
- Live symlink to a directory: `rkvr rmrf link` removes the link only; the directory and its contents are still there, and no `<dir>.tar.gz` of its contents is made.
- Live symlink to a file: same, the file survives.

## Status

- Fixed. `categorize_paths` stats with `symlink_metadata` and canonicalizes only the link's parent (`canonicalize_link`); `file_uid`, `copy_files`, and `remove_targets` act on the link, not its target.
- Unit tests in `src/main.rs`: `test_categorize_paths_dangling_symlink`, `test_archive_and_remove_dangling_symlink`, `test_archive_and_remove_symlink_to_directory`, `test_archive_and_remove_symlink_to_file`.
- Integration test in `tests/integration_tests.rs`: `test_rmrf_dangling_symlink`.
