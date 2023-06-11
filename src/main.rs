
use eyre::{eyre, Result};
use clap::{Parser, Subcommand};
use expanduser::expanduser;
use std::time::SystemTime;
use std::process::{Command, Output};
use std::path::{Path, PathBuf};
use std::env;
use std::fs;
use serde::{Serialize, Deserialize};

use std::collections::HashMap;

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

#[derive(Debug, Serialize, Deserialize)]
struct RmrfConfig {
    path: String,
    sudo: bool,
    keep: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct BkupConfig {
    path: String,
    sudo: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct RkvrConfig {
    rmrf: RmrfConfig,
    bkup: BkupConfig,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Metadata {
    items: HashMap<String, String>,
}


impl Rkvr {

    pub fn new(rkvr_yml: &str) -> Result<Self> {
        let rkvr_path = expanduser(rkvr_yml)?;
        let rkvr_cfg_str = fs::read_to_string(rkvr_path)?;
        let rkvr_cfg: RkvrConfig = serde_yaml::from_str(&rkvr_cfg_str)?;

        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs();
        let cwd = env::current_dir()?;

        Ok(Self {
            rmrf_path: rkvr_cfg.rmrf.path,
            rmrf_keep: rkvr_cfg.rmrf.keep,
            rmrf_sudo: rkvr_cfg.rmrf.sudo,
            bkup_path: rkvr_cfg.bkup.path,
            bkup_sudo: rkvr_cfg.bkup.sudo,
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
    // note: if output as multiple lines with newlines, every line should be indented by the two spaces
    fn archive(&self, items: &[String], path: &str, sudo: bool) -> Result<Vec<PathBuf>> {
        let timestamp_path = Path::new(path).join(&self.timestamp.to_string());
        fs::create_dir_all(&timestamp_path).map_err(|e| eyre!(e))?;

        // Collect all matching items in the current working directory.
        let cwd_items: Vec<_> = items.iter().map(|item| {
            let item_path = self.cwd.join(item);
            if item_path.exists() {
                (item.clone(), Some(item_path))
            } else {
                (item.clone(), None)
            }
        }).collect();

        if cwd_items.iter().all(|(_, path)| path.is_none()) {
            return Ok(vec![]);
        }

        // Archive the items.
        let archive_path = timestamp_path.join("archive.tar.gz");
        let mut tar = Self::create_tar(&archive_path)?;
        let metadata_path = timestamp_path.join("archive.metadata");

        let mut paths_to_remove = vec![];
        let mut metadata = Metadata {
            items: HashMap::new(),
        };

        for (item, item_path) in &cwd_items {
            if let Some(path) = item_path {

                let output: Output = Self::get_output(path, sudo)?;
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

                metadata.items.insert(item.clone(), stdout);

                // Get the relative path from current directory
                let relative_path = path.strip_prefix(&self.cwd).map_err(|_| eyre!("Failed to get relative path"))?;
                tar.append_path(relative_path)?;

                paths_to_remove.push(path.clone());
            }
        }

        let yaml = serde_yaml::to_string(&metadata)?;
        fs::write(metadata_path, yaml)?;

        tar.into_inner()?.finish()?;

        Ok(paths_to_remove)
    }

    fn remove(items: Vec<PathBuf>) -> Result<()> {
        for item in items {
            if item.is_dir() {
                fs::remove_dir_all(item)?;
            } else {
                fs::remove_file(item)?;
            }
        }
        Ok(())
    }

    fn create_tar(archive_path: &PathBuf) -> Result<tar::Builder<flate2::write::GzEncoder<fs::File>>> {
        let tar_gz = fs::File::create(archive_path)?;
        let enc = flate2::write::GzEncoder::new(tar_gz, flate2::Compression::default());
        let tar = tar::Builder::new(enc);
        Ok(tar)
    }

    fn get_output(item: &Path, sudo: bool) -> Result<Output> {
        let output = if item.is_dir() {
            execute(sudo, &["tree", "-l", item.to_str().unwrap()])
        } else {
            execute(sudo, &["ls", "-l", item.to_str().unwrap()])
        };

        if !output.stderr.is_empty() {
            return Err(eyre!(String::from_utf8_lossy(&output.stderr).to_string()));
        }
        Ok(output)
    }

    // patterns: list of globa patterns (item*) to match against the metadata files
    // path: fully qualified path to archive where the timestamp directories are located
    // patterns should be combined with the cwd unless they are fully qualified paths
    // then the search should happen across all metadata files in all timestamp directories
    // matches are inclusive, so if item one matches some pattern and item two matches some other pattern, both are returned
    // returned just means that the contents of the metadata file are printed to stdout
    // note the glob patterns are left anchored, so item* will match item1, item2, item3, etc.
    fn list(patterns: &[String], path: &str) -> Result<()> {
        let archive_path = Path::new(path);
        if !archive_path.exists() {
            return Err(eyre!("Archive path does not exist"));
        }

        let mut metadata_entries: Vec<_> = fs::read_dir(archive_path)?
            .map(|res| res.map(|e| e.path()).map_err(eyre::Report::from))
            .collect::<Result<Vec<_>, eyre::Report>>()?;

        metadata_entries.sort();  // Sort entries in lexicographical order

        for entry_path in metadata_entries {
            if entry_path.is_dir() {
                let timestamp_entries = fs::read_dir(entry_path.clone()).map_err(|e| eyre!(e))?;
                for timestamp_entry in timestamp_entries {
                    let timestamp_entry = timestamp_entry.map_err(|e| eyre!(e))?;
                    let timestamp_path = timestamp_entry.path();
                    if timestamp_path.is_file() && timestamp_path.extension().map_or(false, |ext| ext == "metadata") {
                        // convert the path into a canonicalized path (fully qualified path)
                        let timestamp_path_str = fs::canonicalize(timestamp_path)?.to_string_lossy().into_owned();
                        let contents = fs::read_to_string(Path::new(&timestamp_path_str)).map_err(|e| eyre!(e))?;
                        let metadata: Metadata = serde_yaml::from_str(&contents).map_err(|e| eyre!(e))?;

                        let has_match = if patterns.is_empty() {
                            true
                        } else {
                            patterns.iter().any(|pattern| {
                                metadata.items.keys().any(|key| key.starts_with(pattern))
                            })
                        };

                        if has_match {
                            match entry_path.file_name() {
                                Some(fname) => match fname.to_str() {
                                    Some(timestamp) => {
                                        let indented_contents = contents
                                            .lines()
                                            .map(|line| format!("  {line}"))
                                            .collect::<Vec<_>>()
                                            .join("\n");
                                        println!("{timestamp}:\n{indented_contents}");
                                    },
                                    None => return Err(eyre!("Failed to convert file name to string")),
                                },
                                None => return Err(eyre!("Failed to get file name")),
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
            let Ok(entry) = entry_result else { continue };

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
                    println!("{metadata}");
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
                let paths_to_remove = self.archive(&rmrf.items, &self.rmrf_path, self.rmrf_sudo)?;
                // Remove the original files/directories after successful archiving.
                Self::remove(paths_to_remove)?;
                self.harvest(&self.rmrf_path, self.rmrf_keep)?;
            },
            Action::Bkup(ref bkup) => {
                self.archive(&bkup.items, &self.bkup_path, self.bkup_sudo)?;
            }
            Action::Ls(ref list) => {
                let mode = list.modes.as_ref().ok_or_else(|| eyre!("no mode specified"))?;
                match mode {
                    Mode::Rmrf(rmrf) => Self::list(&rmrf.items, &self.rmrf_path)?,
                    Mode::Bkup(bkup) => Self::list(&bkup.items, &self.bkup_path)?,
                }
            }
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    let rkvr_yml = env::var("RKVR_CFG")
        .unwrap_or("~/.config/rkvr/rkvr.yml".to_owned());
    let rkvr = Rkvr::new(&rkvr_yml)?;
    rkvr.run()?;
    Ok(())
}
