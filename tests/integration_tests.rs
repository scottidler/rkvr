use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn build_binary() {
    let build_output = Command::new("cargo")
        .args(&["build"])
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

fn create_config(temp_path: &Path, rmrf_dir: &Path, bkup_dir: &Path) -> std::path::PathBuf {
    let config_dir = temp_path.join(".config").join("rmrf");
    fs::create_dir_all(&config_dir).unwrap();
    let config_file = config_dir.join("rmrf.cfg");
    fs::write(
        &config_file,
        format!(
            "[DEFAULT]\nrmrf_path = {}\nbkup_path = {}\nsudo = no\nkeep = 21\nthreshold = 70\n",
            rmrf_dir.display(),
            bkup_dir.display()
        ),
    )
    .unwrap();
    config_file
}

fn run_rkvr_command(args: &[&str], home_dir: &Path) -> std::process::Output {
    Command::new(&get_binary_path())
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

    create_config(temp_path, &rmrf_dir, &bkup_dir);

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

    create_config(temp_path, &rmrf_dir, &bkup_dir);

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
fn test_multiple_files_same_directory() {
    build_binary();

    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Create multiple files in same directory
    let test_dir = temp_path.join("shared_logs");
    fs::create_dir_all(&test_dir).unwrap();
    let file1 = test_dir.join("app.log");
    let file2 = test_dir.join("error.log");
    let file3 = test_dir.join("debug.log");
    fs::write(&file1, "app log").unwrap();
    fs::write(&file2, "error log").unwrap();
    fs::write(&file3, "debug log").unwrap();

    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();

    create_config(temp_path, &rmrf_dir, &bkup_dir);

    // Run rmrf on all files
    let output = run_rkvr_command(
        &[
            "rmrf",
            file1.to_str().unwrap(),
            file2.to_str().unwrap(),
            file3.to_str().unwrap(),
        ],
        temp_path,
    );

    assert_success(&output, "Multiple files same directory");
    assert_no_tar_warnings(&output, "Multiple files same directory");

    // Verify all files removed
    assert!(
        !file1.exists() && !file2.exists() && !file3.exists(),
        "All original files should be removed"
    );

    // Verify archive
    let archive_dirs = get_archive_dirs(&rmrf_dir);
    assert_eq!(archive_dirs.len(), 1, "Should have exactly one archive directory");

    let metadata = read_metadata(&archive_dirs[0]);
    assert!(
        metadata.contains(&format!("cwd: {}", test_dir.display())),
        "Metadata should contain correct CWD. Content:\n{}",
        metadata
    );
    assert!(
        metadata.contains("- app.log") && metadata.contains("- error.log") && metadata.contains("- debug.log"),
        "Metadata should contain all filenames. Content:\n{}",
        metadata
    );
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

    create_config(temp_path, &rmrf_dir, &bkup_dir);

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

    create_config(temp_path, &rmrf_dir, &bkup_dir);

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

    create_config(temp_path, &rmrf_dir, &bkup_dir);

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

    create_config(temp_path, &rmrf_dir, &bkup_dir);

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
