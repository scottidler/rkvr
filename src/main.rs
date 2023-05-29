
use eyre::{eyre, Result};
use clap::{Parser, Subcommand};
use configparser::ini::Ini;
use expanduser::expanduser;
use std::time::SystemTime;
use std::process::{Command, Output};
use std::path::{Path, PathBuf};
use std::io::Write;
use std::env;
use std::fs;

//use walkdir::WalkDir;

fn execute<T: AsRef<str>>(sudo: bool, args: &[T]) -> Output {
    let mut args: Vec<String> = args.iter().map(|arg| arg.as_ref().to_owned()).collect();
    if sudo {
        args.insert(0, "sudo".to_owned());
    }
    let mut command = Command::new(args[0].clone());
    for arg in &args[1..] {
        command.arg(arg);
    }
    command.output().expect("failed to execute process")
}

/*
fn find_files_with_extension(dir_path: &Path, extension: &str) -> Vec<String> {
    let mut result = vec![];

    if let Ok(entries) = fs::read_dir(dir_path) {
        for entry in entries {
            if let Ok(entry) = entry {
                let path = entry.path();
                if path.is_file() && path.extension().unwrap() == extension {
                    result.push(path.to_string_lossy().into_owned());
                }
            }
        }
    }

    result
}

fn find_files_older_than(dir_path: &Path, days: i32) -> Vec<String> {
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
*/

// make a name from the basename of the path
fn make_name(path: &Path) -> Result<String> {
    match path.file_name() {
        Some(file_name) => {
            match file_name.to_str() {
                Some(file_str) => {
                    let mut name = file_str.to_owned();
                    name = name.replace("/", "-");
                    name = name.replace(":", "_");
                    name = name.replace(" ", "_");
                    Ok(name)
                },
                None => Err(eyre::eyre!("File name is not valid UTF-8")),
            }
        },
        None => Err(eyre::eyre!("Path does not have a file name")),
    }
}


/*
fn list_dir(path: &str) -> std::io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        println!("{:?} {} {}", metadata.permissions(), metadata.modified()?, entry.path().display());
    }
    Ok(())
}

fn print_dir_tree(path: &str) {
    for entry in WalkDir::new(path) {
        let entry = entry.unwrap();
        println!("{}", entry.path().display());
    }
}
*/

#[derive(Parser, Debug, Default, Clone)]
#[command(name = "rkvr", about = "tool for staging rmrf-ing or bkup-ing files")]
#[command(version = "0.1.0")]
#[command(author = "Scott A. Idler <scott.a.idler@gmail.com>")]
#[command(after_help = "after_help")]
#[command(arg_required_else_help = true)]
struct RkvrCli {

    #[command(subcommand)]
    action: Option<Action>,
}

#[derive(Subcommand, Debug, Clone)]
enum Action {
    Rmrf(ArchiveCli),
    Bkup(ArchiveCli),
    Ls(ListCli),
}

#[derive(Parser, Debug, Default, Clone)]
struct ArchiveCli {

    #[arg()]
    items: Vec<String>,
}

#[derive(Parser, Debug, Default, Clone)]
struct ListCli {
    #[command(subcommand)]
    modes: Option<Mode>,
}

#[derive(Subcommand, Debug, Clone)]
enum Mode {
    Rmrf(ArchiveCli),
    Bkup(ArchiveCli),
}
struct Rkvr {
    rmrf_path: String,
    rmrf_keep: usize,
    rmrf_sudo: bool,
    bkup_path: String,
    bkup_sudo: bool,
    timestamp: u64,
    cwd: PathBuf,
}

impl Rkvr {
    pub fn new(rkvr_cfg: &str) -> Result<Self> {
        /*
        ❯ bat -p ~/.config/rkvr/rkvr.cfg
        [rmrf]
        path=/var/tmp/rmrf/

        sudo=true

        keep=21

        [bkup]
        path=/var/tmp/bkup/

        sudo=true
        */
        // use the above config file to set the defaults
        // expanduser() is needed because configparser::ini::Ini::load() does not expand ~
        let rkvr_path = expanduser(rkvr_cfg)?;
        let mut rkvr_cfg = Ini::new();
        rkvr_cfg.load(&rkvr_path).map_err(|e| eyre!(e))?;
        let rmrf_path = rkvr_cfg.get("rmrf", "path").unwrap_or("/var/tmp/rmrf".to_owned());
        let rmrf_sudo: bool = rkvr_cfg.get("rmrf", "sudo").unwrap_or("true".to_owned()) == "true";
        let rmrf_keep: usize = rkvr_cfg.get("rmrf", "keep").unwrap_or("21".to_owned()).parse().unwrap();
        let bkup_path = rkvr_cfg.get("bkup", "path").unwrap_or("/var/tmp/bkup".to_owned());
        let bkup_sudo: bool = rkvr_cfg.get("bkup", "sudo").unwrap_or("true".to_owned()) == "true";
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs();
        let cwd = env::current_dir()?;
        Ok(Self {
            rmrf_path,
            rmrf_keep,
            rmrf_sudo,
            bkup_path,
            bkup_sudo,
            timestamp,
            cwd,
        })
    }

    // items: list of files or directories to archive; if not found in the cwd, noop
    // path: fully qualified path to archive where the timestamp directory will be created
    // sudo: whether to run the archive command with sudo
    // each timestamp directory will have two files placed in it:
    // - the .tar.gz archive of the items that were matched
    // - metadata file with the output of the ls command for items that are files, tree for items that are directories
    // the metadata file will have the fully qualified path of the item, then a colon:
    // then the output of the ls or tree command on a new line, indented by two spaces
    fn archive(&self, items: &[String], path: &str, sudo: bool) -> Result<()> {
        let timestamp_path = format!("{}/{}", path, self.timestamp);
        let timestamp_path = Path::new(&timestamp_path);
        fs::create_dir_all(&timestamp_path).map_err(|e| eyre!(e))?;

        // Collect all matching items in the current working directory.
        let cwd_items: Vec<_> = items.iter().filter_map(|item| {
            let item_path = self.cwd.join(item);
            if item_path.exists() {
                Some(item_path)
            } else {
                None
            }
        }).collect();

        if cwd_items.is_empty() {
            return Ok(());
        }

        // Archive the items.
        let archive_path = timestamp_path.join("archive.tar.gz");
        let tar_gz = fs::File::create(&archive_path)?;
        let enc = flate2::write::GzEncoder::new(tar_gz, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        for item in &cwd_items {
            tar.append_path(item)?;
        }
        tar.into_inner()?.finish()?;

        // Create metadata files.
        for item in cwd_items {
            let metadata_path = timestamp_path.join(format!("{}.metadata", make_name(&item)?));
            let mut metadata_file = fs::File::create(metadata_path)?;

            let output = if item.is_dir() {
                execute(sudo, &["tree", "-l", item.to_str().unwrap()])
            } else {
                execute(sudo, &["ls", "-l", item.to_str().unwrap()])
            };

            if !output.stderr.is_empty() {
                return Err(eyre!(String::from_utf8_lossy(&output.stderr).to_string()));
            }

            write!(metadata_file, "{}:\n  {}\n", item.to_string_lossy(), String::from_utf8_lossy(&output.stdout))?;
        }

        Ok(())
    }

    // patterns: list of globa patterns (item*) to match against the metadata files
    // path: fully qualified path to archive where the timestamp directories are located
    // patterns should be combined with the cwd unless they are fully qualified paths
    // then the search should happen across all metadata files in all timestamp directories
    // matches are inclusive, so if item one matches some pattern and item two matches some other pattern, both are returned
    // returned just means that the contents of the metadata file are printed to stdout
    // note the glob patterns are left anchored, so item* will match item1, item2, item3, etc.
    fn list(&self, patterns: &[String], path: &str) -> Result<()> {
        let archive_path = Path::new(path);
        if !archive_path.exists() {
            return Err(eyre!("Archive path does not exist"));
        }

        // Create a vector to hold all the glob::Pattern structs.
        let mut glob_patterns = Vec::new();
        for pattern in patterns {
            let pattern_str = if pattern.starts_with("/") {
                pattern.clone()
            } else {
                self.cwd.join(pattern).to_string_lossy().to_string()
            };

            // convert the pattern into a canonicalized pattern (fully qualified path)
            let pattern_str = fs::canonicalize(pattern_str)?.to_string_lossy().into_owned();
            let glob_pattern = glob::Pattern::new(&pattern_str)?;
            glob_patterns.push(glob_pattern);
        }

        let metadata_entries = fs::read_dir(archive_path)?;
        for entry in metadata_entries {
            if let Ok(entry) = entry {
                let entry_path = entry.path();
                if entry_path.is_dir() {
                    let timestamp_entries = fs::read_dir(entry_path)?;
                    for timestamp_entry in timestamp_entries {
                        if let Ok(timestamp_entry) = timestamp_entry {
                            let timestamp_path = timestamp_entry.path();
                            if timestamp_path.is_file() && timestamp_path.extension().unwrap() == "metadata" {
                                // convert the path into a canonicalized path (fully qualified path)
                                let timestamp_path_str = fs::canonicalize(timestamp_path)?.to_string_lossy().into_owned();
                                for pattern in &glob_patterns {
                                    if pattern.matches_path(Path::new(&timestamp_path_str)) {
                                        let contents = fs::read_to_string(Path::new(&timestamp_path_str))?;
                                        println!("{}", contents);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    // path: fully qualified path to archive where the timestamp directory will be created
    // keep: number of days to keep archives (timestamp dirs in the path directory)
    // every timestamp directory that will be deleted will have the metadata file printed to stdout
    // the the entire timestamp directory will be deleted
    fn harvest(&self, path: &str, keep: usize) -> Result<()> {
        // Convert the keep days into a Duration.
        let keep_duration = std::time::Duration::from_secs((keep * 24 * 60 * 60) as u64);

        // Get the current time.
        let now = SystemTime::now();

        // Read the contents of the directory.
        let read_dir = fs::read_dir(path)?;

        for entry_result in read_dir {
            // Unwrap the entry. If this fails, skip to the next entry.
            let entry = match entry_result {
                Ok(entry) => entry,
                Err(_) => continue,
            };

            // Check that the entry is a directory.
            let metadata = entry.metadata()?;
            if !metadata.is_dir() {
                continue;
            }

            // Parse the directory name into a timestamp.
            let dir_name = entry.file_name();
            let timestamp_str = dir_name.to_str().ok_or_else(|| eyre!("Invalid directory name"))?;
            let timestamp: u64 = timestamp_str.parse().map_err(|_| eyre!("Failed to parse directory name into a timestamp"))?;

            // Calculate the age of the directory.
            let dir_age = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(timestamp);
            let dir_age_duration = now.duration_since(dir_age)?;

            // If the directory is older than the keep duration, delete it.
            if dir_age_duration > keep_duration {
                let metadata_file_path = entry.path().join("archive.metadata");

                // If the metadata file exists, print it to stdout.
                if metadata_file_path.exists() {
                    let metadata = fs::read_to_string(&metadata_file_path)?;
                    println!("{}", metadata);
                }

                // Delete the directory.
                fs::remove_dir_all(entry.path())?;
            }
        }

        Ok(())
    }


    fn run(&self) -> Result<()> {
        let cli = RkvrCli::parse();

        // get action, or convert option to error
        let action = cli.action.ok_or_else(|| eyre!("no action specified"))?;
        match action {
            Action::Rmrf(ref rmrf) => {
                self.archive(&rmrf.items, &self.rmrf_path, self.rmrf_sudo)?;
                self.harvest(&self.rmrf_path, self.rmrf_keep)?;
            },
            Action::Bkup(ref bkup) => self.archive(&bkup.items, &self.bkup_path, self.bkup_sudo)?,
            Action::Ls(ref list) => {
                let mode = list.modes.as_ref().ok_or_else(|| eyre!("no mode specified"))?;
                match mode {
                    Mode::Rmrf(rmrf) => self.list(&rmrf.items, &self.rmrf_path)?,
                    Mode::Bkup(bkup) => self.list(&bkup.items, &self.bkup_path)?,
                }
            }
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    let rkvr_cfg = env::var("RKVR_CFG")
        .unwrap_or("~/.config/rkvr/rkvr.cfg".to_owned());
    let rkvr = Rkvr::new(&rkvr_cfg)?;
    rkvr.run()?;
    Ok(())
}
