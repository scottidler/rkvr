#![cfg_attr(debug_assertions, allow(unused_imports, unused_variables, unused_mut, dead_code))]

// Standard library imports
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

#[derive(Parser)]
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
// make a name from the basename of the path
fn make_name(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .ok_or_else(|| eyre!("Failed to get file name"))?
        .to_str()
        .ok_or_else(|| eyre!("Failed to convert to str"))?
        .to_owned()
        .replace("/", "-")
        .replace(":", "_")
        .replace(" ", "_");

    Ok(name)
}

fn execute<T: AsRef<str>>(sudo: bool, args: &[T]) -> Result<Output> {
    let mut command = Command::new(if sudo { "sudo" } else { args[0].as_ref() });

    if sudo {
        command.arg(args[0].as_ref());
    }

    for arg in args[1..].iter() {
        command.arg(arg.as_ref());
    }

    command.output().map_err(|e| eyre!(e))
}

fn main() -> Result<()> {
    let timestamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs();

    let matches = Cli::parse();
    let action: Action = matches.action.unwrap_or_default();
    let targets: Vec<PathBuf> = matches
        .targets
        .iter()
        .map(|f| fs::canonicalize(f).unwrap().to_string_lossy().into_owned())
        .map(|f| PathBuf::from(f))
        .collect();

    let rmrf_cfg_path = dirs::home_dir()
        .ok_or(eyre!("home dir not found!"))?
        .join(".config/rmrf/rmrf2.cfg");
    let mut rmrf_cfg = Ini::new();
    rmrf_cfg
        .load(&rmrf_cfg_path)
        .map_err(|e| eyre!(e))
        .wrap_err("Failed to load config")?;
    let path = rmrf_cfg.get("DEFAULT", "path").unwrap_or("/var/tmp/rmrf".to_owned());
    let sudo: bool = rmrf_cfg.get("DEFAULT", "sudo").unwrap_or("yes".to_owned()) == "yes";
    let days: i32 = rmrf_cfg.get("DEFAULT", "keep").unwrap_or("21".to_owned()).parse()?;

    let rmrf_path = Path::new(&path);
    let bkup_path = Path::new(&rmrf_path).join("bkup");

    fs::create_dir_all(&rmrf_path)?;
    fs::create_dir_all(&bkup_path)?;

    for target in targets.into_iter() {
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
    let now = SystemTime::now();

    // Read the directory
    let entries = fs::read_dir(dir_path)?;

    for entry in entries {
        if let Ok(entry) = entry {
            let path = entry.path();
            let metadata = fs::metadata(&path)?;

            // Get the modified time as a SystemTime
            let modified_time = metadata.modified()?;

            // Calculate the duration since the file was last modified
            if let Ok(duration_since_modified) = now.duration_since(modified_time) {
                // Convert days to duration and compare
                if duration_since_modified > std::time::Duration::from_secs(60 * 60 * 24 * days as u64) {
                    // Delete the file or directory
                    if metadata.is_dir() {
                        fs::remove_dir_all(&path)?;
                    } else {
                        fs::remove_file(&path)?;
                    }
                }
            }
        }
    }

    Ok(())
}

fn archive(path: &Path, timestamp: String, target: &Path, sudo: bool, remove: bool, keep: Option<i32>) -> Result<()> {
    println!(
        "archive: path={} timestamp={} target={}",
        path.to_string_lossy(),
        timestamp,
        target.to_string_lossy()
    );
    let name = make_name(target)?;
    println!("name={}", name);
    let base = path.join(&timestamp);
    println!("base={}", base.to_string_lossy());
    execute(false, &vec!["mkdir", "-p", base.to_str().unwrap()])?;

    if target == &base {
        println!("{} ->", path.to_string_lossy());
        let output = execute(
            false,
            &vec![
                "tar",
                "--absolute-names",
                "-xvf",
                target.to_str().ok_or(eyre!("Failed to convert path to string"))?,
            ],
        )?;
        print!("  {}", String::from_utf8_lossy(&output.stdout));
        return Ok(());
    }
    let tarball = format!("{}.tar.gz", name);
    let metadata = format!("{}.meta", name);

    let tree_path_buf = path.join(target);
    let tree_path = tree_path_buf.to_str().ok_or(eyre!("Couldn't run tree command"))?;

    let ls_path_buf = path.join(target);
    let ls_path = ls_path_buf.to_str().ok_or(eyre!("Couldn't run ls command"))?;

    let output = if path.join(target).metadata()?.is_dir() {
        execute(sudo, &vec!["tree", "-a", "-h", "-L", "2", tree_path])?
    } else {
        execute(sudo, &vec!["ls", "-alh", ls_path])?
    };

    let metadata_path = base.join(&metadata);
    println!("metadata_path={}", metadata_path.to_string_lossy());
    File::create(&metadata_path).expect("create file failed");

    let mut perms = fs::metadata(&metadata_path)?.permissions();
    perms.set_mode(0o755); // Use octal notation for permissions
    fs::set_permissions(&metadata_path, perms)?;

    let mut metadata_file = OpenOptions::new().write(true).open(metadata_path)?;
    metadata_file.write_all(&output.stdout)?;

    let output = execute(
        false,
        &vec![
            "tar",
            "--absolute-names",
            "--preserve-permissions",
            "-cvzf",
            &tarball,
            path.join(target)
                .to_str()
                .ok_or(eyre!("Failed to convert path to string"))?,
        ],
    )?;

    print!("  {}", String::from_utf8_lossy(&output.stdout));

    let new_tarball_path = base.join(&tarball);
    fs::rename(&tarball, &new_tarball_path)?;
    println!("-> {}", new_tarball_path.to_string_lossy());

    if let Some(days) = keep {
        let output = execute(
            false,
            &vec![
                "find",
                base.to_string_lossy().as_ref(),
                "-mtime",
                &format!("+{}", days),
                "-type",
                "d",
                "-print",
            ],
        )?;
        let deleted_dirs = String::from_utf8_lossy(&output.stdout);
        if !deleted_dirs.trim().is_empty() {
            println!("{}", deleted_dirs);
            println!("-> /dev/null");
        }
        execute(
            false,
            &vec![
                "find",
                base.to_string_lossy().as_ref(),
                "-mtime",
                &format!("+{}", days),
                "-type",
                "d",
                "-delete",
            ],
        )?;
    }

    // Debugging: Print target path before any delete operation
    println!("Target path: {:?}", target);
    if !target.exists() {
        println!("Target does not exist.");
        return Err(eyre!("Target does not exist"));
    }

    // Create a path for the new tarball
    let tarball_path = base.join(format!("{}.tar.gz", name));

    // Create and open a new file for the tarball
    let tar_gz = File::create(&tarball_path)?;

    // Create a GzEncoder with the default compression level
    let enc = GzEncoder::new(tar_gz, Compression::default());

    // Create a new tar builder
    let mut tar = Builder::new(enc);

    // Check if the target is a directory or a file
    let metadata = fs::metadata(target)?;
    if metadata.is_dir() {
        // Append a directory to the tarball
        tar.append_dir_all(name, target)?;
    } else {
        // Append a file to the tarball
        tar.append_path_with_name(target, name)?;
    }

    // Finalize the tarball
    tar.into_inner()?;

    let rmrf_path = target.to_str().ok_or(eyre!("Error running rm -rf"))?;
    if remove {
        println!("rm -rf {}", target.to_string_lossy());
        execute(sudo, &vec!["rm", "-rf", rmrf_path])?;
    }

    Ok(())
}

fn list_tarball_contents(tarball_path: &Path) -> Result<()> {
    let tar_gz = File::open(tarball_path)?;
    let tar = flate2::read::GzDecoder::new(tar_gz);
    let mut archive = tar::Archive::new(tar);

    for file in archive.entries()? {
        let mut file = file?;
        println!("  {}", file.path()?.display());
    }

    Ok(())
}

fn list(dir_path: &Path, list_contents: bool) -> Result<()> {
    let tarballs = find_files_with_extension(dir_path, "tar.gz");
    for tarball in tarballs {
        let metadata = fs::metadata(&tarball)?;
        let size = metadata.len();
        println!("{:?} {}K", tarball, size / 1024);

        if list_contents {
            println!("Contents:");
            list_tarball_contents(&Path::new(&tarball))?;
        }
    }

    let metadata = fs::metadata(dir_path)?;
    let size = metadata.len();
    println!("{:?} {}K", dir_path, size / 1024);

    Ok(())
}

fn find_files_with_extension(dir_path: &Path, ext: &str) -> Vec<String> {
    let mut result = vec![];

    if let Ok(entries) = fs::read_dir(dir_path) {
        for entry in entries {
            if let Ok(entry) = entry {
                let path = entry.path();
                if path.is_file() && path.extension().unwrap() == ext {
                    result.push(path.to_string_lossy().into_owned());
                }
            }
        }
    }

    result
}

fn find_files_older_than(dir_path: &Path, days: usize) -> Vec<String> {
    let now = SystemTime::now();
    let duration = std::time::Duration::from_secs((days * 24 * 60 * 60) as u64);
    let cutoff = now - duration;

    let mut result = vec![];

    if let Ok(entries) = fs::read_dir(dir_path) {
        for entry in entries {
            if let Ok(entry) = entry {
                let path = entry.path();
                if let Ok(metadata) = fs::metadata(&path) {
                    if let Ok(modified) = metadata.modified() {
                        if modified < cutoff {
                            result.push(path.to_string_lossy().into_owned());
                        }
                    }
                }
            }
        }
    }

    result
}
