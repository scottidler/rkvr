#![allow(dead_code)]

use clap::{Parser, Subcommand};
use dirs;
use std::fs;
use std::fs::File;
use std::path::{Path,PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;
use configparser::ini::Ini;
use std::io::prelude::*;
use std::fs::OpenOptions;

use std::os::unix::fs::PermissionsExt;
use libc::S_IRUSR;
use libc::S_IWUSR;
use libc::S_IXUSR;
use libc::S_IRGRP;
use libc::S_IWGRP;
use libc::S_IXGRP;
use libc::S_IROTH;
use libc::S_IWOTH;
use libc::S_IXOTH;

#[derive(Parser)]
#[command(
    author = "Scott A. Idler",
    version = "0.1",
    about = "tool for staging rmrf-ing or bkup-ing files",
    long_about = None)]
struct Cli {
    /*
    /// Optional name to operate on
    name: Option<String>,

    /// Sets a custom config file:w
    ///
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Turn debugging information on
    #[arg(short, long, action = clap::ArgAction::Count)]
    debug: u8,
    */
    #[arg(name = "targets")]
    targets: Vec<String>,

    #[command(subcommand)]
    action: Option<Action>,
}

#[derive(Subcommand, Debug, Default)]
enum Action {
    Bkup,
    #[default]
    #[command(about = "[default] rmrf files")]
    Rmrf,
    LsBkup,
    LsRmrf,
    BkupRmrf,
}
// make a name from the basename of the path
fn make_name(path: &Path) -> String {
    let mut name = path.file_name().unwrap().to_str().unwrap().to_owned();
    name = name.replace("/", "-");
    name = name.replace(":", "_");
    name = name.replace(" ", "_");
    name
}

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

fn main() {
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let matches = Cli::parse();
    let action: Action = matches.action.unwrap_or_default();
    let targets: Vec<PathBuf> = matches
        .targets
        .iter()
        .map(|f| fs::canonicalize(f).unwrap().to_string_lossy().into_owned())
        .map(|f| PathBuf::from(f))
        .collect();

    /*
    ❯ bat -p .config/rmrf/rmrf2.cfg

    #where to store tarballs of removed files
    RMRF_PATH=/var/tmp/rmrf2

    #use sudo by default
    SUDO=yes

    #how many days to keep files in tarball at /var/tmp/rmrf
    KEEPALIVE=21

     */
    let rmrf_cfg_path = dirs::home_dir().unwrap().join(".config/rmrf/rmrf2.cfg");
    let mut rmrf_cfg = Ini::new();
    rmrf_cfg.load(&rmrf_cfg_path).unwrap();
    let path = rmrf_cfg.get("DEFAULT", "path").unwrap_or("/var/tmp/rmrf".to_owned());
    let sudo: bool = rmrf_cfg.get("DEFAULT", "sudo").unwrap_or("yes".to_owned()) == "yes";
    let keep: i32 = rmrf_cfg.get("DEFAULT", "keep").unwrap_or("21".to_owned()).parse().unwrap();

    let rmrf_path = Path::new(&path);
    let bkup_path = Path::new(&rmrf_path).join("bkup");

    fs::create_dir_all(&rmrf_path).unwrap();
    fs::create_dir_all(&bkup_path).unwrap();

    for target in targets.into_iter() {
        match action {
            Action::Bkup => archive(&bkup_path, timestamp.to_string(), &target, sudo, None),
            Action::Rmrf => archive(&rmrf_path, timestamp.to_string(), &target, sudo, Some(keep)),
            Action::LsBkup => println!("ls bkup"),
            Action::LsRmrf => println!("ls rmrf"),
            Action::BkupRmrf => println!("bkup rmrf"),
        }
    }
}

fn archive(path: &Path, timestamp: String, target: &Path, sudo: bool, keep: Option<i32>) {
    let name = make_name(target);
    println!("name={}", name);
    let base = path.join(&timestamp);
    println!("base={}", base.to_string_lossy());
    execute(false, &vec!["mkdir", "-p", base.to_str().unwrap()]);

    if target == &base {
        println!("{} ->", path.to_string_lossy());
        let output = execute(sudo, &vec![
            "tar",
            "--absolute-names",
            "-xvf",
            target.to_str().unwrap(),
        ]);
        print!("  {}", String::from_utf8_lossy(&output.stdout));
        return;
    }
    let tarball = format!("{}.tar.gz", name);
    let metadata = format!("{}.meta", name);

    let output = if path.join(target).metadata().unwrap().is_dir() {
        execute(sudo, &vec![
            "tree",
            "-a",
            "-h",
            "-L",
            "2",
            path.join(target).to_str().unwrap(),
        ])
    } else {
        execute(sudo, &vec![
            "ls",
            "-alh",
            path.join(target).to_str().unwrap(),
        ])
    };

    let metadata_path = base.join(&metadata);
    println!("metadata_path={}", metadata_path.to_string_lossy());
    File::create(&metadata_path).expect("create file failed");

    let mut file_permissions = fs::metadata(&metadata_path)
        .unwrap()
        .permissions();
    file_permissions.set_mode(S_IRUSR | S_IWUSR | S_IXUSR | S_IRGRP | S_IWGRP | S_IXGRP | S_IROTH | S_IWOTH | S_IXOTH);
    fs::set_permissions(&metadata_path, file_permissions).unwrap();

    let mut metadata_file = OpenOptions::new()
        .write(true)
        .open(metadata_path)
        .unwrap();
    metadata_file.write_all(&output.stdout).unwrap();

    let output = execute(sudo, &vec![
        "tar",
        "--absolute-names",
        "--preserve-permissions",
        "-cvzf",
        &tarball,
        path.join(target).to_str().unwrap(),
    ]);
    print!("  {}", String::from_utf8_lossy(&output.stdout));

    let new_tarball_path = base.join(&tarball);
    fs::rename(&tarball, &new_tarball_path).unwrap();
    println!("-> {}", new_tarball_path.to_string_lossy());

    if let Some(keep_days) = keep {
        let deleted = execute(sudo, &vec![
            "find",
            base.to_string_lossy().as_ref(),
            "-mtime",
            &format!("+{}", keep_days),
            "-type",
            "d",
            "-print",
        ]);
        let deleted_dirs = String::from_utf8_lossy(&deleted.stdout);
        if !deleted_dirs.trim().is_empty() {
            println!("{}", deleted_dirs);
            println!("-> /dev/null");
        }
        execute(sudo, &vec![
            "find",
            base.to_string_lossy().as_ref(),
            "-mtime",
            &format!("+{}", keep_days),
            "-type",
            "d",
            "-delete",
        ]);
        execute(sudo, &vec![
            "rm",
            "-rf",
            target.to_str().unwrap(),
        ]);
    }

    match keep {
        Some(days) => {

        },
        None => {
            execute(sudo, &vec![
                "rm",
                "-rf",
                target.to_str().unwrap(),
            ]);
        },
    }
}

// fn acrhive(path: &Path, timestamp: String, target: &String, sudo: bool, keep: Option<i32>) {
//     let name = make_name(&target);
//     println!("name={}", name);
//     // create a path based upon path / timestamp
//     // name.tar.gz will be the archive name located at that ^^^ path
//     // mame.meta will be a file containing either the ls of the file or tree of a directory
//     let base = path.join(timestamp);
//     if target == base.to_string_lossy().into_owned() {
//         println!("{} ->", path);
//         let output = execute(sudo, &vec![
//             "tar",
//             "--absolute-names",
//             "-xvf",
//             &target]);
//         print!("  {}", String::from_utf8_lossy(&output.stdout));
//         return;
//     }
//     let tarball = format!("{name}.tar.gz");
//     let metadata = format!("{name}.meta");

//     let output = if fs::metadata(&path).unwrap().is_dir() {
//         execute(sudo, &vec![
//             "tree",
//             "-a",
//             "-h",
//             "-L",
//             "2",
//             &path])
//     } else {
//         execute(sudo, &vec![
//             "ls",
//             "-alh",
//             &path])
//     };
//     let mut metadata_file = File::create(path.join(metadata)).unwrap();
//     metadata_file.write_all(&output.stdout).unwrap();

//     let output = execute(sudo, &vec![
//         "tar",
//         "--absolute-names",
//         "--preserve-permissions",
//         "-cvzf",
//         &tarball,
//         &path]);
//     print!("  {}", String::from_utf8_lossy(&output.stdout));

//     // move the tarball and metadata file to the base path
//     if let Some(days) = keep {
//         // print days that will be deleted
//         let output = execute(sudo, &vec!["find", base.to_string_lossy().as_ref(), "-mtime", &days.to_string(), "-type", "d", "-print"]);
//         print!("  {}", String::from_utf8_lossy(&output.stdout));
//     }
// }


// fn archive2(dir_path: &Path, sudo: bool, files: &[String], tarball: &str, keep: Option<i32>) {
//     if files[0].starts_with(dir_path.to_string_lossy().as_ref()) {
//         println!("{} ->", files[0]);
//         // map archive to vec of strings, chain with files
//         let args: Vec<String> = EXTRACT
//             .iter()
//             .map(|s| s.to_string())
//             .chain(files.iter().cloned())
//             .collect::<Vec<String>>();
//         let output = execute(sudo, &args);
//         let output = execute(sudo, vec!["tar", "--"])
//         print!("  {}", String::from_utf8_lossy(&output.stdout));
//         return;
//     }

//     let tarball_path = dir_path.join(tarball);

//     // map archive to vec of strings, chain the tarball and the files
//     let args: Vec<String> = ARCHIVE
//         .iter()
//         .map(|s| s.to_string())
//         .chain(vec![tarball_path.to_string_lossy().into_owned()])
//         .chain(files.iter().cloned())
//         .collect::<Vec<String>>();
//     let output = execute(sudo, &args);
//     println!("{}", String::from_utf8_lossy(&output.stdout));

//     println!("-> {}", tarball_path.to_string_lossy());

//     if let Some(keep) = keep {
//         let deleted = find_files_older_than(dir_path, keep);
//         if !deleted.is_empty() {
//             for path in &deleted {
//                 println!("{}", path);
//             }
//             println!("-> /dev/null");
//         }

//         for path in deleted {
//             fs::remove_file(path).unwrap();
//         }

//         for file in files {
//             fs::remove_file(file).unwrap();
//         }
//     }
// }

fn list(dir_path: &Path) {
    let tarballs = find_files_with_extension(dir_path, "tar.gz");
    for tarball in tarballs {
        let metadata = fs::metadata(&tarball).unwrap();
        let size = metadata.len();
        println!("{:?} {}K", tarball, size / 1024);

        let output = Command::new("sudo")
            .arg("tar")
            .arg("--absolute-names")
            .arg("-tvf")
            .arg(&tarball)
            .output()
            .unwrap();
        print!("  {}", String::from_utf8_lossy(&output.stdout));
    }

    let metadata = fs::metadata(dir_path).unwrap();
    let size = metadata.len();
    println!("{:?} {}K", dir_path, size / 1024);
}

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
