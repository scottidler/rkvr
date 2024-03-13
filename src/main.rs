#![allow(unused_variables)]

// Standard library imports
use log::{debug, info, warn, error};
use std::fs::{self, File, DirEntry};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;
use std::env;

// Third-party crate imports
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use serde::{Serialize, Deserialize};
use clap::{Parser, Subcommand};
use configparser::ini::Ini;
use eyre::{eyre, Context, Result};
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

fn as_paths(paths: &[String]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
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

fn create_metadata(base: &Path, cwd: &Path, targets: &[PathBuf]) -> Result<()> {
    info!("fn create_metadata: base={} cwd={} targets={:?}", base.display(), cwd.display(), targets);

    let output = Command::new("exa")
        .args(EXA_ARGS)
        .args(targets.iter().map(|t| t.to_str().unwrap())) // Adjusted to convert PathBuf to &str
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

fn archive_target(base: &Path, target: &PathBuf, sudo: bool, cwd: &Path) -> Result<PathBuf> {
    let target_path = cwd.join(target);
    let parent_dir = target_path.parent().unwrap_or(cwd);
    let file_name = target_path.file_name().ok_or_else(|| eyre!("Failed to get file name from path: {}", target.display()))?;
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

fn archive(path: &Path, timestamp: u64, targets: &[PathBuf], sudo: bool, remove: bool, keep: Option<i32>) -> Result<()> {
    let cwd = env::current_dir().wrap_err("Failed to get current directory")?;
    let base = path.join(timestamp.to_string());
    fs::create_dir_all(&base).wrap_err("Failed to create base directory")?;

    create_metadata(&base, &cwd, targets)?;

    let target_paths: Vec<_> = targets.iter()
        .map(|target| archive_target(&base, target, sudo, &cwd))
        .collect::<Result<Vec<_>, _>>()?;

    if remove {
        remove_targets(&base, &target_paths)?;
    }

    if let Some(days) = keep {
        cleanup(&base, days as usize)?;
    }

    Ok(())
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

fn print_directory(dir_path: &Path) {
    println!("{}/", dir_path.display());
    if let Ok(metadata_content) = fs::read_to_string(dir_path.join("metadata.yml")) {
        println!("{}", metadata_content);
    }
}

fn process_pattern(matcher: &SkimMatcherV2, dir_name: &str, full_path: &PathBuf, pattern: &str, threshold: i64) -> Result<bool, std::io::Error> {
    if matcher.fuzzy_match(dir_name, pattern).is_some() ||
       matcher.fuzzy_match(full_path.to_str().unwrap_or_default(), pattern).is_some() {
        return Ok(true);
    }

    let metadata_path = full_path.join("metadata.yml");
    if metadata_path.exists() {
        let metadata_content = fs::read_to_string(&metadata_path)?;
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

fn process_directory(matcher: &SkimMatcherV2, dir: &DirEntry, patterns: &[String], threshold: i64) -> Result<bool, std::io::Error> {
    let dir_name = dir.file_name().to_string_lossy().to_string();
    let full_path = dir.path().canonicalize()?;

    if !patterns.is_empty() {
        for pattern in patterns {
            if process_pattern(matcher, &dir_name, &full_path, pattern, threshold)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }

    Ok(true)
}

fn list(dir_path: &Path, patterns: &[String], threshold: i64) -> Result<()> {
    let matcher = SkimMatcherV2::default();
    let dir_path = fs::canonicalize(dir_path)?;

    let mut dirs: Vec<_> = fs::read_dir(&dir_path)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .collect();

    dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

    let mut any_matches = false;

    for dir in &dirs {
        if patterns.is_empty() || process_directory(&matcher, dir, &patterns, threshold)? {
            print_directory(&dir.path());
            any_matches = true;
        }
    }

    if !any_matches && !patterns.is_empty() {
        warn!("No matches found for the given search term(s).");
    }

    Ok(())
}

// given the following timestamp directory:
// ~ via 🐍 v3.10.12 via 🦀 v1.76.0 on ☁️  (us-west-2) on ☁️
// ❯ tree -a -I '.*|__*|tf|venv|target|incremental' /var/tmp/rmrf2/1708380459/
// /var/tmp/rmrf2/1708380459/
// ├── apple.tar.gz
// ├── banana.tar.gz
// └── metadata.yml
//
// The user can supply one of the following targets to recover:
// /var/tmp/rmrf2/1708380459/   this will recover all of the files: apple.tar.gz, banana.tar.gz
//
// After the files have been successfully recovered, the program will remove the timestamp directory.
//
// The process should be the same, get the recovery path by getting the cwd value by loading the
// metata.yml file. Then the untar should place the files relative to the cwd value.

fn recover(dir: &Path, targets: &[PathBuf]) -> Result<()> {
    let dir = dir.canonicalize().wrap_err("Failed to canonicalize rmrf path")?;
    debug!("recover: dir={} targets={}", dir.display(), targets.iter().map(|t| t.to_string_lossy()).collect::<Vec<_>>().join(", "));

    for target in targets {
        debug!("target_path={}", target.display());

        let target_path = if target.is_absolute() {
            target.canonicalize().wrap_err("Failed to canonicalize target path")?
        } else {
            dir.join(target).canonicalize().wrap_err("Failed to canonicalize combined target path")?
        };
        debug!("canonical target_path={}", target_path.display());

        if !target_path.starts_with(&dir) {
            error!("Target path is not within the specified directory: {}", target_path.display());
            continue;
        }

        let metadata_path = target_path.join("metadata.yml");
        debug!("metadata_path={}", metadata_path.display());
        let metadata: Metadata = serde_yaml::from_reader(File::open(&metadata_path).wrap_err("Failed to open metadata.yml")?)?;

        let tarballs = find_tarballs(&target_path);
        debug!("tarballs={:?}", tarballs);

        for entry in tarballs {
            extract_tarball(&entry.as_path(), &metadata.cwd)?;
        }

        fs::remove_dir_all(&target_path).wrap_err("Failed to remove the target directory after recovery")?;
    }

    Ok(())
}

fn find_tarballs(dir_path: &Path) -> Vec<PathBuf> {
    debug!("find_tarballs: dir_path={}", dir_path.display());
    let mut tarballs = Vec::new();
    if dir_path.is_dir() {
        for entry in fs::read_dir(dir_path).expect("Directory not found") {
            let entry = entry.expect("Failed to read entry");
            let path = entry.path();
            if path.is_file() && is_tar_gz(&path) {
                tarballs.push(path);
            } else {
                debug!("Skipping non-tarball file: {}", path.display());
            }
        }
    }
    tarballs
}

fn is_tar_gz(path: &Path) -> bool {
    match path.to_str() {
        Some(s) => s.ends_with(".tar.gz"),
        None => false,
    }
}

fn extract_tarball(tarball_path: &Path, destination: &Path) -> Result<()> {
    Command::new("tar")
        .arg("-xzf")
        .arg(tarball_path)
        .arg("-C")
        .arg(destination)
        .status()
        .wrap_err_with(|| format!("Failed to extract tarball: {}", tarball_path.display()))?;

    info!("Successfully recovered {}", tarball_path.display());
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
    let threshold: i64 = rmrf_cfg.get("DEFAULT", "threshold").unwrap_or("70".to_owned()).parse()?;

    info!(
        "Configuration - rmrf_path: {:?}, bkup_path: {:?}, sudo: {}, keep for days: {}, threshold: {}",
        rmrf_path, bkup_path, sudo, days, threshold,
    );

    debug!("rmrf_path: {:?}, bkup_path: {:?}", rmrf_path, bkup_path);

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
                recover(&rmrf_path, &as_paths(&args.targets))?;
            },
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
            // This is the default Rmrf action
            archive(&rmrf_path, timestamp, &as_paths(&matches.targets), sudo, true, Some(days))?;
        }
    }

    Ok(())
}
