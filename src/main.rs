// src/main.rs
use libc::getuid;
use log::{debug, info};
use std::collections::HashMap;
use std::env;
use std::fs::OpenOptions;
use std::fs::{self, DirEntry, File};
use std::io::{self, BufWriter, ErrorKind, Write};
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::time::SystemTime;
use which::which;

// Third-party crate imports
use atty::Stream;
use clap::Parser;
use colored::*;
use configparser::ini::Ini;
use env_logger::Target;
use eyre::{eyre, Context, Result};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use serde::{Deserialize, Serialize};

// Local modules
mod cli;
mod config;

use cli::{Action, Cli};
use config::Config;

static EZA_ARGS: &[&str] = &[
    "--tree",
    "--long",
    "-a",
    "--ignore-glob=.*",
    "--ignore-glob=__*",
    "--ignore-glob=tf",
    "--ignore-glob=venv",
    "--ignore-glob=target",
    "--ignore-glob=incremental",
];

#[derive(Serialize, Deserialize, Debug)]
struct Metadata {
    cwd: PathBuf,
    #[serde(default)]
    targets: Vec<String>,
    contents: String,
}

fn as_paths(paths: &[String]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|p| {
            if let Some(rest) = p.strip_prefix("~/") {
                dirs::home_dir().unwrap_or_default().join(rest)
            } else {
                PathBuf::from(p)
            }
        })
        .collect()
}

fn get_log_file_path() -> Result<PathBuf> {
    let log_dir = config::xdg_data_dir()
        .ok_or_else(|| eyre!("Could not determine local data directory"))?
        .join("rkvr")
        .join("logs");

    fs::create_dir_all(&log_dir).wrap_err_with(|| format!("Failed to create log directory: {}", log_dir.display()))?;

    Ok(log_dir.join("rkvr.log"))
}

fn setup_logging() -> Result<()> {
    let log_file_path = get_log_file_path()?;

    if env::var("RUST_LOG").is_ok() {
        env_logger::init();
        info!("Using RUST_LOG environment variable for logging configuration");
        info!("Log file location: {}", log_file_path.display());
        return Ok(());
    }

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file_path)
        .wrap_err_with(|| format!("Failed to open log file: {}", log_file_path.display()))?;

    env_logger::Builder::from_env(env_logger::Env::default().filter_or("RUST_LOG", "info"))
        .format(|buf, record| {
            use std::io::Write;
            writeln!(
                buf,
                "{} [{}] [{}] {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .target(Target::Pipe(Box::new(log_file)))
        .init();

    info!("Logging initialized - file: {}", log_file_path.display());
    Ok(())
}

fn current_uid() -> u32 {
    unsafe { getuid() as u32 }
}

/// Upper bound for the `getpwnam_r` scratch buffer; an entry that needs more is an error.
const PASSWD_BUF_MAX: usize = 1 << 20;

/// Home directory of `user` from the passwd database.
/// `Ok(None)` = no such user; `Err` = lookup failure or an unusable `pw_dir`.
fn passwd_home(user: &str) -> Result<Option<PathBuf>> {
    use std::ffi::{CStr, CString, OsStr};
    use std::os::unix::ffi::OsStrExt;

    debug!("passwd_home: user={}", user);
    let c_user = CString::new(user).map_err(|_| eyre!("user name {:?} contains a NUL byte", user))?;

    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut buf_len = if suggested > 0 { (suggested as usize).min(PASSWD_BUF_MAX) } else { 1024 };

    loop {
        let mut buf: Vec<libc::c_char> = vec![0; buf_len];
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer refers to a live local; `buf.len()` is the real buffer size.
        let rc = unsafe { libc::getpwnam_r(c_user.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };

        if rc == libc::ERANGE {
            if buf_len >= PASSWD_BUF_MAX {
                return Err(eyre!(
                    "getpwnam_r({}) failed: {} (buffer cap {} bytes reached)",
                    user,
                    io::Error::from_raw_os_error(rc),
                    PASSWD_BUF_MAX
                ));
            }
            buf_len = (buf_len * 2).min(PASSWD_BUF_MAX);
            debug!("passwd_home: ERANGE, retrying with {} bytes", buf_len);
            continue;
        }
        // getpwnam_r(3) ERRORS: "0 or ENOENT or ESRCH or EBADF or EPERM ... The given name
        // or uid was not found." glibc passes through the last NSS module's errno, e.g.
        // ENOENT from `sss` when sssd's socket is absent.
        if result.is_null() && matches!(rc, 0 | libc::ENOENT | libc::ESRCH | libc::EBADF | libc::EPERM) {
            debug!("passwd_home: no passwd entry for {} (rc={})", user, rc);
            return Ok(None);
        }
        if rc != 0 {
            return Err(eyre!(
                "getpwnam_r({}) failed: {}",
                user,
                io::Error::from_raw_os_error(rc)
            ));
        }
        if result.is_null() {
            return Err(eyre!("getpwnam_r({}) returned success without an entry", user));
        }
        if pwd.pw_dir.is_null() {
            return Err(eyre!("passwd entry for {} has no home directory", user));
        }

        // SAFETY: `pw_dir` points into `buf`, which is alive; the bytes are copied out here.
        let bytes = unsafe { CStr::from_ptr(pwd.pw_dir) }.to_bytes();
        let home = PathBuf::from(OsStr::from_bytes(bytes));
        if home.as_os_str().is_empty() || home.is_relative() {
            return Err(eyre!(
                "passwd entry for {} has an unusable home directory {:?}",
                user,
                home
            ));
        }
        debug!("passwd_home: {} -> {}", user, home.display());
        return Ok(Some(home));
    }
}

/// Home directory whose `.config/rmrf/rmrf.cfg` applies: the invoking user's when running
/// as root under `sudo` (`SUDO_USER` set and not `root`), otherwise `fallback`.
fn rmrf_cfg_home(
    uid: u32,
    sudo_user: Option<&str>,
    lookup: impl Fn(&str) -> Result<Option<PathBuf>>,
    fallback: Option<PathBuf>,
) -> Result<PathBuf> {
    debug!("rmrf_cfg_home: uid={} sudo_user={:?}", uid, sudo_user);
    match sudo_user {
        Some(user) if uid == 0 && user != "root" => {
            lookup(user)?.ok_or_else(|| eyre!("SUDO_USER={} has no passwd entry", user))
        }
        _ => fallback.ok_or(eyre!("home dir not found!")),
    }
}

/// Read `rmrf.cfg`. Nothing at `path` = an empty `Ini` (every key has a default);
/// anything there that cannot be read, a dangling symlink included, is an error.
fn load_rmrf_cfg(path: &Path) -> Result<Ini> {
    debug!("load_rmrf_cfg: path={}", path.display());
    let mut ini = Ini::new();
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            info!("No rmrf config at {}, using defaults", path.display());
            return Ok(ini);
        }
        Err(e) => {
            return Err(e).wrap_err_with(|| format!("Failed to stat rmrf config {}", path.display()));
        }
        Ok(_) => {}
    }
    let contents =
        fs::read_to_string(path).wrap_err_with(|| format!("Failed to read rmrf config {}", path.display()))?;
    ini.read(contents)
        .map_err(|e| eyre!(e))
        .wrap_err_with(|| format!("Failed to parse rmrf config {}", path.display()))?;
    debug!("load_rmrf_cfg: loaded {}", path.display());
    Ok(ini)
}

fn file_uid(path: &Path) -> eyre::Result<u32> {
    Ok(fs::symlink_metadata(path)?.uid())
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Canonicalize a symlink's parent and rejoin its file name, so the link
/// itself is addressed and its target (possibly missing) is never followed.
fn canonicalize_link(link: &Path) -> io::Result<PathBuf> {
    let parent = match link.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = link
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "symlink has no file name"))?;
    Ok(fs::canonicalize(parent)?.join(name))
}

/// Whether a removal the kernel refused may be retried through `sudo rm`.
/// Never as root: `sudo` adds nothing there, and an EPERM as root is reported, not retried.
fn escalation_allowed(sudo: bool, uid: u32) -> bool {
    sudo && uid != 0
}

/// Remove `path` without following it: a symlink is unlinked, a real directory is removed
/// recursively. A path already gone counts as removed. When the kernel refuses with
/// `PermissionDenied` and escalation is allowed, retry as `sudo rm -f|-rf -- <path>`.
fn remove_path(path: &Path, sudo: bool) -> Result<()> {
    debug!("remove_path: path={} sudo={}", path.display(), sudo);
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            debug!("remove_path: {} already gone", path.display());
            return Ok(());
        }
        Err(e) => return Err(e).wrap_err_with(|| format!("Failed to stat {}", path.display())),
    };
    let is_dir = meta.is_dir();
    let plain = if is_dir { fs::remove_dir_all(path) } else { fs::remove_file(path) };
    match plain {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            debug!("remove_path: {} vanished during removal", path.display());
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::PermissionDenied && escalation_allowed(sudo, current_uid()) => {
            sudo_rm(path, is_dir)
        }
        Err(e) if e.kind() == ErrorKind::PermissionDenied => Err(eyre!(
            "permission denied removing {} (parent: {})",
            path.display(),
            parent_display(path)
        )),
        Err(e) => Err(e).wrap_err_with(|| format!("Failed to remove {}", path.display())),
    }
}

/// The directory whose permissions govern unlinking `path`, for error messages.
fn parent_display(path: &Path) -> String {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.display().to_string(),
        _ => ".".to_string(),
    }
}

/// `sudo rm -f -- <path>` (`-rf` only for a real directory). `rm` never follows a symlink
/// operand, and `--` keeps a leading-dash path from being read as an option.
fn sudo_rm(path: &Path, is_dir: bool) -> Result<()> {
    let flags = if is_dir { "-rf" } else { "-f" };
    info!("Escalating: sudo rm {} -- {}", flags, path.display());
    let status = Command::new("sudo")
        .arg("rm")
        .arg(flags)
        .arg("--")
        .arg(path)
        .status()
        .wrap_err_with(|| format!("Failed to run sudo rm for {}", path.display()))?;
    if !status.success() {
        return Err(eyre!("sudo rm {} -- {} failed ({})", flags, path.display(), status));
    }
    Ok(())
}

/// Remove every bin entry older than `days`, judged by the entry itself (never a symlink's
/// target). Attempts every entry; any failures are printed and returned as one error.
fn cleanup(dir_path: &std::path::Path, days: usize, sudo: bool) -> Result<()> {
    info!(
        "fn cleanup: dir_path={} days={} sudo={}",
        dir_path.to_string_lossy(),
        days,
        sudo
    );

    let now = SystemTime::now();
    debug!("Current time: {:?}", now);

    let delete_threshold = std::time::Duration::from_secs(60 * 60 * 24 * days as u64);
    debug!("Delete threshold duration: {:?} ({} days)", delete_threshold, days);

    let entries =
        fs::read_dir(dir_path).wrap_err_with(|| format!("Failed to read bin directory {}", dir_path.display()))?;

    let mut failures: Vec<eyre::Report> = Vec::new();
    for entry in entries {
        match cleanup_entry(entry, now, delete_threshold, sudo) {
            Ok(()) => {}
            Err(e) => failures.push(e),
        }
    }

    if !failures.is_empty() {
        for failure in &failures {
            eprintln!("{:#}", failure);
        }
        return Err(eyre!(
            "{} stale entries in {} were not fully removed",
            failures.len(),
            dir_path.display()
        ));
    }

    info!("Cleanup completed");
    Ok(())
}

/// One bin entry of `cleanup`: remove it when its own (`lstat`) mtime is past the threshold.
fn cleanup_entry(
    entry: io::Result<DirEntry>,
    now: SystemTime,
    delete_threshold: std::time::Duration,
    sudo: bool,
) -> Result<()> {
    let path = entry.wrap_err("Failed to read bin directory entry")?.path();
    debug!("Checking path: {}", path.to_string_lossy());

    let metadata = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).wrap_err_with(|| format!("Failed to stat {}", path.display())),
    };
    let modified_time = metadata
        .modified()
        .wrap_err_with(|| format!("Failed to read mtime of {}", path.display()))?;
    debug!("Modified time: {:?}", modified_time);

    if let Ok(duration_since_modified) = now.duration_since(modified_time) {
        debug!(
            "Duration since modified: {:?}, Delete threshold: {:?}",
            duration_since_modified, delete_threshold
        );
        if duration_since_modified > delete_threshold {
            info!("Deleting path: {}", path.to_string_lossy());
            remove_path(&path, sudo)?;
        }
    }
    Ok(())
}

fn resolve_eza_path() -> Result<String> {
    // First try the normal which lookup
    if let Ok(path) = which("eza") {
        return Ok(path.to_string_lossy().to_string());
    }

    // If we're running under sudo, try to find eza as the original user
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        // Run 'which eza' as the original user with their login environment
        let output = Command::new("sudo")
            .args(["-u", &sudo_user, "-i", "which", "eza"])
            .output();

        if let Ok(output) = output {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() && std::path::Path::new(&path).exists() {
                    return Ok(path);
                }
            }
        }
    }

    eyre::bail!("Could not find eza command. Please install eza: https://github.com/eza-community/eza")
}

fn create_metadata(base: &Path, cwd: &Path, targets: &[PathBuf]) -> Result<()> {
    info!(
        "fn create_metadata: base={} cwd={} targets={:?}",
        base.display(),
        cwd.display(),
        targets
    );

    let eza_tree = resolve_eza_path()?;
    let output = Command::new(&eza_tree)
        .args(EZA_ARGS)
        .args(targets.iter().map(|t| t.to_str().unwrap()))
        .output()
        .wrap_err("Failed to execute eza command")?;

    let metadata_content = String::from_utf8_lossy(&output.stdout);
    debug!("Metadata content: {}", metadata_content);

    let target_names: Vec<String> = targets
        .iter()
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string())
        })
        .collect();

    let metadata = Metadata {
        cwd: cwd.to_path_buf(),
        contents: metadata_content.to_string(),
        targets: target_names,
    };

    let yaml_metadata = serde_yaml::to_string(&metadata).wrap_err("Failed to serialize metadata to YAML")?;
    let metadata_path = base.join("metadata.yml");
    fs::write(&metadata_path, yaml_metadata.as_bytes()).wrap_err("Failed to write metadata file")?;
    Ok(())
}

fn create_tar_command(sudo: bool, tarball_path: &Path, cwd: &Path, targets: Vec<String>) -> Result<Command> {
    let relative_targets: Vec<String> = targets
        .into_iter()
        .map(|t| {
            let target_path = Path::new(&t);
            if target_path.is_absolute() {
                target_path
                    .strip_prefix(cwd)
                    .map(|rel| rel.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| {
                        target_path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| t.clone())
                    })
            } else {
                t
            }
        })
        .collect();

    if sudo {
        let mut cmd = Command::new("sudo");
        cmd.args([
            "tar",
            "-czf",
            tarball_path.to_str().unwrap(),
            "-C",
            cwd.to_str().unwrap(),
        ]);
        cmd.args(&relative_targets);
        Ok(cmd)
    } else {
        let mut cmd = Command::new("tar");
        cmd.args(["-czf", tarball_path.to_str().unwrap(), "-C", cwd.to_str().unwrap()]);
        cmd.args(&relative_targets);
        Ok(cmd)
    }
}

fn archive_directory(base: &Path, target: &PathBuf, sudo: bool, cwd: &Path) -> Result<()> {
    let owner = fs::metadata(target)?.uid();
    let need_sudo = owner != current_uid();
    if need_sudo && !sudo {
        eyre::bail!(
            "Directory {} is owned by uid={} but sudo is disabled; enable sudo in config",
            target.display(),
            owner
        );
    }

    let dir_name = target
        .file_name()
        .ok_or_else(|| eyre!("Failed to extract directory name"))?
        .to_string_lossy()
        .into_owned();
    let tarball_path = base.join(format!("{}.tar.gz", dir_name));

    let rel = target
        .strip_prefix(cwd)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| target.to_string_lossy().into_owned())
        });

    let mut cmd = create_tar_command(need_sudo, &tarball_path, cwd, vec![rel])?;
    let status = cmd.status()?;
    if !status.success() {
        eyre::bail!("Failed to archive {} (status {})", target.display(), status);
    }

    Ok(())
}

fn is_archive(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()) {
        matches!(ext.as_str(), "tar" | "gz" | "tgz" | "xz" | "zip" | "7z")
    } else {
        false
    }
}

fn copy_files(base: &Path, loose: &[PathBuf], sudo: bool) -> Result<()> {
    let me = current_uid();
    for src in loose {
        let fname = src.file_name().unwrap();
        let dest = base.join(fname);
        let meta = fs::symlink_metadata(src)?;
        let owner = meta.uid();
        if owner == me {
            if meta.file_type().is_symlink() {
                symlink(fs::read_link(src)?, &dest)?;
            } else {
                fs::copy(src, &dest)?;
                fs::set_permissions(&dest, meta.permissions())?;
            }
        } else {
            if !sudo {
                eyre::bail!(
                    "Not permitted to copy {} (owned by uid={}); enable sudo in config",
                    src.display(),
                    owner
                );
            }
            let status = Command::new("sudo")
                .args(["cp", "-a", src.to_str().unwrap(), dest.to_str().unwrap()])
                .status()?;
            if !status.success() {
                eyre::bail!("`sudo cp -a` failed with status {}", status);
            }
        }
    }
    Ok(())
}

fn tar_gz_files(base: &Path, group: &[PathBuf], sudo: bool, cwd: &Path) -> Result<()> {
    let parent_name = group[0]
        .parent()
        .and_then(|p| p.file_name())
        .ok_or_else(|| eyre!("Cannot determine parent dir for {:?}", group[0]))?
        .to_string_lossy()
        .into_owned();

    let tarball_path = base.join(format!("{}.tar.gz", parent_name));

    let relative_targets: Vec<String> = group
        .iter()
        .map(|p| {
            p.strip_prefix(cwd)
                .map(|r| r.to_string_lossy().into_owned())
                .unwrap_or_else(|_| {
                    p.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.to_string_lossy().into_owned())
                })
        })
        .collect();

    let mut cmd = create_tar_command(sudo, &tarball_path, cwd, relative_targets)?;
    let status = cmd.status()?;
    if !status.success() {
        eyre::bail!("Failed to create {} (status {})", tarball_path.display(), status);
    }

    Ok(())
}

fn archive_group(base: &Path, group: &[PathBuf], sudo: bool, cwd: &Path) -> Result<()> {
    let need_sudo = group
        .iter()
        .map(|p| file_uid(p))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .any(|uid| uid != current_uid());

    if need_sudo && !sudo {
        eyre::bail!("Found files owned by another user; re‑run with `sudo = yes` in your config");
    }

    let (bundle, loose): (Vec<_>, Vec<_>) = group.iter().cloned().partition(|path| !is_archive(path));

    if !loose.is_empty() {
        copy_files(base, &loose, need_sudo)?;
    }

    if !bundle.is_empty() {
        tar_gz_files(base, &bundle, need_sudo, cwd)?;
    } else {
        debug!("No files to bundle for this group.");
    }

    Ok(())
}

fn categorize_paths(targets: &[PathBuf], cwd: &Path) -> Result<(Vec<PathBuf>, Vec<Vec<PathBuf>>)> {
    let mut directories = Vec::new();
    let mut file_groups_map: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

    let cwd_canonical = fs::canonicalize(cwd).wrap_err("Failed to canonicalize cwd")?;
    debug!("Canonicalized cwd: {}", cwd_canonical.display());

    for target in targets {
        let target_is_symlink = is_symlink(target);
        let canonical_path = if target_is_symlink {
            canonicalize_link(target)
        } else {
            fs::canonicalize(target)
        }
        .map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                eyre!("{}: No such file or directory", target.display())
            } else {
                eyre!("Failed to canonicalize target {}: {}", target.display(), e)
            }
        })?;
        debug!("Canonicalized target: {}", canonical_path.display());

        let relative_path = match canonical_path.strip_prefix(&cwd_canonical) {
            Ok(rel_path) => rel_path.to_path_buf(),
            Err(e) => {
                debug!("Unable to strip prefix from path '{}': {}", canonical_path.display(), e);
                canonical_path.clone()
            }
        };
        debug!("Relative path: {}", relative_path.display());

        if !target_is_symlink && canonical_path.is_dir() {
            directories.push(canonical_path);
        } else {
            let group_key = relative_path
                .parent()
                .map(|p| cwd_canonical.join(p))
                .unwrap_or_else(|| canonical_path.parent().unwrap().to_path_buf());

            file_groups_map.entry(group_key).or_default().push(canonical_path);
        }
    }

    let mut groups = Vec::new();
    for (_, files) in file_groups_map.into_iter() {
        groups.push(files);
    }

    Ok((directories, groups))
}

/// Remove every target, continuing past failures. Every target is already archived, so a
/// survivor is printed to stderr and counted in the returned error rather than abandoning the rest.
fn remove_targets(targets: &[PathBuf], sudo: bool) -> Result<()> {
    debug!("remove_targets: targets={:?} sudo={}", targets, sudo);
    let failures: Vec<eyre::Report> = targets
        .iter()
        .filter_map(|target| remove_path(target, sudo).err())
        .collect();
    if failures.is_empty() {
        return Ok(());
    }
    for failure in &failures {
        eprintln!("{:#}", failure);
    }
    Err(eyre!(
        "{} of {} targets were archived but not fully removed",
        failures.len(),
        targets.len()
    ))
}

/// Return the next free `<timestamp>-<NNN>` bundle directory under `path`,
/// advancing `index` past any names already taken. This keeps bundle names
/// unique both within one invocation and across two invocations that land in
/// the same second, while preserving chronological string sorting.
fn next_bundle_dir(path: &Path, timestamp: &str, index: &mut usize) -> PathBuf {
    loop {
        let candidate = path.join(format!("{timestamp}-{:03}", *index));
        *index += 1;
        if !candidate.exists() {
            return candidate;
        }
    }
}

fn archive(
    path: &Path,
    timestamp: &str,
    targets: &[PathBuf],
    sudo: bool,
    remove: bool,
    keep: Option<i32>,
) -> Result<()> {
    debug!(
        "fn archive: path={} timestamp={} targets={:?} sudo={} remove={} keep={:?}",
        path.display(),
        timestamp,
        targets,
        sudo,
        remove,
        keep,
    );
    let current_cwd = env::current_dir().wrap_err("Failed to get current directory")?;
    let (directories, groups) = categorize_paths(targets, &current_cwd)?;

    // Each bundle gets the shared timestamp plus a zero-padded index, so names
    // stay unique within one invocation and still sort chronologically as strings.
    let mut bundle_index: usize = 0;

    for group in groups.iter() {
        if !group.is_empty() {
            let base = next_bundle_dir(path, timestamp, &mut bundle_index);
            fs::create_dir_all(&base).wrap_err("Failed to create base directory")?;

            let group_cwd = if let Some(first_file) = group.first() {
                first_file.parent().unwrap_or(&current_cwd).to_path_buf()
            } else {
                current_cwd.clone()
            };

            create_metadata(&base, &group_cwd, group)?;
            archive_group(&base, group, sudo, &group_cwd)?;

            for target in group {
                println!("{}", target.display());
            }
            println!("-> {}/", base.display());
        }
    }

    for directory in directories.iter() {
        let base = next_bundle_dir(path, timestamp, &mut bundle_index);
        fs::create_dir_all(&base).wrap_err("Failed to create base directory")?;

        let dir_cwd = directory.parent().unwrap_or(&current_cwd);
        create_metadata(&base, dir_cwd, std::slice::from_ref(directory))?;
        archive_directory(&base, directory, sudo, dir_cwd)?;

        println!("{}", directory.display());
        println!("-> {}/", base.display());
    }

    if remove {
        remove_targets(targets, sudo)?;
    }

    if let Some(days) = keep {
        cleanup(path, days as usize, sudo)?;
    }

    Ok(())
}

fn get_preferred_pager() -> String {
    std::env::var("RMRF_PAGER").unwrap_or_else(|_| "less -RFX".to_string())
}

fn use_pager<F>(write_content: F) -> Result<()>
where
    F: FnOnce(&mut BufWriter<ChildStdin>) -> Result<()>,
{
    let pager_command = get_preferred_pager();
    let mut parts = pager_command.split_whitespace();
    let pager = parts.next().unwrap_or("less");
    let args = parts.collect::<Vec<&str>>();

    let mut pager_process = Command::new(pager).args(&args).stdin(Stdio::piped()).spawn()?;

    if let Some(stdin) = pager_process.stdin.take() {
        let mut writer = BufWriter::new(stdin);
        if let Err(e) = write_content(&mut writer) {
            if e.downcast_ref::<io::Error>()
                .is_none_or(|io_err| io_err.kind() != io::ErrorKind::BrokenPipe)
            {
                return Err(e);
            }
        }
    }

    let _status = pager_process.wait()?;
    Ok(())
}

fn process_pattern(
    matcher: &SkimMatcherV2,
    dir_name: &str,
    full_path: &Path,
    pattern: &str,
    threshold: i64,
) -> Result<bool> {
    if matcher.fuzzy_match(dir_name, pattern).is_some()
        || matcher
            .fuzzy_match(full_path.to_str().unwrap_or_default(), pattern)
            .is_some()
    {
        return Ok(true);
    }

    let metadata_path = full_path.join("metadata.yml");
    if metadata_path.exists() {
        let metadata_content = std::fs::read_to_string(&metadata_path)?;
        for line in metadata_content.lines() {
            if let Some(score) = matcher.fuzzy_match(line, pattern) {
                if score > threshold {
                    return Ok(true);
                }
            }
        }
    }

    Ok(false)
}

fn process_directory(matcher: &SkimMatcherV2, dir: &DirEntry, patterns: &[String], threshold: i64) -> Result<bool> {
    let dir_name = dir.file_name().to_string_lossy().to_string();
    let full_path = dir
        .path()
        .canonicalize()
        .wrap_err("Failed to canonicalize directory path")?;

    for pattern in patterns {
        if process_pattern(matcher, &dir_name, &full_path, pattern, threshold)? {
            return Ok(true);
        }
    }

    Ok(patterns.is_empty())
}

fn format_directory(dir_path: &Path) -> Result<String> {
    let mut output = format!("{}", dir_path.display().to_string().bright_blue().bold());
    let metadata_path = dir_path.join("metadata.yml");
    if let Ok(metadata_content) = fs::read_to_string(&metadata_path) {
        let formatted_lines: Vec<String> = metadata_content
            .lines()
            .map(|line| {
                if line.starts_with("cwd:") {
                    let parts: Vec<&str> = line.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        format!("  {}: {}", "cwd".white(), parts[1].trim().bright_red())
                    } else {
                        format!("  {}", line)
                    }
                } else if line.starts_with("targets:") {
                    format!("  {}", "targets:".white())
                } else if line.starts_with("contents:") {
                    format!("  {}", "contents:".white())
                } else if line.starts_with("- ") {
                    format!("  {}", line.bright_red())
                } else if line.starts_with("  ") && !line.trim().is_empty() {
                    format!("  {}", line.bright_yellow())
                } else {
                    format!("  {}", line)
                }
            })
            .collect();
        let formatted_content = formatted_lines.join("\n");
        output += &format!("\n{}\n", formatted_content);
    }
    Ok(output)
}

fn list(dir_path: &Path, patterns: &[String], threshold: i64) -> Result<()> {
    let matcher = SkimMatcherV2::default();
    let dir_path = fs::canonicalize(dir_path).wrap_err("Failed to canonicalize directory path")?;

    let mut dirs: Vec<_> = fs::read_dir(&dir_path)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .collect();

    dirs.sort_by_key(|a| std::cmp::Reverse(a.file_name()));

    if atty::is(Stream::Stdout) {
        use_pager(|writer: &mut BufWriter<ChildStdin>| -> Result<()> {
            for dir in dirs.iter() {
                if patterns.is_empty()
                    || process_directory(&matcher, dir, patterns, threshold).map_err(io::Error::other)?
                {
                    let dir_output = format_directory(&dir.path()).map_err(io::Error::other)?;
                    writer.write_all(dir_output.as_bytes())?;
                    writer.write_all(b"\n")?;
                }
            }
            Ok(())
        })?;
    } else {
        for dir in dirs.iter() {
            if patterns.is_empty() || process_directory(&matcher, dir, patterns, threshold)? {
                let dir_output = format_directory(&dir.path())?;
                println!("{}", dir_output);
            }
        }
    }

    Ok(())
}

fn extract_bundle(bundle: &Path, restore_to: &Path, sudo: bool) -> Result<()> {
    let owner = fs::metadata(bundle)?.uid();
    let me = current_uid();

    let status = if owner != me {
        if !sudo {
            eyre::bail!(
                "Cannot extract root-owned archive {} without sudo enabled",
                bundle.display()
            );
        }
        Command::new("sudo")
            .args([
                "tar",
                "xpf",
                bundle.to_str().unwrap(),
                "-C",
                restore_to.to_str().unwrap(),
                "--same-owner",
            ])
            .status()?
    } else {
        Command::new("tar")
            .args(["xzf", bundle.to_str().unwrap(), "-C", restore_to.to_str().unwrap()])
            .status()?
    };

    if !status.success() {
        eyre::bail!("tar extraction failed with status {}", status);
    }
    Ok(())
}

fn recover(root: &Path, ts_dirs: &[PathBuf], sudo: bool) -> Result<()> {
    for ts in ts_dirs {
        let ts_path = if ts.is_absolute() { ts.clone() } else { root.join(ts) };
        let ts_dir = ts_path.canonicalize().wrap_err("canonicalizing timestamp dir")?;

        let meta_path = ts_dir.join("metadata.yml");
        let meta: Metadata = serde_yaml::from_reader(File::open(&meta_path).wrap_err("opening metadata.yml")?)
            .wrap_err("parsing metadata.yml")?;
        let cwd = meta.cwd;
        let originals = &meta.targets;

        let (to_copy, to_extract): (Vec<PathBuf>, Vec<PathBuf>) = fs::read_dir(&ts_dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().and_then(|n| n.to_str()) != Some("metadata.yml"))
            .partition(|p| {
                let fname = p.file_name().unwrap().to_string_lossy();
                originals.iter().any(|t| t == &fname)
            });

        for bundle in to_extract {
            info!("Extracting {} → {}", bundle.display(), cwd.display());
            extract_bundle(&bundle, &cwd, sudo)?;
        }

        for src in to_copy {
            info!("Restoring {} → {}", src.display(), cwd.display());
            copy_files(&cwd, &[src], sudo)?;
        }

        fs::remove_dir_all(&ts_dir).wrap_err_with(|| format!("removing {}", ts_dir.display()))?;
    }
    Ok(())
}

fn main() -> Result<()> {
    setup_logging()?;

    let args = std::env::args().collect::<Vec<String>>();
    info!("main: args={:?}", args);

    let current_level = log::max_level();
    debug!("Current log level: {:?}", current_level);

    // YYYY-MM-DD-HHMMSS: colon-free so the directory name is safe on every
    // filesystem (Windows/macOS included). Same-second collisions are resolved
    // by the `-NNN` bundle suffix (see next_bundle_dir).
    let timestamp = chrono::Local::now().format("%Y-%m-%d-%H%M%S").to_string();
    debug!("Current timestamp: {}", timestamp);

    let matches = Cli::parse_from(args);
    debug!("CLI arguments parsed: {:?}", matches);

    // Load configuration
    let config = Config::load(matches.config.clone())?;
    debug!("Configuration loaded: {:?}", config);

    let action: Action = matches.action.clone().unwrap_or_default();
    info!("Action: {:?}", action);

    let rmrf_cfg_path = rmrf_cfg_home(
        current_uid(),
        env::var("SUDO_USER").ok().as_deref(),
        passwd_home,
        dirs::home_dir(),
    )?
    .join(".config/rmrf/rmrf.cfg");
    debug!("Configuration file path: {:?}", rmrf_cfg_path);

    let rmrf_cfg = load_rmrf_cfg(&rmrf_cfg_path)?;
    debug!("Configuration loaded: {:?}", rmrf_cfg);

    let rmrf_path = rmrf_cfg
        .get("DEFAULT", "rmrf_path")
        .unwrap_or("/var/tmp/rmrf".to_owned());
    let rmrf_path = Path::new(&rmrf_path);

    let bkup_path = rmrf_cfg
        .get("DEFAULT", "bkup_path")
        .unwrap_or("/var/tmp/bkup".to_owned());
    let bkup_path = Path::new(&bkup_path);

    let sudo: bool = rmrf_cfg.get("DEFAULT", "sudo").unwrap_or("yes".to_owned()) == "yes";
    let days: i32 = rmrf_cfg
        .get("DEFAULT", "keep")
        .map(|s| s.parse().unwrap_or(config.cleanup_days as i32))
        .unwrap_or(config.cleanup_days as i32);
    let threshold: i64 = rmrf_cfg
        .get("DEFAULT", "threshold")
        .unwrap_or("70".to_owned())
        .parse()?;

    info!(
        "Configuration - rmrf_path: {:?}, bkup_path: {:?}, sudo: {}, keep for days: {}, threshold: {}",
        rmrf_path, bkup_path, sudo, days, threshold,
    );

    fs::create_dir_all(rmrf_path)?;
    fs::create_dir_all(bkup_path)?;
    info!("Directories created or verified: {:?}, {:?}", rmrf_path, bkup_path);

    match &matches.action {
        Some(action) => match action {
            Action::Bkup(args) => {
                archive(bkup_path, &timestamp, &as_paths(&args.targets), sudo, false, None)?;
            }
            Action::Rmrf(args) => {
                archive(rmrf_path, &timestamp, &as_paths(&args.targets), sudo, true, Some(days))?;
            }
            Action::Rcvr(args) => {
                recover(rmrf_path, &as_paths(&args.targets), sudo)?;
            }
            Action::LsBkup(args) => {
                list(bkup_path, &args.targets, threshold)?;
            }
            Action::LsRmrf(args) => {
                list(rmrf_path, &args.targets, threshold)?;
            }
            Action::BkupRmrf(args) => {
                archive(bkup_path, &timestamp, &as_paths(&args.targets), sudo, true, None)?;
            }
        },
        None => {
            archive(
                rmrf_path,
                &timestamp,
                &as_paths(&matches.targets),
                sudo,
                true,
                Some(days),
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_categorize_paths_same_directory() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let dir1 = temp_path.join("logs");
        fs::create_dir_all(&dir1).unwrap();

        let file1 = dir1.join("app.log");
        let file2 = dir1.join("error.log");
        fs::write(&file1, "app").unwrap();
        fs::write(&file2, "error").unwrap();

        let targets = vec![file1, file2];
        let (directories, groups) = categorize_paths(&targets, temp_path).unwrap();

        assert_eq!(directories.len(), 0, "Should have no directories");
        assert_eq!(groups.len(), 1, "Should have one group");
        assert_eq!(groups[0].len(), 2, "Group should contain both files");
    }

    #[test]
    fn test_categorize_paths_mixed_files_and_directories() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let dir1 = temp_path.join("logs");
        let dir2 = temp_path.join("project");
        fs::create_dir_all(&dir1).unwrap();
        fs::create_dir_all(&dir2).unwrap();

        let file1 = dir1.join("app.log");
        fs::write(&file1, "app").unwrap();

        let targets = vec![file1, dir2.clone()];
        let (directories, groups) = categorize_paths(&targets, temp_path).unwrap();

        assert_eq!(directories.len(), 1, "Should have one directory");
        assert_eq!(groups.len(), 1, "Should have one file group");
        assert_eq!(directories[0], dir2, "Directory should match");
        assert_eq!(groups[0].len(), 1, "File group should contain one file");
    }

    #[test]
    fn test_create_metadata() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let base = temp_path.join("archive");
        let cwd = temp_path.join("source");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&cwd).unwrap();

        let file1 = cwd.join("test.txt");
        fs::write(&file1, "test content").unwrap();

        let targets = vec![file1];
        create_metadata(&base, &cwd, &targets).unwrap();

        let metadata_file = base.join("metadata.yml");
        assert!(metadata_file.exists(), "Metadata file should be created");

        let metadata_content = fs::read_to_string(&metadata_file).unwrap();
        assert!(metadata_content.contains(&format!("cwd: {}", cwd.display())));
        assert!(metadata_content.contains("- test.txt"));
        assert!(metadata_content.contains("contents: |"));
    }

    #[test]
    fn test_create_tar_command_relative_paths() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let tarball = temp_path.join("test.tar.gz");
        let cwd = temp_path.join("source");
        fs::create_dir_all(&cwd).unwrap();

        let targets = vec!["file1.txt".to_string(), "file2.txt".to_string()];

        let command = create_tar_command(false, &tarball, &cwd, targets).unwrap();

        assert_eq!(command.get_program(), "tar");

        let args: Vec<_> = command.get_args().collect();
        let args_str = format!("{:?}", args);
        assert!(args_str.contains("file1.txt"));
        assert!(args_str.contains("file2.txt"));
    }

    #[test]
    fn test_create_tar_command_with_sudo() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let tarball = temp_path.join("test.tar.gz");
        let cwd = temp_path.join("source");
        fs::create_dir_all(&cwd).unwrap();

        let targets = vec!["file1.txt".to_string()];

        let command = create_tar_command(true, &tarball, &cwd, targets).unwrap();

        assert_eq!(command.get_program(), "sudo");

        let args: Vec<_> = command.get_args().collect();
        let args_str = format!("{:?}", args);
        assert!(args_str.contains("tar"));
    }

    #[test]
    fn test_is_archive() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let tar_file = temp_path.join("test.tar.gz");
        fs::write(&tar_file, "fake tar content").unwrap();

        let regular_file = temp_path.join("test.txt");
        fs::write(&regular_file, "regular content").unwrap();

        assert!(is_archive(&tar_file), "Should recognize .tar.gz as archive");
        assert!(!is_archive(&regular_file), "Should not recognize .txt as archive");
    }

    #[test]
    #[allow(clippy::absurd_extreme_comparisons, unused_comparisons)]
    fn test_current_uid() {
        let uid = current_uid();
        assert!(uid >= 0, "UID should be valid");
    }

    #[test]
    fn test_file_uid() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let test_file = temp_path.join("test.txt");
        fs::write(&test_file, "test").unwrap();

        let uid = file_uid(&test_file).unwrap();
        assert_eq!(uid, current_uid(), "File should be owned by current user");
    }

    #[test]
    fn test_as_paths() {
        let strings = vec!["/tmp/file1.txt".to_string(), "relative/file2.txt".to_string()];

        let paths = as_paths(&strings);

        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], PathBuf::from("/tmp/file1.txt"));
        assert_eq!(paths[1], PathBuf::from("relative/file2.txt"));
    }

    #[test]
    fn test_remove_targets() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let test_file = temp_path.join("test.txt");
        let test_dir = temp_path.join("test_dir");
        let test_dir_file = test_dir.join("inner.txt");

        fs::write(&test_file, "test").unwrap();
        fs::create_dir_all(&test_dir).unwrap();
        fs::write(&test_dir_file, "inner").unwrap();

        let targets = vec![test_file.clone(), test_dir.clone()];

        remove_targets(&targets, false).unwrap();

        assert!(!test_file.exists(), "File should be removed");
        assert!(!test_dir.exists(), "Directory should be removed");
    }

    #[test]
    fn test_cleanup_basic_functionality() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let dir1 = temp_path.join("1234567890");
        let dir2 = temp_path.join("9876543210");

        fs::create_dir_all(&dir1).unwrap();
        fs::create_dir_all(&dir2).unwrap();

        fs::write(dir1.join("metadata.yml"), "cwd: /tmp\ntargets: []\ncontents: |").unwrap();
        fs::write(dir2.join("metadata.yml"), "cwd: /tmp\ntargets: []\ncontents: |").unwrap();

        cleanup(temp_path, 30, false).unwrap();

        assert!(dir1.exists(), "Recently created directory should still exist");
        assert!(dir2.exists(), "Recently created directory should still exist");

        cleanup(temp_path, 365, false).unwrap();

        assert!(dir1.exists(), "Directory should exist with long threshold");
        assert!(dir2.exists(), "Directory should exist with long threshold");
    }

    #[test]
    fn test_archive_single_file() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let source_dir = temp_path.join("source");
        let archive_dir = temp_path.join("archive");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&archive_dir).unwrap();

        let test_file = source_dir.join("test.txt");
        fs::write(&test_file, "test content").unwrap();

        let timestamp = "2026-06-14-153045";
        let targets = vec![test_file.clone()];

        archive(&archive_dir, timestamp, &targets, false, false, None).unwrap();

        assert!(test_file.exists(), "Original file should still exist");

        let expected_archive = archive_dir.join(format!("{timestamp}-000"));
        assert!(expected_archive.exists(), "Archive directory should be created");

        let metadata_file = expected_archive.join("metadata.yml");
        assert!(metadata_file.exists(), "Metadata file should be created");

        let metadata_content = fs::read_to_string(&metadata_file).unwrap();
        assert!(metadata_content.contains(&format!("cwd: {}", source_dir.display())));
        assert!(metadata_content.contains("- test.txt"));
    }

    #[test]
    fn test_archive_and_remove() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let source_dir = temp_path.join("source");
        let archive_dir = temp_path.join("archive");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&archive_dir).unwrap();

        let test_file = source_dir.join("test.txt");
        fs::write(&test_file, "test content").unwrap();

        let timestamp = "2026-06-14-153045";
        let targets = vec![test_file.clone()];

        archive(&archive_dir, timestamp, &targets, false, true, None).unwrap();

        assert!(!test_file.exists(), "Original file should be removed");

        let expected_archive = archive_dir.join(format!("{timestamp}-000"));
        assert!(expected_archive.exists(), "Archive directory should be created");
    }

    /// The `tar -tv` line for `name` in the bundle's `source.tar.gz`, e.g.
    /// `lrwxrwxrwx user/group 0 date hook.sh -> /path/missing.sh`.
    fn tarball_entry(archive_dir: &Path, timestamp: &str, name: &str) -> String {
        let tarball = archive_dir.join(format!("{timestamp}-000")).join("source.tar.gz");
        let output = Command::new("tar").arg("-tvzf").arg(&tarball).output().unwrap();
        assert!(output.status.success(), "tar -tvzf {} failed", tarball.display());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find(|l| l.contains(&format!(" {name} -> ")))
            .unwrap_or_else(|| panic!("no symlink entry for {name} in {}", tarball.display()))
            .to_string()
    }

    #[test]
    fn test_categorize_paths_dangling_symlink() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = fs::canonicalize(temp_dir.path()).unwrap();

        let link = temp_path.join("dangling");
        symlink(temp_path.join("gone"), &link).unwrap();

        let (directories, groups) = categorize_paths(std::slice::from_ref(&link), &temp_path).unwrap();

        assert!(directories.is_empty());
        assert_eq!(groups, vec![vec![link]]);
    }

    #[test]
    fn test_archive_and_remove_dangling_symlink() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let source_dir = temp_path.join("source");
        let archive_dir = temp_path.join("archive");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&archive_dir).unwrap();

        let missing = source_dir.join("missing.sh");
        let link = source_dir.join("hook.sh");
        symlink(&missing, &link).unwrap();

        let timestamp = "2026-06-14-153045";
        archive(&archive_dir, timestamp, std::slice::from_ref(&link), false, true, None).unwrap();

        assert!(fs::symlink_metadata(&link).is_err(), "Link should be removed");
        assert!(!missing.exists(), "Missing target should stay missing");
        let entry = tarball_entry(&archive_dir, timestamp, "hook.sh");
        assert!(entry.starts_with('l'), "Archive should hold the link itself: {entry}");
        assert!(entry.ends_with(&format!("-> {}", missing.display())));
    }

    #[test]
    fn test_archive_and_remove_symlink_to_directory() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let source_dir = temp_path.join("source");
        let archive_dir = temp_path.join("archive");
        let checkout = temp_path.join("checkout");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&archive_dir).unwrap();
        fs::create_dir_all(&checkout).unwrap();
        fs::write(checkout.join("keep.txt"), "keep").unwrap();

        let link = source_dir.join("repo");
        symlink(&checkout, &link).unwrap();

        let timestamp = "2026-06-14-153045";
        archive(&archive_dir, timestamp, std::slice::from_ref(&link), false, true, None).unwrap();

        assert!(fs::symlink_metadata(&link).is_err(), "Link should be removed");
        assert!(
            checkout.join("keep.txt").exists(),
            "Directory behind the link should survive"
        );
        let entry = tarball_entry(&archive_dir, timestamp, "repo");
        assert!(entry.ends_with(&format!("-> {}", checkout.display())));
        assert!(!archive_dir
            .join(format!("{timestamp}-000"))
            .join("repo.tar.gz")
            .exists());
    }

    #[test]
    fn test_archive_and_remove_symlink_to_file() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let source_dir = temp_path.join("source");
        let archive_dir = temp_path.join("archive");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&archive_dir).unwrap();

        let real = temp_path.join("real.txt");
        fs::write(&real, "content").unwrap();
        let link = source_dir.join("alias.txt");
        symlink(&real, &link).unwrap();

        let timestamp = "2026-06-14-153045";
        archive(&archive_dir, timestamp, std::slice::from_ref(&link), false, true, None).unwrap();

        assert!(fs::symlink_metadata(&link).is_err(), "Link should be removed");
        assert_eq!(
            fs::read_to_string(&real).unwrap(),
            "content",
            "File behind the link should survive"
        );
        let entry = tarball_entry(&archive_dir, timestamp, "alias.txt");
        assert!(entry.ends_with(&format!("-> {}", real.display())));
    }

    #[test]
    fn test_config_load_default() {
        let config = Config::load(None).unwrap();
        assert_eq!(config.cleanup_days, 30);
        assert!(!config.auto_cleanup);
        assert!(config.archive_location.contains("rkvr/archive"));
    }

    #[test]
    fn test_config_load_from_file() {
        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join("test_config.yml");

        let config_content = r#"
cleanup_days: 45
auto_cleanup: true
archive_location: "/tmp/test_archive"
"#;
        fs::write(&config_file, config_content).unwrap();

        let config = Config::load(Some(config_file)).unwrap();
        assert_eq!(config.cleanup_days, 45);
        assert!(config.auto_cleanup);
        assert_eq!(config.archive_location, "/tmp/test_archive");
    }

    #[test]
    fn test_config_load_partial_file() {
        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join("partial_config.yml");

        let config_content = r#"
cleanup_days: 15
"#;
        fs::write(&config_file, config_content).unwrap();

        let config = Config::load(Some(config_file)).unwrap();
        assert_eq!(config.cleanup_days, 15);
        assert!(!config.auto_cleanup);
        assert!(config.archive_location.contains("rkvr/archive"));
    }

    #[test]
    fn test_config_load_invalid_file() {
        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join("invalid_config.yml");

        let config_content = r#"
invalid_yaml: [unclosed
"#;
        fs::write(&config_file, config_content).unwrap();

        let result = Config::load(Some(config_file));
        assert!(result.is_err(), "Should fail to load invalid YAML");
    }

    #[test]
    fn test_config_load_nonexistent_file() {
        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join("nonexistent.yml");

        let config = Config::load(Some(config_file)).unwrap();
        assert_eq!(config.cleanup_days, 30);
        assert!(!config.auto_cleanup);
    }

    #[test]
    fn test_config_integration_with_main() {
        let temp_dir = TempDir::new().unwrap();
        let config_file = temp_dir.path().join("integration_config.yml");

        let config_content = r#"
cleanup_days: 7
auto_cleanup: true
archive_location: "/tmp/integration_test"
"#;
        fs::write(&config_file, config_content).unwrap();

        let config = Config::load(Some(config_file)).unwrap();
        assert_eq!(config.cleanup_days, 7);
        assert!(config.auto_cleanup);
        assert_eq!(config.archive_location, "/tmp/integration_test");
    }

    #[test]
    fn test_cleanup_with_sudo_disabled() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create directories with regular permissions
        let dir1 = temp_path.join("1234567890");
        let dir2 = temp_path.join("9876543210");

        fs::create_dir_all(&dir1).unwrap();
        fs::create_dir_all(&dir2).unwrap();

        fs::write(dir1.join("metadata.yml"), "cwd: /tmp\ntargets: []\ncontents: |").unwrap();
        fs::write(dir2.join("metadata.yml"), "cwd: /tmp\ntargets: []\ncontents: |").unwrap();

        // Test cleanup with sudo=false (should work for user-owned files)
        cleanup(temp_path, 30, false).unwrap();

        assert!(dir1.exists(), "Recently created directory should still exist");
        assert!(dir2.exists(), "Recently created directory should still exist");

        // Test cleanup with longer threshold
        cleanup(temp_path, 365, false).unwrap();

        assert!(dir1.exists(), "Directory should exist with long threshold");
        assert!(dir2.exists(), "Directory should exist with long threshold");
    }

    #[test]
    fn test_cleanup_with_sudo_enabled() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create directories with regular permissions
        let dir1 = temp_path.join("1234567890");
        let dir2 = temp_path.join("9876543210");

        fs::create_dir_all(&dir1).unwrap();
        fs::create_dir_all(&dir2).unwrap();

        fs::write(dir1.join("metadata.yml"), "cwd: /tmp\ntargets: []\ncontents: |").unwrap();
        fs::write(dir2.join("metadata.yml"), "cwd: /tmp\ntargets: []\ncontents: |").unwrap();

        // Test cleanup with sudo=true (should still work for user-owned files)
        cleanup(temp_path, 30, true).unwrap();

        assert!(dir1.exists(), "Recently created directory should still exist");
        assert!(dir2.exists(), "Recently created directory should still exist");

        // Test cleanup with longer threshold
        cleanup(temp_path, 365, true).unwrap();

        assert!(dir1.exists(), "Directory should exist with long threshold");
        assert!(dir2.exists(), "Directory should exist with long threshold");
    }

    #[test]
    fn test_cleanup_old_directories() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create directories
        let old_dir = temp_path.join("1234567890");
        let recent_dir = temp_path.join("9876543210");

        fs::create_dir_all(&old_dir).unwrap();
        fs::create_dir_all(&recent_dir).unwrap();

        fs::write(old_dir.join("metadata.yml"), "cwd: /tmp\ntargets: []\ncontents: |").unwrap();
        fs::write(recent_dir.join("metadata.yml"), "cwd: /tmp\ntargets: []\ncontents: |").unwrap();

        // We can't easily change file timestamps in tests without external tools,
        // so we'll test the logic by using a very short threshold (0 days)
        // This should delete all directories
        cleanup(temp_path, 0, false).unwrap();

        // Both directories should be deleted with 0 day threshold
        assert!(!old_dir.exists(), "Old directory should be removed");
        assert!(!recent_dir.exists(), "Directory should be removed with 0 day threshold");
    }

    fn assert_not_root() {
        assert_ne!(
            current_uid(),
            0,
            "running as root: root bypasses directory permissions, so this EACCES test would pass vacuously"
        );
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Back-date `path`'s own mtime (`lstat`, never a symlink's target) by `days`.
    fn set_lstat_mtime_days_ago(path: &Path, days: u64) {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        ts.tv_sec = (now - days * 24 * 60 * 60) as libc::time_t;
        let times = [ts, ts];
        // SAFETY: `c_path` and `times` are live locals for the duration of the call.
        let rc = unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                c_path.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        assert_eq!(rc, 0, "utimensat({}): {}", path.display(), io::Error::last_os_error());
    }

    #[test]
    fn escalation_allowed_sudo_non_root() {
        assert!(escalation_allowed(true, 1000));
    }

    #[test]
    fn escalation_allowed_sudo_root() {
        assert!(!escalation_allowed(true, 0));
    }

    #[test]
    fn escalation_allowed_no_sudo_non_root() {
        assert!(!escalation_allowed(false, 1000));
    }

    #[test]
    fn escalation_allowed_no_sudo_root() {
        assert!(!escalation_allowed(false, 0));
    }

    #[test]
    fn remove_path_unwritable_parent_without_sudo_errors() {
        assert_not_root();
        let temp_dir = TempDir::new().unwrap();
        let locked = temp_dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        let target = locked.join("a.txt");
        fs::write(&target, "a").unwrap();
        set_mode(&locked, 0o555);

        let result = remove_path(&target, false);
        set_mode(&locked, 0o755);

        let message = format!("{:#}", result.expect_err("unlink in a 0555 dir must fail without sudo"));
        assert!(message.contains("permission denied"), "message: {message}");
        assert!(message.contains(&target.display().to_string()), "message: {message}");
        assert!(
            message.contains(&format!("(parent: {})", locked.display())),
            "message: {message}"
        );
        assert!(target.exists(), "target must survive a refused removal");
    }

    #[test]
    fn remove_path_removes_dangling_symlink() {
        let temp_dir = TempDir::new().unwrap();
        let link = temp_dir.path().join("dangling");
        symlink(temp_dir.path().join("missing"), &link).unwrap();

        remove_path(&link, false).unwrap();

        assert!(fs::symlink_metadata(&link).is_err(), "dangling link should be gone");
    }

    #[test]
    fn remove_path_missing_target_is_ok() {
        let temp_dir = TempDir::new().unwrap();
        remove_path(&temp_dir.path().join("never-existed"), false).unwrap();
    }

    #[test]
    fn remove_path_removes_symlink_not_its_target_dir() {
        let temp_dir = TempDir::new().unwrap();
        let real = temp_dir.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("keep.txt"), "keep").unwrap();
        let link = temp_dir.path().join("link");
        symlink(&real, &link).unwrap();

        remove_path(&link, false).unwrap();

        assert!(fs::symlink_metadata(&link).is_err(), "link should be gone");
        assert!(real.join("keep.txt").exists(), "link target must not be touched");
    }

    #[test]
    fn remove_path_removes_directory_tree() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path().join("tree");
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("nested/file.txt"), "x").unwrap();

        remove_path(&dir, false).unwrap();

        assert!(!dir.exists(), "directory tree should be gone");
    }

    #[test]
    fn remove_targets_continues_past_failure() {
        assert_not_root();
        let temp_dir = TempDir::new().unwrap();
        let locked = temp_dir.path().join("src");
        let open = temp_dir.path().join("src2");
        fs::create_dir(&locked).unwrap();
        fs::create_dir(&open).unwrap();
        let stuck = locked.join("a.txt");
        let free = open.join("c.txt");
        fs::write(&stuck, "a").unwrap();
        fs::write(&free, "c").unwrap();
        set_mode(&locked, 0o555);

        // The failing target is first, so removing `free` proves continuation.
        let result = remove_targets(&[stuck.clone(), free.clone()], false);
        set_mode(&locked, 0o755);

        let message = format!("{:#}", result.expect_err("a refused target must fail the call"));
        assert!(message.contains("1 of 2 targets"), "message: {message}");
        assert!(stuck.exists(), "refused target survives");
        assert!(!free.exists(), "later target is still removed");
    }

    #[test]
    fn remove_targets_dir_and_its_child_is_ok() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path().join("dir");
        let child = dir.join("child.txt");
        fs::create_dir(&dir).unwrap();
        fs::write(&child, "c").unwrap();

        remove_targets(&[dir.clone(), child], false).unwrap();

        assert!(!dir.exists());
    }

    #[test]
    fn cleanup_removes_stale_dangling_symlink() {
        let temp_dir = TempDir::new().unwrap();
        let bin = temp_dir.path();
        let link = bin.join("2026-01-01-000000-000");
        symlink(bin.join("missing-target"), &link).unwrap();
        set_lstat_mtime_days_ago(&link, 10);
        let fresh = bin.join("2026-11-16-000000-000");
        fs::create_dir(&fresh).unwrap();

        cleanup(bin, 1, false).unwrap();

        assert!(
            fs::symlink_metadata(&link).is_err(),
            "stale dangling link should be removed"
        );
        assert!(fresh.exists(), "fresh bundle should be kept");
    }

    #[test]
    fn cleanup_continues_past_failure() {
        assert_not_root();
        let temp_dir = TempDir::new().unwrap();
        let bin = temp_dir.path();

        // Two refused bundles around one removable one: whatever order read_dir yields,
        // stopping after the first failure either skips `free` or undercounts to 1.
        let make_stuck = |name: &str| {
            let stuck = bin.join(name);
            let locked = stuck.join("locked");
            fs::create_dir_all(&locked).unwrap();
            fs::write(locked.join("file.txt"), "x").unwrap();
            set_mode(&locked, 0o555);
            set_lstat_mtime_days_ago(&stuck, 10);
            locked
        };
        let locked_a = make_stuck("2026-01-01-000000-000");
        let locked_b = make_stuck("2026-01-03-000000-000");

        let free = bin.join("2026-01-02-000000-000");
        fs::create_dir(&free).unwrap();
        fs::write(free.join("metadata.yml"), "cwd: /tmp").unwrap();
        set_lstat_mtime_days_ago(&free, 10);

        let result = cleanup(bin, 1, false);
        set_mode(&locked_a, 0o755);
        set_mode(&locked_b, 0o755);

        let message = format!("{:#}", result.expect_err("a refused stale entry must fail cleanup"));
        assert!(message.contains("2 stale entries"), "message: {message}");
        assert!(locked_a.join("file.txt").exists(), "refused bundle content survives");
        assert!(locked_b.join("file.txt").exists(), "refused bundle content survives");
        assert!(!free.exists(), "removable stale bundle is still removed");
    }

    #[test]
    fn rmrf_cfg_missing_returns_defaults() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join(".config/rmrf/rmrf.cfg");

        let ini = load_rmrf_cfg(&path).expect("a missing rmrf.cfg must load as defaults");
        assert!(
            ini.sections().is_empty(),
            "expected an empty Ini, got {:?}",
            ini.sections()
        );
        assert_eq!(ini.get("DEFAULT", "rmrf_path"), None);
    }

    #[test]
    fn rmrf_cfg_present_file_is_read() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("rmrf.cfg");
        fs::write(&path, "rmrf_path=/x/rmrf\nsudo=no\nkeep=21\n").unwrap();

        let ini = load_rmrf_cfg(&path).unwrap();
        assert_eq!(ini.get("DEFAULT", "rmrf_path").as_deref(), Some("/x/rmrf"));
        assert_eq!(ini.get("DEFAULT", "sudo").as_deref(), Some("no"));
        assert_eq!(ini.get("DEFAULT", "keep").as_deref(), Some("21"));
    }

    #[test]
    fn rmrf_cfg_unreadable_is_error() {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            current_uid(),
            0,
            "running as root: root reads a 0000 file, so this test would pass vacuously"
        );
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("rmrf.cfg");
        fs::write(&path, "sudo=no\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        let err = load_rmrf_cfg(&path).expect_err("an unreadable rmrf.cfg must be an error");
        assert!(
            format!("{:#}", err).contains(&path.display().to_string()),
            "error should name the path: {:#}",
            err
        );
    }

    #[test]
    fn rmrf_cfg_dangling_symlink_is_error() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("rmrf.cfg");
        symlink(temp_dir.path().join("missing-target.cfg"), &path).unwrap();

        let err = load_rmrf_cfg(&path).expect_err("a dangling rmrf.cfg symlink must be an error");
        assert!(
            format!("{:#}", err).contains(&path.display().to_string()),
            "error should name the path: {:#}",
            err
        );
    }

    fn lookup_never(_: &str) -> Result<Option<PathBuf>> {
        panic!("lookup must not be called");
    }

    #[test]
    fn rmrf_cfg_home_uses_sudo_user_when_root() {
        let home = rmrf_cfg_home(
            0,
            Some("alice"),
            |user| {
                assert_eq!(user, "alice");
                Ok(Some(PathBuf::from("/home/alice")))
            },
            Some(PathBuf::from("/root")),
        )
        .unwrap();
        assert_eq!(home, PathBuf::from("/home/alice"));
    }

    #[test]
    fn rmrf_cfg_home_ignores_sudo_user_when_not_root() {
        let home = rmrf_cfg_home(1000, Some("alice"), lookup_never, Some(PathBuf::from("/home/me"))).unwrap();
        assert_eq!(home, PathBuf::from("/home/me"));
    }

    #[test]
    fn rmrf_cfg_home_ignores_sudo_user_root() {
        let home = rmrf_cfg_home(0, Some("root"), lookup_never, Some(PathBuf::from("/root"))).unwrap();
        assert_eq!(home, PathBuf::from("/root"));
    }

    #[test]
    fn rmrf_cfg_home_without_sudo_user_and_no_fallback_is_error() {
        let err = rmrf_cfg_home(0, None, lookup_never, None).unwrap_err();
        assert!(err.to_string().contains("home dir not found"), "{}", err);
    }

    #[test]
    fn rmrf_cfg_home_unknown_sudo_user_is_error() {
        let err = rmrf_cfg_home(0, Some("ghost"), |_| Ok(None), Some(PathBuf::from("/root"))).unwrap_err();
        assert_eq!(err.to_string(), "SUDO_USER=ghost has no passwd entry");
    }

    #[test]
    fn rmrf_cfg_home_lookup_error_propagates() {
        let err = rmrf_cfg_home(
            0,
            Some("alice"),
            |_| Err(eyre!("nss exploded")),
            Some(PathBuf::from("/root")),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "nss exploded");
    }

    #[test]
    fn rmrf_cfg_passwd_home_root() {
        assert_eq!(passwd_home("root").unwrap(), Some(PathBuf::from("/root")));
    }

    #[test]
    fn rmrf_cfg_passwd_home_unknown_user_is_none() {
        assert_eq!(passwd_home("rkvr-no-such-user-zz9").unwrap(), None);
    }

    #[test]
    fn rmrf_cfg_passwd_home_nul_in_name_is_error() {
        assert!(passwd_home("ro\0ot").is_err());
    }
}
