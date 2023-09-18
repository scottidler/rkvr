#![cfg_attr(debug_assertions, allow(unused_imports, unused_variables, unused_mut, dead_code))]

// Standard library imports
use log::{debug, error, info, warn};
use std::fs::{self, File, OpenOptions};
use std::io::prelude::*;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

// Third-party crate imports
use chrono::{Duration, TimeZone, Utc};
use clap::{Parser, Subcommand};
use configparser::ini::Ini;
use dirs;
use eyre::{eyre, Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use tar::Builder;
use walkdir::WalkDir;

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

#[derive(Subcommand, Debug, Default)]
enum Action {
    #[command(about = "bkup files")]
    Bkup,
    #[default]
    #[command(about = "rmrf files [default]")]
    Rmrf,
    #[command(about = "list bkup files")]
    LsBkup,
    #[command(about = "list rmrf files")]
    LsRmrf,
    #[command(about = "bkup files and rmrf the local files")]
    BkupRmrf,
}
fn make_name(path: &Path) -> Result<String> {
    debug!("Entering make_name function");
    info!("Processing path: {}", path.to_string_lossy());

    let name = path
        .file_name()
        .ok_or_else(|| eyre!("Failed to get file name"))?
        .to_str()
        .ok_or_else(|| eyre!("Failed to convert to str"))?
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

    let action: Action = matches.action.unwrap_or_default();
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

    let path = rmrf_cfg.get("DEFAULT", "path").unwrap_or("/var/tmp/rmrf".to_owned());
    let sudo: bool = rmrf_cfg.get("DEFAULT", "sudo").unwrap_or("yes".to_owned()) == "yes";
    let days: i32 = rmrf_cfg.get("DEFAULT", "keep").unwrap_or("21".to_owned()).parse()?;
    info!(
        "Configuration - path: {}, sudo: {}, keep for days: {}",
        path, sudo, days
    );

    let rmrf_path = Path::new(&path);
    let bkup_path = Path::new(&rmrf_path).join("bkup");
    debug!("rmrf_path: {:?}, bkup_path: {:?}", rmrf_path, bkup_path);

    fs::create_dir_all(&rmrf_path)?;
    fs::create_dir_all(&bkup_path)?;
    info!("Directories created or verified: {:?}, {:?}", rmrf_path, bkup_path);

    for target in targets.into_iter() {
        info!("Processing target: {:?}", target);
        match action {
            Action::Bkup => archive(&bkup_path, timestamp.to_string(), &target, sudo, false, None)?,
            Action::Rmrf => archive(&rmrf_path, timestamp.to_string(), &target, sudo, true, Some(days))?,
            Action::LsBkup => list(&bkup_path, true)?,
            Action::LsRmrf => list(&rmrf_path, true)?,
            Action::BkupRmrf => {
                archive(&bkup_path, timestamp.to_string(), &target, sudo, false, None)?;
                archive(&rmrf_path, timestamp.to_string(), &target, sudo, true, Some(days))?;
            }
        }
    }

    Ok(())
}

fn cleanup(dir_path: &std::path::Path, days: usize) -> Result<()> {
    info!("fn cleanup: dir_path={} days={}", dir_path.to_string_lossy(), days);

    let now = SystemTime::now();
    debug!("Current time: {:?}", now);

    let entries = fs::read_dir(dir_path)?;
    debug!("Directory entries read");

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

fn archive(path: &Path, timestamp: String, target: &Path, sudo: bool, remove: bool, keep: Option<i32>) -> Result<()> {
    info!(
        "fn archive: path={} timestamp={} target={} sudo={} remove={} keep={:?}",
        path.to_string_lossy(),
        timestamp,
        target.to_string_lossy(),
        sudo,
        remove,
        keep,
    );

    let name = make_name(target)?;
    debug!("Generated name: {}", name);

    let base = path.join(&timestamp);
    debug!("Base path: {}", base.to_string_lossy());

    execute(false, &vec!["mkdir", "-p", base.to_str().unwrap()])?;
    debug!("Created base directory");

    let tarball = format!("{}.tar.gz", name);
    let metadata = format!("{}.meta", name);

    let metadata_path = base.join(&metadata);
    debug!("Metadata path: {}", metadata_path.to_string_lossy());

    File::create(&metadata_path)?;
    debug!("Created metadata file");

    let mut perms = fs::metadata(&metadata_path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&metadata_path, perms)?;
    debug!("Set permissions for metadata file");

    let output = execute(
        sudo,
        &vec![
            "tar",
            "--absolute-names",
            "--preserve-permissions",
            "-cvzf",
            &tarball,
            target.to_str().unwrap(),
        ],
    )?;
    debug!("Executed tar command");

    let new_tarball_path = base.join(&tarball);
    fs::rename(&tarball, &new_tarball_path)?;
    debug!("Renamed tarball");

    if sudo {
        let current_user = whoami::username();
        execute(true, &vec!["chown", &current_user, new_tarball_path.to_str().unwrap()])?;
        debug!("Changed ownership of tarball");
    }

    if let Some(days) = keep {
        debug!("Keep for days: {}", days);
        execute(
            false,
            &vec![
                "find",
                base.to_str().unwrap(),
                "-mtime",
                &format!("+{}", days),
                "-type",
                "d",
                "-delete",
            ],
        )?;
        debug!("Executed find command for cleanup");
    }

    if remove {
        let rmrf_path = target.to_str().unwrap();
        debug!("Removing target: {}", rmrf_path);
        execute(sudo, &vec!["rm", "-rf", rmrf_path])?;
        debug!("Executed rm -rf command");
    }

    debug!("Exiting archive function");
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

fn list(dir_path: &Path, list_contents: bool) -> Result<()> {
    info!("fn list: list_contents={}", dir_path.display());

    let tarballs = find_files_with_extension(dir_path, "tar.gz");
    debug!("Found tarballs: {:?}", tarballs);

    for tarball in tarballs {
        let metadata = fs::metadata(&tarball)?;
        let size = metadata.len();
        debug!("Tarball metadata: size = {}", size);

        println!("{:?} {}K", tarball, size / 1024);
        info!("Listed tarball: {:?}", tarball);

        if list_contents {
            debug!("Listing contents of tarball: {:?}", tarball);
            println!("Contents:");
            list_tarball_contents(&Path::new(&tarball))?;
        }
    }

    let metadata = fs::metadata(dir_path)?;
    let size = metadata.len();
    debug!("Directory metadata: size = {}", size);

    println!("{:?} {}K", dir_path, size / 1024);
    info!("Listed directory: {}", dir_path.display());

    debug!("Exiting list function");
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
