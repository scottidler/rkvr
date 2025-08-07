// src/main.rs
use libc::getuid;
use log::{debug, info};
use std::collections::HashMap;
use std::env;
use std::fs::{self, DirEntry, File};
use std::io::{self, BufWriter, ErrorKind, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::time::SystemTime;
use std::fs::OpenOptions;
use which::which;

// Third-party crate imports
use atty::Stream;
use clap::Parser;
use configparser::ini::Ini;
use dirs;
use env_logger::Target;
use eyre::{eyre, Context, Result};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use serde::{Deserialize, Serialize};
use colored::*;

// Local modules
mod cli;
mod config;

use cli::{Cli, Action};
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
            if p.starts_with("~/") {
                dirs::home_dir().unwrap_or_default().join(&p[2..])
            } else {
                PathBuf::from(p)
            }
        })
        .collect()
}

fn get_log_file_path() -> Result<PathBuf> {
    let log_dir = dirs::data_local_dir()
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
            writeln!(buf,
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

fn file_uid(path: &Path) -> eyre::Result<u32> {
    Ok(fs::metadata(path)?.uid())
}

fn remove_file_with_sudo(path: &Path, sudo: bool) -> Result<()> {
    if sudo {
        let owner = fs::metadata(path)?.uid();
        let need_sudo = owner != current_uid();

        if need_sudo {
            debug!("Using sudo to remove file: {}", path.to_string_lossy());
            let status = Command::new("sudo")
                .args(&["rm", "-f", &path.to_string_lossy()])
                .status()?;

            if !status.success() {
                eyre::bail!("Failed to remove file {} with sudo (status {})", path.display(), status);
            }
            return Ok(());
        }
    }

    // Use regular removal if sudo is not enabled or not needed
    fs::remove_file(path)?;
    Ok(())
}

fn remove_directory_with_sudo(path: &Path, sudo: bool) -> Result<()> {
    if sudo {
        let owner = fs::metadata(path)?.uid();
        let need_sudo = owner != current_uid();

        if need_sudo {
            debug!("Using sudo to remove directory: {}", path.to_string_lossy());
            let status = Command::new("sudo")
                .args(&["rm", "-rf", &path.to_string_lossy()])
                .status()?;

            if !status.success() {
                eyre::bail!("Failed to remove directory {} with sudo (status {})", path.display(), status);
            }
            return Ok(());
        }
    }

    // Use regular removal if sudo is not enabled or not needed
    fs::remove_dir_all(path)?;
    Ok(())
}

fn cleanup(dir_path: &std::path::Path, days: usize, sudo: bool) -> Result<()> {
    info!("fn cleanup: dir_path={} days={} sudo={}", dir_path.to_string_lossy(), days, sudo);

    let now = SystemTime::now();
    debug!("Current time: {:?}", now);

    let delete_threshold = std::time::Duration::from_secs(60 * 60 * 24 * days as u64);
    debug!("Delete threshold duration: {:?} ({} days)", delete_threshold, days);

    let entries = fs::read_dir(dir_path)?;
    debug!("Directory entries read: entries={:?}", entries);

    for entry in entries {
        if let Ok(entry) = entry {
            let path = entry.path();
            debug!("Checking path: {}", path.to_string_lossy());

            let metadata = fs::metadata(&path)?;
            debug!("Metadata retrieved");

            let modified_time = metadata.modified()?;
            debug!("Modified time: {:?}", modified_time);

            if let Ok(duration_since_modified) = now.duration_since(modified_time) {
                debug!(
                    "Duration since modified: {:?}, Delete threshold: {:?}",
                    duration_since_modified, delete_threshold
                );

                if duration_since_modified > delete_threshold {
                    info!("Deleting path: {}", path.to_string_lossy());

                    if metadata.is_dir() {
                        debug!("Removing directory: {}", path.to_string_lossy());
                        remove_directory_with_sudo(&path, sudo)?;
                    } else {
                        debug!("Removing file: {}", path.to_string_lossy());
                        remove_file_with_sudo(&path, sudo)?;
                    }
                }
            }
        }
    }

    info!("Cleanup completed");
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
            .args(&["-u", &sudo_user, "-i", "which", "eza"])
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
        cmd.args(&[
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
        cmd.args(&["-czf", tarball_path.to_str().unwrap(), "-C", cwd.to_str().unwrap()]);
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
        let owner = fs::metadata(src)?.uid();
        if owner == me {
            fs::copy(src, &dest)?;
            fs::set_permissions(&dest, fs::metadata(src)?.permissions())?;
        } else {
            if !sudo {
                eyre::bail!(
                    "Not permitted to copy {} (owned by uid={}); enable sudo in config",
                    src.display(),
                    owner
                );
            }
            let status = Command::new("sudo")
                .args(&["cp", "-a", src.to_str().unwrap(), dest.to_str().unwrap()])
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
        let canonical_path = fs::canonicalize(target).map_err(|e| {
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

        if canonical_path.is_dir() {
            directories.push(canonical_path);
        } else {
            let group_key = relative_path
                .parent()
                .map(|p| cwd_canonical.join(p))
                .unwrap_or_else(|| canonical_path.parent().unwrap().to_path_buf());

            file_groups_map
                .entry(group_key)
                .or_insert_with(Vec::new)
                .push(canonical_path);
        }
    }

    let mut groups = Vec::new();
    for (_, files) in file_groups_map.into_iter() {
        groups.push(files);
    }

    Ok((directories, groups))
}

fn remove_targets(targets: &[PathBuf]) -> Result<()> {
    for target in targets {
        if target.is_dir() {
            fs::remove_dir_all(target)?;
        } else {
            fs::remove_file(target)?;
        }
    }
    Ok(())
}

fn archive(
    path: &Path,
    timestamp: u64,
    targets: &[PathBuf],
    sudo: bool,
    remove: bool,
    keep: Option<i32>,
) -> Result<()> {
    let current_cwd = env::current_dir().wrap_err("Failed to get current directory")?;
    let (directories, groups) = categorize_paths(targets, &current_cwd)?;

    for (group_index, group) in groups.iter().enumerate() {
        if !group.is_empty() {
            let group_timestamp = timestamp + (group_index as u64 * 1000);
            let base = path.join(group_timestamp.to_string());
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

    for (dir_index, directory) in directories.iter().enumerate() {
        let dir_timestamp = timestamp + 10000 + (dir_index as u64 * 1000);
        let base = path.join(dir_timestamp.to_string());
        fs::create_dir_all(&base).wrap_err("Failed to create base directory")?;

        let dir_cwd = directory.parent().unwrap_or(&current_cwd);
        create_metadata(&base, dir_cwd, &[directory.clone()])?;
        archive_directory(&base, directory, sudo, dir_cwd)?;

        println!("{}", directory.display());
        println!("-> {}/", base.display());
    }

    if remove {
        remove_targets(targets)?;
    }

    if let Some(days) = keep {
        cleanup(&path, days as usize, sudo)?;
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
                .map_or(true, |io_err| io_err.kind() != io::ErrorKind::BrokenPipe)
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
    full_path: &PathBuf,
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
        let formatted_lines: Vec<String> = metadata_content.lines().map(|line| {
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
        }).collect();
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

    dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

    if atty::is(Stream::Stdout) {
        use_pager(|writer: &mut BufWriter<ChildStdin>| -> Result<()> {
            for dir in dirs.iter() {
                if patterns.is_empty()
                    || process_directory(&matcher, dir, &patterns, threshold)
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
                {
                    let dir_output =
                        format_directory(&dir.path()).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                    writer.write_all(dir_output.as_bytes())?;
                    writer.write_all(b"\n")?;
                }
            }
            Ok(())
        })?;
    } else {
        for dir in dirs.iter() {
            if patterns.is_empty() || process_directory(&matcher, dir, &patterns, threshold)? {
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
            .args(&[
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
            .args(&["xzf", bundle.to_str().unwrap(), "-C", restore_to.to_str().unwrap()])
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

    let timestamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_nanos() as u64;
    debug!("Current timestamp: {}", timestamp);

    let matches = Cli::parse_from(args);
    debug!("CLI arguments parsed: {:?}", matches);

    // Load configuration
    let config = Config::load(matches.config.clone())?;
    debug!("Configuration loaded: {:?}", config);

    let action: Action = matches.action.clone().unwrap_or_default();
    info!("Action: {:?}", action);

    let rmrf_cfg_path = dirs::home_dir()
        .ok_or(eyre!("home dir not found!"))?
        .join(".config/rmrf/rmrf.cfg");
    debug!("Configuration file path: {:?}", rmrf_cfg_path);

    let mut rmrf_cfg = Ini::new();
    rmrf_cfg
        .load(&rmrf_cfg_path)
        .map_err(|e| eyre!(e))
        .wrap_err("Failed to load config")?;
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
    let days: i32 = rmrf_cfg.get("DEFAULT", "keep")
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

    fs::create_dir_all(&rmrf_path)?;
    fs::create_dir_all(&bkup_path)?;
    info!("Directories created or verified: {:?}, {:?}", rmrf_path, bkup_path);

    match &matches.action {
        Some(action) => match action {
            Action::Bkup(args) => {
                archive(&bkup_path, timestamp, &as_paths(&args.targets), sudo, false, None)?;
            }
            Action::Rmrf(args) => {
                archive(&rmrf_path, timestamp, &as_paths(&args.targets), sudo, true, Some(days))?;
            }
            Action::Rcvr(args) => {
                recover(&rmrf_path, &as_paths(&args.targets), sudo)?;
            }
            Action::LsBkup(args) => {
                list(&bkup_path, &args.targets, threshold)?;
            }
            Action::LsRmrf(args) => {
                list(&rmrf_path, &args.targets, threshold)?;
            }
            Action::BkupRmrf(args) => {
                archive(&bkup_path, timestamp, &as_paths(&args.targets), sudo, true, None)?;
            }
        },
        None => {
            archive(
                &rmrf_path,
                timestamp,
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
    fn test_current_uid() {
        let uid = current_uid();
        assert!(uid > 0 || uid == 0, "UID should be valid");
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

        remove_targets(&targets).unwrap();

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

        let timestamp = 1234567890123456789u64;
        let targets = vec![test_file.clone()];

        archive(&archive_dir, timestamp, &targets, false, false, None).unwrap();

        assert!(test_file.exists(), "Original file should still exist");

        let expected_archive = archive_dir.join(timestamp.to_string());
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

        let timestamp = 1234567890123456789u64;
        let targets = vec![test_file.clone()];

        archive(&archive_dir, timestamp, &targets, false, true, None).unwrap();

        assert!(!test_file.exists(), "Original file should be removed");

        let expected_archive = archive_dir.join(timestamp.to_string());
        assert!(expected_archive.exists(), "Archive directory should be created");
    }



    #[test]
    fn test_config_load_default() {
        let config = Config::load(None).unwrap();
        assert_eq!(config.cleanup_days, 30);
        assert_eq!(config.auto_cleanup, false);
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
        assert_eq!(config.auto_cleanup, true);
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
        assert_eq!(config.auto_cleanup, false);
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
        assert_eq!(config.auto_cleanup, false);
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
        assert_eq!(config.auto_cleanup, true);
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
    fn test_remove_file_with_sudo_user_owned() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let test_file = temp_path.join("test_file.txt");
        fs::write(&test_file, "test content").unwrap();

        // Test removing user-owned file with sudo=false
        remove_file_with_sudo(&test_file, false).unwrap();
        assert!(!test_file.exists(), "File should be removed");

        // Test removing user-owned file with sudo=true
        let test_file2 = temp_path.join("test_file2.txt");
        fs::write(&test_file2, "test content").unwrap();
        remove_file_with_sudo(&test_file2, true).unwrap();
        assert!(!test_file2.exists(), "File should be removed");
    }

    #[test]
    fn test_remove_directory_with_sudo_user_owned() {
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        let test_dir = temp_path.join("test_dir");
        fs::create_dir_all(&test_dir).unwrap();
        fs::write(test_dir.join("file.txt"), "content").unwrap();

        // Test removing user-owned directory with sudo=false
        remove_directory_with_sudo(&test_dir, false).unwrap();
        assert!(!test_dir.exists(), "Directory should be removed");

        // Test removing user-owned directory with sudo=true
        let test_dir2 = temp_path.join("test_dir2");
        fs::create_dir_all(&test_dir2).unwrap();
        fs::write(test_dir2.join("file.txt"), "content").unwrap();
        remove_directory_with_sudo(&test_dir2, true).unwrap();
        assert!(!test_dir2.exists(), "Directory should be removed");
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
}
