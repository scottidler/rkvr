// Standard library imports
use log::{debug, info, warn, error};
use std::fs::{self, File, DirEntry};
use std::path::{Path, PathBuf};
use std::io::{self, Write, BufWriter};
use std::process::{Command, Stdio, ChildStdin, ExitStatus};
use std::time::SystemTime;
use std::collections::HashMap;
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

static EXA_ARGS: &[&str] = &[
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
    contents: String,
}

fn as_paths(paths: &[String]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

#[derive(Parser, Debug)]
#[command(name = "rmrf", about = "tool for staging rmrf-ing or bkup-ing files")]
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

    let output = Command::new("exa")
        .args(EXA_ARGS)
        .args(targets.iter().map(|t| t.to_str().unwrap()))
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

fn archive_directory(base: &Path, target: &PathBuf, sudo: bool, cwd: &Path) -> Result<()> {
    let target_path = cwd.join(target);
    let dir_name = target.file_name().ok_or_else(|| eyre!("Failed to extract directory/file name"))?;
    let tarball_name = format!("{}.tar.gz", dir_name.to_string_lossy());
    let tarball_path = base.join(&tarball_name);
    debug!("archive_directory: target_path={} dir_name={} tarball_name={} tarball_path={}", target_path.display(), dir_name.to_string_lossy(), tarball_name, tarball_path.display());

    // The -C flag changes the working directory to `cwd` before starting the operation.
    // The `.` specifies to include the target directory/file relative to `cwd`.
    let output = if sudo {
        Command::new("sudo")
            .arg("tar")
            .args(&["-czf", tarball_path.to_str().unwrap(), "-C", cwd.to_str().unwrap(), &format!("{}", target.to_string_lossy())])
            .output()
    } else {
        Command::new("tar")
            .args(&["-czf", tarball_path.to_str().unwrap(), "-C", cwd.to_str().unwrap(), &format!("{}", target.to_string_lossy())])
            .output()
    }.wrap_err_with(|| format!("Failed to execute tar command for {}", target_path.to_string_lossy()))?;

    if !output.status.success() {
        let error_message = String::from_utf8_lossy(&output.stderr);
        debug!("Tar command failed with error: {}", error_message);
        eyre::bail!("Failed to archive {}", target.to_string_lossy());
    }

    Ok(())
}

fn archive_group(base: &Path, group: &[PathBuf], sudo: bool, cwd: &Path) -> Result<()> {
    if group.is_empty() {
        error!("An empty slice of files was passed to archive_group, indicating a critical error in the program logic.");
        return Err(eyre!("An empty slice of files was passed to archive_group, indicating a critical error in the program logic."));
    }

    let group_name = group.first().and_then(|path| path.parent()).and_then(|p| p.file_name())
        .ok_or_else(|| eyre!("Failed to derive group name"))?
        .to_string_lossy();
    let tarball_name = format!("{}.tar.gz", group_name);
    let tarball_path = base.join(&tarball_name);

    // The `-C` option changes the working directory before starting the operation
    // Here, it's changed to `cwd` and then paths are added relative to this directory
    let tar_args: Vec<String> = group.iter().map(|path| path.to_string_lossy().into_owned()).collect();

    let output = if sudo {
        let mut cmd = Command::new("sudo");
        cmd.arg("tar").arg("-czf").arg(tarball_path.to_str().unwrap()).arg("-C").arg(cwd.to_str().unwrap());
        tar_args.iter().for_each(|arg| { cmd.arg(arg); });
        cmd.output()
    } else {
        let mut cmd = Command::new("tar");
        cmd.arg("-czf").arg(tarball_path.to_str().unwrap()).arg("-C").arg(cwd.to_str().unwrap());
        tar_args.iter().for_each(|arg| { cmd.arg(arg); });
        cmd.output()
    }.wrap_err_with(|| format!("Failed to execute tar command for group {}", group_name))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eyre::bail!("Failed to archive group {}: {}", group_name, stderr);
    }

    Ok(())
}

fn categorize_paths(targets: &[PathBuf], cwd: &Path) -> Result<(Vec<PathBuf>, Vec<Vec<PathBuf>>), std::io::Error> {
    let mut directories = Vec::new();
    let mut file_groups_map: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let cwd_canonical = fs::canonicalize(cwd)?;

    for target in targets {
        let canonical_path = fs::canonicalize(target)?;
        let relative_path = match canonical_path.strip_prefix(&cwd_canonical) {
            Ok(rel_path) => rel_path.to_path_buf(),
            Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::Other, format!("Error calculating relative path: {}", e))),
        };

        // Determine if the path is a directory or if its parent is directly `cwd`
        if canonical_path.is_dir() || relative_path.parent().map_or(false, |p| p == Path::new("")) {
            directories.push(relative_path);
        } else if let Some(parent) = relative_path.parent() {
            file_groups_map.entry(parent.to_path_buf())
                .or_insert_with(Vec::new)
                .push(relative_path);
        }
    }

    let mut groups = Vec::new();
    for (_, files) in file_groups_map.into_iter() {
        groups.push(files);
    }

    Ok((directories, groups))
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
        remove_targets(&base, targets)?;
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

fn get_preferred_pager() -> String {
    std::env::var("RMRF_PAGER")
        .unwrap_or_else(|_| "less -RFX".to_string())
}

fn use_pager<F>(write_content: F) -> io::Result<()>
where
    F: FnOnce(&mut BufWriter<ChildStdin>) -> io::Result<()>,
{
    let pager_command = get_preferred_pager();
    debug!("Using pager command: {}", pager_command);
    let mut parts = pager_command.split_whitespace();
    let pager = parts.next().unwrap_or("less");
    let args = parts.collect::<Vec<&str>>();
    debug!("Using pager: {} with args: {:?}", pager, args);

    let mut pager_process = Command::new(pager)
        .args(&args)
        .stdin(Stdio::piped())
        .spawn()?;

    if let Some(stdin) = pager_process.stdin.take() {
        let mut writer = BufWriter::new(stdin);
        if let Err(e) = write_content(&mut writer) {
            if e.kind() != io::ErrorKind::BrokenPipe {
                warn!("Failed to write to pager: {}", e);
                return Err(e);
            }
        }
    }

    let status: ExitStatus = pager_process.wait()?;
    debug!("Pager exited with status: {:?}", status);
    Ok(())
}

fn list(dir_path: &Path, patterns: &[String], threshold: i64) -> Result<()> {
    let matcher = SkimMatcherV2::default();
    let dir_path = fs::canonicalize(dir_path)?;
    debug!("list: dir_path={} patterns={:?} threshold={}", dir_path.display(), patterns, threshold);

    let mut dirs: Vec<_> = fs::read_dir(&dir_path)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .collect();

    dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

    debug!("dirs: {:?}", dirs);

    if atty::is(Stream::Stdout) {
        debug!("Using pager");
        use_pager(|writer: &mut BufWriter<ChildStdin>| -> io::Result<()> {
            for dir in &dirs {
                if patterns.is_empty() || process_directory(&matcher, dir, &patterns, threshold)? {
                    print_directory(&dir.path());
                    writeln!(writer, "{}/", dir.path().display())?;
                    if let Ok(metadata_content) = fs::read_to_string(dir.path().join("metadata.yml")) {
                        writeln!(writer, "{}\n", metadata_content)?;
                    }
                }
            }
            Ok(())
        })?;
    } else {
        debug!("Not using pager");
        // Print directly to stdout if the output is being piped or redirected
        for dir in &dirs {
            if patterns.is_empty() || process_directory(&matcher, dir, &patterns, threshold)? {
                print_directory(&dir.path());
                println!("{}/", dir.path().display());
                if let Ok(metadata_content) = fs::read_to_string(dir.path().join("metadata.yml")) {
                    println!("{}\n", metadata_content);
                }
            }
        }
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

fn extract_tarball(tarball_path: &Path, restore_path: &Path) -> Result<()> {
    debug!("Extracting {} to {}", tarball_path.display(), restore_path.display());

    let output = Command::new("tar")
        .arg("-xzf")
        .arg(tarball_path)
        .arg("-C")
        .arg(restore_path)
        .output()
        .expect("Failed to execute tar command");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!("Tar extraction failed: {}", stderr));
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
        let metadata_path = target.join("metadata.yml");
        let metadata: Metadata = serde_yaml::from_reader(File::open(&metadata_path).wrap_err("Failed to open metadata.yml")?)?;

        let tarballs = find_tarballs(target);
        for tarball in tarballs {
            debug!("Recovering tarball: {}", tarball.display());
            extract_tarball(&tarball, &metadata.cwd)?;
        }
    }
    fs::remove_dir_all(dir).wrap_err("Failed to remove the directory after recovery")?;

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
