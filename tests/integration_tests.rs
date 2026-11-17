use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn build_binary() {
    let build_output = Command::new("cargo")
        .args(["build"])
        .output()
        .expect("Failed to build project");

    if !build_output.status.success() {
        panic!(
            "Failed to build project: {}",
            String::from_utf8_lossy(&build_output.stderr)
        );
    }
}

fn get_binary_path() -> std::path::PathBuf {
    std::env::current_dir().unwrap().join("target/debug/rkvr")
}

fn create_config(temp_path: &Path, rmrf_dir: &Path, bkup_dir: &Path, sudo: bool) -> std::path::PathBuf {
    let config_dir = temp_path.join(".config").join("rmrf");
    fs::create_dir_all(&config_dir).unwrap();
    let config_file = config_dir.join("rmrf.cfg");
    fs::write(
        &config_file,
        format!(
            "[DEFAULT]\nrmrf_path = {}\nbkup_path = {}\nsudo = {}\nkeep = 21\nthreshold = 70\n",
            rmrf_dir.display(),
            bkup_dir.display(),
            if sudo { "yes" } else { "no" }
        ),
    )
    .unwrap();
    config_file
}

fn run_rkvr_command(args: &[&str], home_dir: &Path) -> std::process::Output {
    Command::new(get_binary_path())
        .args(args)
        .env("HOME", home_dir)
        .output()
        .expect("Failed to execute rkvr command")
}

fn assert_success(output: &std::process::Output, context: &str) {
    if !output.status.success() {
        panic!(
            "{} failed:\nStdout: {}\nStderr: {}",
            context,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn assert_no_tar_warnings(output: &std::process::Output, context: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Removing leading"),
        "{}: Should not have tar warnings about absolute paths. Stderr:\n{}",
        context,
        stderr
    );
}

fn get_archive_dirs(rmrf_dir: &Path) -> Vec<std::path::PathBuf> {
    fs::read_dir(rmrf_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.path())
        .collect()
}

fn read_metadata(archive_dir: &Path) -> String {
    let metadata_file = archive_dir.join("metadata.yml");
    assert!(
        metadata_file.exists(),
        "metadata.yml should exist in {}",
        archive_dir.display()
    );
    fs::read_to_string(&metadata_file).unwrap()
}

#[test]
fn test_rmrf_correct_cwd_for_files_outside_current_dir() {
    build_binary();

    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Create test directories and files
    let test_dir1 = temp_path.join("test_logs");
    fs::create_dir_all(&test_dir1).unwrap();

    let test_file1 = test_dir1.join("app.log");
    fs::write(&test_file1, "test log content 1").unwrap();

    // Set up temporary rmrf directory
    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();

    create_config(temp_path, &rmrf_dir, &bkup_dir, false);

    // Run rmrf on files in different directories
    let output = run_rkvr_command(&["rmrf", test_file1.to_str().unwrap()], temp_path);

    if !output.status.success() {
        panic!(
            "rmrf command failed: {}\nStderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Check that the file was archived
    assert!(!test_file1.exists(), "Original file should be removed");

    // Find the archive directory
    let rmrf_entries: Vec<_> = fs::read_dir(&rmrf_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .collect();

    assert_eq!(rmrf_entries.len(), 1, "Should have exactly one archive directory");

    let archive_dir = &rmrf_entries[0].path();
    let metadata_file = archive_dir.join("metadata.yml");

    assert!(metadata_file.exists(), "metadata.yml should exist");

    // Read and verify metadata
    let metadata_content = fs::read_to_string(&metadata_file).unwrap();

    // The CWD should be the parent directory of the archived file, not the current working directory
    assert!(
        metadata_content.contains(&format!("cwd: {}", test_dir1.display())),
        "Metadata should contain correct CWD. Actual content:\n{}",
        metadata_content
    );

    assert!(
        metadata_content.contains("- app.log"),
        "Metadata should contain the filename. Actual content:\n{}",
        metadata_content
    );
}

#[test]
fn test_rmrf_no_tar_warnings_for_absolute_paths() {
    build_binary();

    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Create test file with absolute path
    let test_dir = temp_path.join("deep").join("nested").join("path");
    fs::create_dir_all(&test_dir).unwrap();
    let test_file = test_dir.join("deep_file.log");
    fs::write(&test_file, "deep nested content").unwrap();

    // Set up rmrf directory
    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();

    create_config(temp_path, &rmrf_dir, &bkup_dir, false);

    let output = run_rkvr_command(&["rmrf", test_file.to_str().unwrap()], temp_path);

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should not contain tar warnings about removing leading '/' from member names
    assert!(
        !stderr.contains("Removing leading"),
        "Should not have tar warnings about absolute paths. Stderr:\n{}\nStdout:\n{}",
        stderr,
        stdout
    );

    if !output.status.success() {
        panic!("rmrf command should succeed. Stderr:\n{}\nStdout:\n{}", stderr, stdout);
    }
}

#[test]
fn test_multiple_files_different_directories() {
    build_binary();

    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Create files in different directories
    let dir1 = temp_path.join("app_logs");
    let dir2 = temp_path.join("system_logs");
    let dir3 = temp_path.join("debug_logs");
    fs::create_dir_all(&dir1).unwrap();
    fs::create_dir_all(&dir2).unwrap();
    fs::create_dir_all(&dir3).unwrap();

    let file1 = dir1.join("app.log");
    let file2 = dir2.join("system.log");
    let file3 = dir3.join("debug.log");
    fs::write(&file1, "app content").unwrap();
    fs::write(&file2, "system content").unwrap();
    fs::write(&file3, "debug content").unwrap();

    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();

    create_config(temp_path, &rmrf_dir, &bkup_dir, false);

    // Run rmrf on files from different directories
    let output = run_rkvr_command(
        &[
            "rmrf",
            file1.to_str().unwrap(),
            file2.to_str().unwrap(),
            file3.to_str().unwrap(),
        ],
        temp_path,
    );

    assert_success(&output, "Multiple files different directories");
    assert_no_tar_warnings(&output, "Multiple files different directories");

    // Verify all files removed
    assert!(
        !file1.exists() && !file2.exists() && !file3.exists(),
        "All original files should be removed"
    );

    // Should have multiple archive directories (one per source directory)
    let archive_dirs = get_archive_dirs(&rmrf_dir);
    assert_eq!(
        archive_dirs.len(),
        3,
        "Should have three archive directories for three different source dirs"
    );

    // Verify each archive has correct metadata
    for archive_dir in &archive_dirs {
        let metadata = read_metadata(archive_dir);
        // Each should have its own correct CWD
        assert!(
            metadata.contains(&format!("cwd: {}", dir1.display()))
                || metadata.contains(&format!("cwd: {}", dir2.display()))
                || metadata.contains(&format!("cwd: {}", dir3.display())),
            "Each metadata should contain correct CWD. Content:\n{}",
            metadata
        );
    }
}

#[test]
fn test_directory_archiving() {
    build_binary();

    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Create directory with files
    let test_dir = temp_path.join("project");
    let sub_dir = test_dir.join("src");
    fs::create_dir_all(&sub_dir).unwrap();

    let file1 = test_dir.join("README.md");
    let file2 = sub_dir.join("main.rs");
    fs::write(&file1, "readme content").unwrap();
    fs::write(&file2, "rust code").unwrap();

    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();

    create_config(temp_path, &rmrf_dir, &bkup_dir, false);

    // Run rmrf on directory
    let output = run_rkvr_command(&["rmrf", test_dir.to_str().unwrap()], temp_path);
    assert_success(&output, "Directory archiving");
    assert_no_tar_warnings(&output, "Directory archiving");

    // Verify directory was removed
    assert!(!test_dir.exists(), "Original directory should be removed");

    // Verify archive
    let archive_dirs = get_archive_dirs(&rmrf_dir);
    assert_eq!(archive_dirs.len(), 1, "Should have exactly one archive directory");

    let metadata = read_metadata(&archive_dirs[0]);
    assert!(
        metadata.contains(&format!("cwd: {}", temp_path.display())),
        "Metadata should contain parent directory as CWD. Content:\n{}",
        metadata
    );
    assert!(
        metadata.contains("- project"),
        "Metadata should contain directory name. Content:\n{}",
        metadata
    );
}

#[test]
fn test_bkup_command() {
    build_binary();

    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    let test_dir = temp_path.join("backup_test");
    fs::create_dir_all(&test_dir).unwrap();
    let test_file = test_dir.join("important.txt");
    fs::write(&test_file, "important data").unwrap();

    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();

    create_config(temp_path, &rmrf_dir, &bkup_dir, false);

    // Run bkup (should not remove original)
    let output = run_rkvr_command(&["bkup", test_file.to_str().unwrap()], temp_path);
    assert_success(&output, "Backup command");
    assert_no_tar_warnings(&output, "Backup command");

    // Verify original file still exists
    assert!(test_file.exists(), "Original file should still exist after backup");

    // Verify backup was created
    let bkup_dirs = get_archive_dirs(&bkup_dir);
    assert_eq!(bkup_dirs.len(), 1, "Should have exactly one backup directory");

    let metadata = read_metadata(&bkup_dirs[0]);
    assert!(
        metadata.contains(&format!("cwd: {}", test_dir.display())),
        "Backup metadata should contain correct CWD. Content:\n{}",
        metadata
    );
}

#[test]
fn test_recovery_functionality() {
    build_binary();

    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Create and archive a file
    let test_dir = temp_path.join("recovery_test");
    fs::create_dir_all(&test_dir).unwrap();
    let test_file = test_dir.join("recover_me.txt");
    fs::write(&test_file, "recover this content").unwrap();

    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();

    create_config(temp_path, &rmrf_dir, &bkup_dir, false);

    // Archive the file
    let rmrf_output = run_rkvr_command(&["rmrf", test_file.to_str().unwrap()], temp_path);
    assert_success(&rmrf_output, "Archive for recovery test");
    assert!(!test_file.exists(), "File should be removed after rmrf");

    // Get the archive directory
    let archive_dirs = get_archive_dirs(&rmrf_dir);
    assert_eq!(archive_dirs.len(), 1, "Should have one archive directory");
    let archive_timestamp = archive_dirs[0].file_name().unwrap().to_str().unwrap();

    // Recover the file
    let recover_output = run_rkvr_command(&["rcvr", archive_timestamp], temp_path);
    assert_success(&recover_output, "File recovery");

    // Verify file was recovered
    assert!(test_file.exists(), "File should be recovered");
    let recovered_content = fs::read_to_string(&test_file).unwrap();
    assert_eq!(
        recovered_content, "recover this content",
        "Recovered content should match original"
    );

    // Verify archive directory was cleaned up
    let remaining_dirs = get_archive_dirs(&rmrf_dir);
    assert_eq!(
        remaining_dirs.len(),
        0,
        "Archive directory should be removed after recovery"
    );
}

#[test]
fn test_rmrf_dangling_symlink() {
    build_binary();

    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    let hooks = temp_path.join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let missing = hooks.join("deleted-by-branch.sh");
    let link = hooks.join("git-no-dash-c.sh");
    std::os::unix::fs::symlink(&missing, &link).unwrap();

    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();
    create_config(temp_path, &rmrf_dir, &bkup_dir, false);

    let output = run_rkvr_command(&["rmrf", link.to_str().unwrap()], temp_path);
    assert_success(&output, "rmrf dangling symlink");

    assert!(fs::symlink_metadata(&link).is_err(), "Link should be removed");
    let archive_dirs = get_archive_dirs(&rmrf_dir);
    assert_eq!(archive_dirs.len(), 1, "Should have exactly one archive directory");
    let listing = Command::new("tar")
        .arg("-tvzf")
        .arg(archive_dirs[0].join("hooks.tar.gz"))
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(
        listing.contains(&format!("git-no-dash-c.sh -> {}", missing.display())),
        "Tarball should hold the link with its original target:\n{listing}"
    );
    assert!(read_metadata(&archive_dirs[0]).contains("- git-no-dash-c.sh"));
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn assert_not_root(test: &str) {
    assert_ne!(
        unsafe { libc::getuid() },
        0,
        "{test} depends on EACCES; root bypasses directory permissions, so it would pass vacuously"
    );
}

struct FakeSudo {
    bin_dir: std::path::PathBuf,
    log: std::path::PathBuf,
}

impl FakeSudo {
    fn new(root: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let bin_dir = root.join("fakebin");
        fs::create_dir_all(&bin_dir).unwrap();
        let script = bin_dir.join("sudo");
        fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$SUDO_LOG\"\nexit \"$SUDO_RC\"\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        FakeSudo {
            bin_dir,
            log: root.join("sudo.log"),
        }
    }

    fn run(&self, args: &[&str], home_dir: &Path, rc: i32) -> std::process::Output {
        let path = format!(
            "{}:{}",
            self.bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(get_binary_path())
            .args(args)
            .env("HOME", home_dir)
            .env("PATH", path)
            .env("SUDO_LOG", &self.log)
            .env("SUDO_RC", rc.to_string())
            .env_remove("SUDO_USER")
            .output()
            .expect("Failed to execute rkvr command")
    }

    fn lines(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
}

fn rmrf_sandbox(temp_path: &Path, sudo: bool) -> std::path::PathBuf {
    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();
    create_config(temp_path, &rmrf_dir, &bkup_dir, sudo);
    rmrf_dir
}

#[test]
fn rmrf_escalates_with_sudo_on_eacces() {
    assert_not_root("rmrf_escalates_with_sudo_on_eacces");
    build_binary();
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    rmrf_sandbox(temp_path, true);
    let fake = FakeSudo::new(temp_path);

    let locked = temp_path.join("locked");
    fs::create_dir_all(&locked).unwrap();
    let target = locked.join("a b.txt");
    fs::write(&target, "x").unwrap();
    set_mode(&locked, 0o555);

    let output = fake.run(&["rmrf", target.to_str().unwrap()], temp_path, 0);
    set_mode(&locked, 0o755);

    assert_success(&output, "rmrf escalation");
    assert_eq!(
        fake.lines(),
        vec!["rm", "-f", "--", target.to_str().unwrap()],
        "sudo argv must be exactly rm -f -- <target>"
    );
}

#[test]
fn rmrf_escalates_directory_with_rm_rf() {
    assert_not_root("rmrf_escalates_directory_with_rm_rf");
    build_binary();
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    rmrf_sandbox(temp_path, true);
    let fake = FakeSudo::new(temp_path);

    let locked = temp_path.join("locked");
    fs::create_dir_all(&locked).unwrap();
    let target = locked.join("a dir");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("f.txt"), "x").unwrap();
    set_mode(&locked, 0o555);

    let output = fake.run(&["rmrf", target.to_str().unwrap()], temp_path, 0);
    set_mode(&locked, 0o755);

    assert_success(&output, "rmrf directory escalation");
    assert_eq!(
        fake.lines(),
        vec!["rm", "-rf", "--", target.to_str().unwrap()],
        "sudo argv must be exactly rm -rf -- <target>"
    );
}

#[test]
fn rmrf_sudo_failure_reports_survivor() {
    assert_not_root("rmrf_sudo_failure_reports_survivor");
    build_binary();
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    let rmrf_dir = rmrf_sandbox(temp_path, true);
    let fake = FakeSudo::new(temp_path);

    let locked = temp_path.join("locked");
    fs::create_dir_all(&locked).unwrap();
    let first = locked.join("a.txt");
    fs::write(&first, "x").unwrap();
    let open = temp_path.join("open");
    fs::create_dir_all(&open).unwrap();
    let second = open.join("c.txt");
    fs::write(&second, "y").unwrap();
    set_mode(&locked, 0o555);

    let output = fake.run(
        &["rmrf", first.to_str().unwrap(), second.to_str().unwrap()],
        temp_path,
        1,
    );
    set_mode(&locked, 0o755);

    assert_eq!(output.status.code(), Some(1), "failing sudo must exit 1");
    assert!(first.exists(), "first target must survive a failing sudo");
    assert!(!second.exists(), "second target must still be removed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(first.to_str().unwrap()),
        "stderr must name the survivor:\n{stderr}"
    );
    let bundles = get_archive_dirs(&rmrf_dir);
    assert!(
        bundles.iter().any(|b| read_metadata(b).contains("a.txt")),
        "the survivor's bundle must remain in the bin"
    );
}

#[test]
fn rmrf_continues_past_failure() {
    assert_not_root("rmrf_continues_past_failure");
    build_binary();
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    rmrf_sandbox(temp_path, false);

    let src = temp_path.join("src");
    let src2 = temp_path.join("src2");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&src2).unwrap();
    let a = src.join("a.txt");
    let c = src2.join("c.txt");
    fs::write(&a, "a").unwrap();
    fs::write(&c, "c").unwrap();
    set_mode(&src, 0o555);

    let output = run_rkvr_command(&["rmrf", a.to_str().unwrap(), c.to_str().unwrap()], temp_path);
    set_mode(&src, 0o755);

    assert_eq!(output.status.code(), Some(1), "a failed removal must exit 1");
    assert!(!c.exists(), "src2/c.txt must be removed despite the earlier failure");
    assert!(a.exists(), "src/a.txt must survive");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(a.to_str().unwrap()),
        "stderr must name src/a.txt:\n{stderr}"
    );
}
