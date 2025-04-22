// src/main.rs
use log::{debug, info};
use std::fs::{self, File, DirEntry};
use std::path::{Path, PathBuf};
use std::io::{self, Write, BufWriter};
use std::process::{Command, Stdio, ChildStdin};
use std::time::SystemTime;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use libc::getuid;
use std::env;

// Third-party crate imports
use rayon::prelude::*;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use serde::{Serialize, Deserialize};
use clap::{Parser, Subcommand};
use configparser::ini::Ini;
use eyre::{eyre, Context, Result};
use atty::Stream;
use dirs;

static EZA_ARGS: &[&str] = &[
    "--tree", "--long", "-a",
    "--ignore-glob=.*", "--ignore-glob=__*", "--ignore-glob=tf",
    "--ignore-glob=venv", "--ignore-glob=target", "--ignore-glob=incremental",
];

mod built_info {
    include!(concat!(env!("OUT_DIR"), "/git_describe.rs"));
}

#[derive(Serialize, Deserialize, Debug)]
struct Metadata {
    cwd: PathBuf,
    #[serde(default)]
    targets: Vec<String>,
    contents: String,
}

fn as_paths(paths: &[String]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

#[derive(Parser, Debug)]
#[command(name = "rkvr", about = "tool for staging rmrf-ing or bkup-ing files")]
#[command(version = built_info::GIT_DESCRIBE)]
#[command(author = "Scott A. Idler <scott.a.idler@gmail.com>")]
#[command(arg_required_else_help = true)]
struct Cli {
    #[arg(name = "targets")]
    targets: Vec<String>,

    #[command(subcommand)]
    action: Option<Action>,
}

#[derive(Parser, Clone, Debug)]
struct Args {
    #[arg(name = "targets")]
    targets: Vec<String>,
}

#[derive(Subcommand, Clone, Debug)]
enum Action {
    #[command(about = "bkup files")]
    Bkup(Args),
    #[command(about = "rmrf files [default]")]
    Rmrf(Args),
    #[command(about = "recover rmrf|bkup files")]
    Rcvr(Args),
    #[command(about = "list bkup files")]
    LsBkup(Args),
    #[command(about = "list rmrf files")]
    LsRmrf(Args),
    #[command(about = "bkup files and rmrf the local files")]
    BkupRmrf(Args),
}

impl Default for Action {
    fn default() -> Self {
        Action::Rmrf(Args { targets: vec![] })
    }
}

fn current_uid() -> u32 {
    // unsafe call into libc
    unsafe { getuid() as u32 }
}

fn file_uid(path: &Path) -> eyre::Result<u32> {
    Ok(fs::metadata(path)?.uid())
}
fn cleanup(dir_path: &std::path::Path, days: usize) -> Result<()> {
    info!(
        "fn cleanup: dir_path={} days={}",
        dir_path.to_string_lossy(),
        days
    );

    let now = SystemTime::now();
    debug!("Current time: {:?}", now);

    let delete_threshold = std::time::Duration::from_secs(60 * 60 * 24 * days as u64);
    debug!(
        "Delete threshold duration: {:?} ({} days)",
        delete_threshold, days
    );

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
                        fs::remove_dir_all(&path)?;
                    } else {
                        debug!("Removing file: {}", path.to_string_lossy());
                        fs::remove_file(&path)?;
                    }
                }
            }
        }
    }

    info!("Cleanup completed");
    Ok(())
}

fn create_metadata(base: &Path, cwd: &Path, targets: &[PathBuf]) -> Result<()> {
    info!("fn create_metadata: base={} cwd={} targets={:?}", base.display(), cwd.display(), targets);

    let output = Command::new("eza")
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

fn create_tar_command(
    sudo: bool,
    tarball_path: &Path,
    cwd: &Path,
    targets: Vec<String>,
) -> Result<Command> {
    let relative_targets: Vec<String> = targets
        .into_iter()
        .map(|t| {
            Path::new(&t)
                .strip_prefix(cwd)
                .unwrap_or(Path::new(&t))
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    if sudo {
        let mut cmd = Command::new("sudo");
        cmd.args(&["tar", "-czf", tarball_path.to_str().unwrap(), "-C", cwd.to_str().unwrap()]);
        cmd.args(&relative_targets);
        Ok(cmd)
    } else {
        let mut cmd = Command::new("tar");
        cmd.args(&["-czf", tarball_path.to_str().unwrap(), "-C", cwd.to_str().unwrap()]);
        cmd.args(&relative_targets);
        Ok(cmd)
    }
}

fn archive_directory(
    base: &Path,
    target: &PathBuf,
    sudo: bool,
    cwd: &Path,
) -> Result<()> {
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
        .unwrap_or_else(|_| target.to_string_lossy().into_owned());

    let mut cmd = create_tar_command(need_sudo, &tarball_path, cwd, vec![rel])?;
    let status = cmd.status()?;
    if !status.success() {
        eyre::bail!(
            "Failed to archive {} (status {})",
            target.display(),
            status
        );
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
            // user‑owned: normal copy + preserve perms
            fs::copy(src, &dest)?;
            fs::set_permissions(&dest, fs::metadata(src)?.permissions())?;
        } else {
            // root‑owned (or other): require sudo
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

fn tar_gz_files(
    base: &Path,
    group: &[PathBuf],
    sudo: bool,
    cwd: &Path,
) -> Result<()> {
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
                .unwrap_or_else(|_| p.to_string_lossy().into_owned())
        })
        .collect();

    let mut cmd = create_tar_command(sudo, &tarball_path, cwd, relative_targets)?;
    let status = cmd.status()?;
    if !status.success() {
        eyre::bail!(
            "Failed to create {} (status {})",
            tarball_path.display(),
            status
        );
    }

    Ok(())
}

fn archive_group(
    base: &Path,
    group: &[PathBuf],
    sudo: bool,
    cwd: &Path,
) -> Result<()> {
    let need_sudo = group.iter()
        .map(|p| file_uid(p))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .any(|uid| uid != current_uid());

    if need_sudo && !sudo {
        eyre::bail!(
            "Found files owned by another user; re‑run with `sudo = yes` in your config"
        );
    }

    let (bundle, loose): (Vec<_>, Vec<_>) = group
        .iter()
        .cloned()
        .partition(|path| !is_archive(path));

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
        let canonical_path = fs::canonicalize(target)
            .wrap_err_with(|| format!("Failed to canonicalize target: {}", target.display()))?;
        debug!("Canonicalized target: {}", canonical_path.display());

        let relative_path = match canonical_path.strip_prefix(&cwd_canonical) {
            Ok(rel_path) => rel_path.to_path_buf(),
            Err(e) => {
                debug!("Unable to strip prefix from path '{}': {}", canonical_path.display(), e);
                canonical_path.clone()
            },
        };
        debug!("Relative path: {}", relative_path.display());

        if canonical_path.is_dir() {
            directories.push(canonical_path);
        } else {
            let group_key = relative_path.parent()
                .map(|p| cwd_canonical.join(p))
                .unwrap_or_else(|| canonical_path.parent().unwrap().to_path_buf());
            file_groups_map.entry(group_key)
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

fn output(base: &Path, targets: &[PathBuf]) {
    for target in targets {
        println!("{}", target.display());
    }
    println!("-> {}/", base.display());
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

fn archive(path: &Path, timestamp: u64, targets: &[PathBuf], sudo: bool, remove: bool, keep: Option<i32>) -> Result<()> {
    let cwd = env::current_dir().wrap_err("Failed to get current directory")?;
    let base = path.join(timestamp.to_string());
    fs::create_dir_all(&base).wrap_err("Failed to create base directory")?;

    create_metadata(&base, &cwd, targets)?;

    let (directories, groups) = categorize_paths(targets, &cwd)?;

    directories.par_iter().try_for_each(|directory| {
        archive_directory(&base, directory, sudo, &cwd)
    })?;

    groups.par_iter().try_for_each(|group| {
        archive_group(&base, group, sudo, &cwd)
    })?;

    if remove {
        remove_targets(targets)?;
    }
    output(&base, targets);

    if let Some(days) = keep {
        cleanup(&path, days as usize)?;
    }

    Ok(())
}

fn get_preferred_pager() -> String {
    std::env::var("RMRF_PAGER")
        .unwrap_or_else(|_| "less -RFX".to_string())
}

fn use_pager<F>(write_content: F) -> Result<()>
where
    F: FnOnce(&mut BufWriter<ChildStdin>) -> Result<()>,
{
    let pager_command = get_preferred_pager();
    let mut parts = pager_command.split_whitespace();
    let pager = parts.next().unwrap_or("less");
    let args = parts.collect::<Vec<&str>>();

    let mut pager_process = Command::new(pager)
        .args(&args)
        .stdin(Stdio::piped())
        .spawn()?;

    if let Some(stdin) = pager_process.stdin.take() {
        let mut writer = BufWriter::new(stdin);
        if let Err(e) = write_content(&mut writer) {
            if e.downcast_ref::<io::Error>().map_or(true, |io_err| io_err.kind() != io::ErrorKind::BrokenPipe) {
                return Err(e);
            }
        }
    }

    let _status = pager_process.wait()?;
    Ok(())
}

fn process_pattern(matcher: &SkimMatcherV2, dir_name: &str, full_path: &PathBuf, pattern: &str, threshold: i64) -> Result<bool> {
    if matcher.fuzzy_match(dir_name, pattern).is_some() ||
       matcher.fuzzy_match(full_path.to_str().unwrap_or_default(), pattern).is_some() {
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
    let full_path = dir.path().canonicalize().wrap_err("Failed to canonicalize directory path")?;

    for pattern in patterns {
        if process_pattern(matcher, &dir_name, &full_path, pattern, threshold)? {
            return Ok(true);
        }
    }

    Ok(patterns.is_empty())
}

fn format_directory(dir_path: &Path) -> Result<String> {
    let mut output = format!("{}/", dir_path.display());
    let metadata_path = dir_path.join("metadata.yml");
    if let Ok(metadata_content) = fs::read_to_string(&metadata_path) {
        let indented_content: Vec<String> = metadata_content.lines()
            .map(|line| format!("  {}", line))
            .collect();
        let indented_content_str = indented_content.join("\n");
        output += &format!("\n{}\n", indented_content_str);
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
                if patterns.is_empty() || process_directory(&matcher, dir, &patterns, threshold).map_err(|e| io::Error::new(io::ErrorKind::Other, e))? {
                    let dir_output = format_directory(&dir.path()).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
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
                "tar", "xpf",
                bundle.to_str().unwrap(),
                "-C", restore_to.to_str().unwrap(),
                "--same-owner",
            ])
            .status()?
    } else {
        Command::new("tar")
            .args(&[
                "xzf",
                bundle.to_str().unwrap(),
                "-C", restore_to.to_str().unwrap(),
            ])
            .status()?
    };

    if !status.success() {
        eyre::bail!("tar extraction failed with status {}", status);
    }
    Ok(())
}

fn recover(root: &Path, ts_dirs: &[PathBuf], sudo: bool) -> Result<()> {
    for ts in ts_dirs {
        let ts_path = if ts.is_absolute() {
            ts.clone()
        } else {
            root.join(ts)
        };
        let ts_dir = ts_path
            .canonicalize()
            .wrap_err("canonicalizing timestamp dir")?;

        let meta_path = ts_dir.join("metadata.yml");
        let meta: Metadata = serde_yaml::from_reader(
            File::open(&meta_path).wrap_err("opening metadata.yml")?
        )
        .wrap_err("parsing metadata.yml")?;
        let cwd = meta.cwd;
        let originals = &meta.targets;

        let (to_copy, to_extract): (Vec<PathBuf>, Vec<PathBuf>) =
            fs::read_dir(&ts_dir)?
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

        fs::remove_dir_all(&ts_dir)
            .wrap_err_with(|| format!("removing {}", ts_dir.display()))?;
    }
    Ok(())
}

fn main() -> Result<()> {
    env_logger::init();
    let args = std::env::args().collect::<Vec<String>>();
    info!("main: args={:?}", args);

    let current_level = log::max_level();
    debug!("Current log level: {:?}", current_level);

    let timestamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
    debug!("Current timestamp: {}", timestamp);

    let matches = Cli::parse_from(args);
    debug!("CLI arguments parsed: {:?}", matches);

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
    let days: i32 = rmrf_cfg.get("DEFAULT", "keep").unwrap_or("21".to_owned()).parse()?;
    let threshold: i64 = rmrf_cfg.get("DEFAULT", "threshold").unwrap_or("70".to_owned()).parse()?;

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
            },
            Action::Rmrf(args) => {
                archive(&rmrf_path, timestamp, &as_paths(&args.targets), sudo, true, Some(days))?;
            },
            Action::Rcvr(args) => {
                recover(&rmrf_path, &as_paths(&args.targets), sudo)?;
            }
            Action::LsBkup(args) => {
                list(&bkup_path, &args.targets, threshold)?;
            },
            Action::LsRmrf(args) => {
                list(&rmrf_path, &args.targets, threshold)?;
            },
            Action::BkupRmrf(args) => {
                archive(&bkup_path, timestamp, &as_paths(&args.targets), sudo, true, None)?;
            }
        },
        None => {
            archive(&rmrf_path, timestamp, &as_paths(&matches.targets), sudo, true, Some(days))?;
        }
    }

    Ok(())
}
