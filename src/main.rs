#![cfg_attr(debug_assertions, allow(unused_imports, unused_variables, unused_mut, dead_code))]

// Standard library imports
use log::{debug, error, info, warn};
use std::fs::{self, File, OpenOptions};
use std::io::prelude::*;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;
use std::env;

// Third-party crate imports
use serde::{Serialize, Deserialize};
use chrono::{Duration, TimeZone, Utc};
use clap::{Parser, Subcommand};
use configparser::ini::Ini;
use eyre::{eyre, Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use tar::Builder;
use walkdir::WalkDir;
use dirs;

static EXA_ARGS: &[&str] = &[
    "--tree", "--long", "-a",
    "--ignore-glob=.*", "--ignore-glob=__*", "--ignore-glob=tf",
    "--ignore-glob=venv", "--ignore-glob=target", "--ignore-glob=incremental",
];

#[derive(Serialize, Deserialize, Debug)]
struct Metadata {
    cwd: PathBuf,
    contents: String,
}

#[derive(Parser, Debug)]
#[command(name = "rmrf", about = "tool for staging rmrf-ing or bkup-ing files")]
#[command(version = "0.1.0")]
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

fn make_name(path: &str) -> Result<String> {
    debug!("Entering make_name function");
    info!("Processing path: {}", path);
    let name = path
        .to_owned()
        .replace("/", "-")
        .replace(":", "_")
        .replace(" ", "_");

    debug!("Generated name: {}", name);
    Ok(name)
}

fn execute<T: AsRef<str> + std::fmt::Debug>(sudo: bool, args: &[T]) -> Result<Output> {
    info!("execute: sudo={}, args={:?}", sudo, args);

    let mut command = Command::new(if sudo { "sudo" } else { args[0].as_ref() });

    if sudo {
        command.arg(args[0].as_ref());
    }

    for arg in args[1..].iter() {
        command.arg(arg.as_ref());
    }

    let output = command.output().map_err(|e| eyre!(e));

    match &output {
        Ok(o) => debug!("Command executed successfully with output: {:?}", o),
        Err(e) => error!("Command execution failed with error: {:?}", e),
    }

    output
}

fn cleanup(dir_path: &std::path::Path, days: usize) -> Result<()> {
    info!("fn cleanup: dir_path={} days={}", dir_path.to_string_lossy(), days);

    let now = SystemTime::now();
    debug!("Current time: {:?}", now);

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
                debug!("Duration since modified: {:?}", duration_since_modified);

                if duration_since_modified > std::time::Duration::from_secs(60 * 60 * 24 * days as u64) {
                    info!("Deleting path: {}", path.to_string_lossy());

                    if metadata.is_dir() {
                        fs::remove_dir_all(&path)?;
                    } else {
                        fs::remove_file(&path)?;
                    }
                }
            }
        }
    }

    info!("Cleanup completed");
    Ok(())
}

fn list_tarball_contents(tarball_path: &Path) -> Result<()> {
    info!("fn list_tarball_contents: {}", tarball_path.display());

    let tar_gz = File::open(tarball_path)?;
    debug!("Opened tarball file");

    let tar = flate2::read::GzDecoder::new(tar_gz);
    debug!("Initialized GzDecoder");

    let mut archive = tar::Archive::new(tar);
    debug!("Created tar archive");

    for file in archive.entries()? {
        let mut file = file?;
        debug!("Reading file: {}", file.path()?.display());

        println!("  {}", file.path()?.display());
        info!("Listed file: {}", file.path()?.display());
    }

    debug!("Exiting list_tarball_contents function");
    Ok(())
}

fn get_all_timestamps(dir: &str) -> Result<Vec<String>> {
    (|| -> Result<Vec<String>> {
        Ok(fs::read_dir(dir)?
            .filter_map(|entry| {
                entry
                    .as_ref()
                    .ok()
                    .and_then(|e| e.file_type().ok())
                    .filter(|&ft| ft.is_dir())
                    .and_then(|_| entry.ok())
                    .and_then(|e| e.path().file_name().map(|s| s.to_os_string()))
                    .and_then(|s| s.into_string().ok())
            })
            .collect::<Vec<String>>())
    })()
    .wrap_err_with(|| eyre!("Failed to get all timestamps from directory: {}", dir))
}

fn list_all(dir_path: &Path) -> Result<()> {
    info!("Listing all items in directory: {}", dir_path.display());

    let entries = fs::read_dir(dir_path)?;
    for entry in entries {
        if let Ok(entry) = entry {
            let path = entry.path();
            let metadata = fs::metadata(&path)?;
            let size = metadata.len();
            println!("{:?} {}K", path, size / 1024);
            info!("Listed item: {:?}", path);
        }
    }

    Ok(())
}

fn find_files_with_extension(dir_path: &Path, ext: &str) -> Vec<String> {
    info!(
        "fn find_files_with_extension: dir_path={} ext={}",
        dir_path.display(),
        ext
    );

    let mut result = vec![];

    if let Ok(entries) = fs::read_dir(dir_path) {
        debug!("Successfully read directory: {}", dir_path.display());

        for entry in entries {
            if let Ok(entry) = entry {
                let path = entry.path();
                debug!("Checking path: {}", path.display());

                if path.is_file() && path.extension().unwrap() == ext {
                    debug!("Found matching file: {}", path.display());
                    result.push(path.to_string_lossy().into_owned());
                }
            }
        }
    } else {
        warn!("Failed to read directory: {}", dir_path.display());
    }

    info!("Found {} files with extension: {}", result.len(), ext);
    debug!("Exiting find_files_with_extension function");

    result
}

fn find_files_older_than(dir_path: &Path, days: usize) -> Vec<String> {
    info!(
        "fn find_files_older_than: dir_path={} days={}",
        dir_path.display(),
        days
    );

    let now = SystemTime::now();
    let duration = std::time::Duration::from_secs((days * 24 * 60 * 60) as u64);
    let cutoff = now - duration;
    debug!("Calculated cutoff time: {:?}", cutoff);

    let mut result = vec![];

    if let Ok(entries) = fs::read_dir(dir_path) {
        debug!("Successfully read directory: {}", dir_path.display());

        for entry in entries {
            if let Ok(entry) = entry {
                let path = entry.path();
                debug!("Checking path: {}", path.display());

                if let Ok(metadata) = fs::metadata(&path) {
                    if let Ok(modified) = metadata.modified() {
                        debug!("File last modified at: {:?}", modified);

                        if modified < cutoff {
                            debug!("Found file older than cutoff: {}", path.display());
                            result.push(path.to_string_lossy().into_owned());
                        }
                    } else {
                        warn!("Failed to get modified time for: {}", path.display());
                    }
                } else {
                    warn!("Failed to get metadata for: {}", path.display());
                }
            }
        }
    } else {
        warn!("Failed to read directory: {}", dir_path.display());
    }

    info!("Found {} files older than {} days", result.len(), days);
    debug!("Exiting find_files_older_than function");

    result
}

fn create_metadata(base: &Path, cwd: &Path, targets: &[String]) -> Result<()> {
    info!("fn create_metadata: base={} cwd={} targets={:?}", base.display(), cwd.display(), targets);

    let output = Command::new("exa")
        .args(EXA_ARGS)
        .args(targets)
        .output()
        .wrap_err("Failed to execute exa command")?;

    let metadata_content = String::from_utf8_lossy(&output.stdout);
    debug!("Metadata content: {}", metadata_content);

    let metadata = Metadata {
        cwd: cwd.to_path_buf(),
        contents: metadata_content.to_string(),
    };

    let yaml_metadata = serde_yaml::to_string(&metadata).wrap_err("Failed to serialize metadata to YAML")?;
    let metadata_path = base.join("metadata.yml");
    fs::write(&metadata_path, yaml_metadata.as_bytes()).wrap_err("Failed to write metadata file")?;
    Ok(())
}

fn archive_target(base: &Path, target_str: &str, sudo: bool, cwd: &Path) -> Result<PathBuf> {
    let target_path = cwd.join(target_str);
    let parent_dir = target_path.parent().unwrap_or(cwd);
    let file_name = target_path.file_name().ok_or_else(|| eyre!("Failed to get file name from path: {}", target_str))?;
    let tarball_name = format!("{}.tar.gz", file_name.to_string_lossy());
    let tarball_path = base.join(&tarball_name);

    let output = if sudo {
        Command::new("sudo")
            .arg("tar")
            .args(&["-czf", tarball_path.to_str().unwrap(), "-C", parent_dir.to_str().unwrap(), file_name.to_str().unwrap()])
            .output()
    } else {
        Command::new("tar")
            .args(&["-czf", tarball_path.to_str().unwrap(), "-C", parent_dir.to_str().unwrap(), file_name.to_str().unwrap()])
            .output()
    }.wrap_err_with(|| format!("Failed to execute tar command for {}", file_name.to_string_lossy()))?;

    if !output.status.success() {
        eyre::bail!("Failed to archive {}", file_name.to_string_lossy());
    }

    Ok(target_path)
}

fn remove_targets(base: &Path, targets: &[PathBuf]) -> Result<()> {
    for target in targets {
        if target.is_dir() {
            fs::remove_dir_all(target)?;
        } else {
            fs::remove_file(target)?;
        }
        println!("{}", target.display());
    }
    println!("-> {}/", base.display());
    Ok(())
}

fn archive(path: &Path, timestamp: u64, targets: &[String], sudo: bool, remove: bool, keep: Option<i32>) -> Result<()> {
    let cwd = std::env::current_dir().wrap_err("Failed to get current directory")?;
    let base = path.join(timestamp.to_string());
    fs::create_dir_all(&base).wrap_err("Failed to create base directory")?;

    create_metadata(&base, &cwd, targets)?;

    let target_paths: Vec<_> = targets.iter()
        .map(|target_str| archive_target(&base, target_str, sudo, &cwd))
        .collect::<Result<Vec<_>, _>>()?;

    if remove {
        remove_targets(&base, &target_paths)?;
    }

    if let Some(days) = keep {
        cleanup(&base, days as usize)?;
    }

    Ok(())
}

fn list(dir_path: &Path, targets: &[String]) -> Result<()> {
    let mut dirs: Vec<_> = fs::read_dir(dir_path)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir() && (targets.is_empty() || targets.contains(&entry.file_name().to_string_lossy().into_owned())))
        .collect();

    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.file_name().to_string_lossy().into_owned()));

    for dir in dirs {
        let metadata_path = dir.path().join("metadata.yml");
        if metadata_path.exists() {
            let metadata_content = fs::read_to_string(metadata_path)?;
            let metadata: Metadata = serde_yaml::from_str(&metadata_content)?;

            println!("{}/", dir.path().display());
            println!("{}", metadata.contents);
        }
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
    let targets: Vec<PathBuf> = matches
        .targets
        .iter()
        .map(|f| fs::canonicalize(f).unwrap().to_string_lossy().into_owned())
        .map(|f| PathBuf::from(f))
        .collect();
    info!("Action: {:?}, Targets: {:?}", action, targets);

    let rmrf_cfg_path = dirs::home_dir()
        .ok_or(eyre!("home dir not found!"))?
        .join(".config/rmrf/rmrf2.cfg");
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

    info!(
        "Configuration - rmrf_path: {:?}, bkup_path: {:?}, sudo: {}, keep for days: {}",
        rmrf_path, bkup_path, sudo, days
    );

    debug!("rmrf_path: {:?}, bkup_path: {:?}", rmrf_path, bkup_path);

    fs::create_dir_all(&rmrf_path)?;
    fs::create_dir_all(&bkup_path)?;
    info!("Directories created or verified: {:?}, {:?}", rmrf_path, bkup_path);

    match &matches.action {
        Some(action) => match action {
            Action::Bkup(args) => {
                archive(&bkup_path, timestamp, &args.targets, sudo, false, None)?;
            },
            Action::Rmrf(args) => {
                archive(&rmrf_path, timestamp, &args.targets, sudo, true, Some(days))?;
            },
            Action::LsBkup(args) => {
                list(&bkup_path, &args.targets)?;
            },
            Action::LsRmrf(args) => {
                list(&rmrf_path, &args.targets)?;
            },
            Action::BkupRmrf(args) => {
                archive(&bkup_path, timestamp, &args.targets, sudo, true, None)?;
            }
        },
        None => {
            // This is the default Rmrf action
            archive(&rmrf_path, timestamp, &matches.targets, sudo, true, Some(days))?;
        }
    }

    Ok(())
}
