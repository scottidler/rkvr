use std::fs;
use std::process::Command;
use tempfile::TempDir;
use std::path::Path;

fn build_binary() {
    let build_output = Command::new("cargo")
        .args(&["build"])
        .output()
        .expect("Failed to build project");
    
    if !build_output.status.success() {
        panic!("Failed to build project: {}", String::from_utf8_lossy(&build_output.stderr));
    }
}

fn get_binary_path() -> std::path::PathBuf {
    std::env::current_dir().unwrap().join("target/debug/rkvr")
}

fn create_config(temp_path: &Path, rmrf_dir: &Path, bkup_dir: &Path) -> std::path::PathBuf {
    let config_dir = temp_path.join(".config").join("rmrf");
    fs::create_dir_all(&config_dir).unwrap();
    let config_file = config_dir.join("rmrf.cfg");
    fs::write(&config_file, format!(
        "[DEFAULT]\nrmrf_path = {}\nbkup_path = {}\nsudo = no\nkeep = 21\nthreshold = 70\n", 
        rmrf_dir.display(), 
        bkup_dir.display()
    )).unwrap();
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
        panic!("{} failed:\nStdout: {}\nStderr: {}", 
               context,
               String::from_utf8_lossy(&output.stdout),
               String::from_utf8_lossy(&output.stderr));
    }
}

fn assert_no_tar_warnings(output: &std::process::Output, context: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("Removing leading"), 
            "{}: Should not have tar warnings about absolute paths. Stderr:\n{}", context, stderr);
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
    assert!(metadata_file.exists(), "metadata.yml should exist in {}", archive_dir.display());
    fs::read_to_string(&metadata_file).unwrap()
}

#[test]
fn test_mixed_files_and_directories() {
    build_binary();
    
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    
    // Create mixed content
    let file_dir = temp_path.join("files");
    let project_dir = temp_path.join("project");
    fs::create_dir_all(&file_dir).unwrap();
    fs::create_dir_all(&project_dir.join("src")).unwrap();
    
    let single_file = file_dir.join("config.txt");
    let project_file = project_dir.join("Cargo.toml");
    fs::write(&single_file, "config data").unwrap();
    fs::write(&project_file, "cargo data").unwrap();
    
    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();
    
    create_config(temp_path, &rmrf_dir, &bkup_dir);
    
    // Run rmrf on mix of file and directory
    let output = run_rkvr_command(&[
        "rmrf",
        single_file.to_str().unwrap(),
        project_dir.to_str().unwrap()
    ], temp_path);
    
    assert_success(&output, "Mixed files and directories");
    assert_no_tar_warnings(&output, "Mixed files and directories");
    
    // Verify both removed
    assert!(!single_file.exists() && !project_dir.exists(), 
            "Both file and directory should be removed");
    
    // Should have two archive directories
    let archive_dirs = get_archive_dirs(&rmrf_dir);
    assert_eq!(archive_dirs.len(), 2, "Should have two archive directories");
}

#[test]
fn test_bkup_rmrf_command() {
    build_binary();
    
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    
    let test_dir = temp_path.join("bkup_rmrf_test");
    fs::create_dir_all(&test_dir).unwrap();
    let test_file = test_dir.join("data.txt");
    fs::write(&test_file, "data content").unwrap();
    
    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();
    
    create_config(temp_path, &rmrf_dir, &bkup_dir);
    
    // Run bkup-rmrf (should backup and remove)
    let output = run_rkvr_command(&["bkup-rmrf", test_file.to_str().unwrap()], temp_path);
    assert_success(&output, "Backup-rmrf command");
    assert_no_tar_warnings(&output, "Backup-rmrf command");
    
    // Verify original file was removed
    assert!(!test_file.exists(), "Original file should be removed after bkup-rmrf");
    
    // Verify backup was created
    let bkup_dirs = get_archive_dirs(&bkup_dir);
    assert_eq!(bkup_dirs.len(), 1, "Should have exactly one backup directory");
    
    let metadata = read_metadata(&bkup_dirs[0]);
    assert!(metadata.contains(&format!("cwd: {}", test_dir.display())), 
            "Bkup-rmrf metadata should contain correct CWD. Content:\n{}", metadata);
}

#[test]
fn test_list_rmrf_functionality() {
    build_binary();
    
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    
    // Create and archive multiple files
    let test_dir1 = temp_path.join("list_test1");
    let test_dir2 = temp_path.join("list_test2");
    fs::create_dir_all(&test_dir1).unwrap();
    fs::create_dir_all(&test_dir2).unwrap();
    
    let file1 = test_dir1.join("file1.txt");
    let file2 = test_dir2.join("file2.txt");
    fs::write(&file1, "content1").unwrap();
    fs::write(&file2, "content2").unwrap();
    
    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();
    
    create_config(temp_path, &rmrf_dir, &bkup_dir);
    
    // Archive files
    let output1 = run_rkvr_command(&["rmrf", file1.to_str().unwrap()], temp_path);
    assert_success(&output1, "First rmrf command");
    
    // Check first archive was created
    let archive_dirs_after_first = get_archive_dirs(&rmrf_dir);
    println!("Archive directories after first command: {:?}", archive_dirs_after_first);
    assert_eq!(archive_dirs_after_first.len(), 1, "Should have one archive directory after first command");
    
    std::thread::sleep(std::time::Duration::from_millis(100)); // Ensure different timestamps
    
    let output2 = run_rkvr_command(&["rmrf", file2.to_str().unwrap()], temp_path);
    assert_success(&output2, "Second rmrf command");
    
    // Check both archives exist
    let archive_dirs_after_second = get_archive_dirs(&rmrf_dir);
    println!("Archive directories after second command: {:?}", archive_dirs_after_second);
    assert_eq!(archive_dirs_after_second.len(), 2, "Should have two archive directories after second command");
    
    // List archived files
    let list_output = run_rkvr_command(&["ls-rmrf"], temp_path);
    assert_success(&list_output, "List rmrf files");
    
    let output_str = String::from_utf8_lossy(&list_output.stdout);
    println!("Actual ls-rmrf output:\n{}", output_str);
    assert!(output_str.contains(&format!("cwd: {}", test_dir1.display())), 
            "List output should contain first directory CWD");
    assert!(output_str.contains(&format!("cwd: {}", test_dir2.display())), 
            "List output should contain second directory CWD");
    assert!(output_str.contains("- file1.txt"), 
            "List output should contain first filename");
    assert!(output_str.contains("- file2.txt"), 
            "List output should contain second filename");
}

#[test]
fn test_list_bkup_functionality() {
    build_binary();
    
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    
    let test_dir = temp_path.join("bkup_list_test");
    fs::create_dir_all(&test_dir).unwrap();
    let test_file = test_dir.join("backup_file.txt");
    fs::write(&test_file, "backup content").unwrap();
    
    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();
    
    create_config(temp_path, &rmrf_dir, &bkup_dir);
    
    // Create backup
    run_rkvr_command(&["bkup", test_file.to_str().unwrap()], temp_path);
    
    // List backup files
    let list_output = run_rkvr_command(&["ls-bkup"], temp_path);
    assert_success(&list_output, "List backup files");
    
    let output_str = String::from_utf8_lossy(&list_output.stdout);
    assert!(output_str.contains(&format!("cwd: {}", test_dir.display())), 
            "List backup output should contain correct CWD");
    assert!(output_str.contains("- backup_file.txt"), 
            "List backup output should contain filename");
}

#[test]
fn test_edge_case_deeply_nested_paths() {
    build_binary();
    
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    
    // Create deeply nested file
    let deep_dir = temp_path.join("very").join("deeply").join("nested").join("directory").join("structure");
    fs::create_dir_all(&deep_dir).unwrap();
    let deep_file = deep_dir.join("deep_file.log");
    fs::write(&deep_file, "deep content").unwrap();
    
    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();
    
    create_config(temp_path, &rmrf_dir, &bkup_dir);
    
    // Archive deeply nested file
    let output = run_rkvr_command(&["rmrf", deep_file.to_str().unwrap()], temp_path);
    assert_success(&output, "Deeply nested file");
    assert_no_tar_warnings(&output, "Deeply nested file");
    
    // Verify correct CWD in metadata
    let archive_dirs = get_archive_dirs(&rmrf_dir);
    assert_eq!(archive_dirs.len(), 1, "Should have one archive directory");
    
    let metadata = read_metadata(&archive_dirs[0]);
    assert!(metadata.contains(&format!("cwd: {}", deep_dir.display())), 
            "Metadata should contain deeply nested CWD. Content:\n{}", metadata);
}

#[test]
fn test_edge_case_files_with_special_characters() {
    build_binary();
    
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    
    // Create files with special characters
    let test_dir = temp_path.join("special_chars");
    fs::create_dir_all(&test_dir).unwrap();
    
    let file1 = test_dir.join("file with spaces.txt");
    let file2 = test_dir.join("file-with-dashes.log");
    let file3 = test_dir.join("file_with_underscores.dat");
    fs::write(&file1, "spaces content").unwrap();
    fs::write(&file2, "dashes content").unwrap();
    fs::write(&file3, "underscores content").unwrap();
    
    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();
    
    create_config(temp_path, &rmrf_dir, &bkup_dir);
    
    // Archive files with special characters
    let output = run_rkvr_command(&[
        "rmrf",
        file1.to_str().unwrap(),
        file2.to_str().unwrap(),
        file3.to_str().unwrap()
    ], temp_path);
    
    assert_success(&output, "Files with special characters");
    assert_no_tar_warnings(&output, "Files with special characters");
    
    // Verify all files were archived
    assert!(!file1.exists() && !file2.exists() && !file3.exists(), 
            "All files with special characters should be removed");
    
    let archive_dirs = get_archive_dirs(&rmrf_dir);
    assert_eq!(archive_dirs.len(), 1, "Should have one archive directory");
    
    let metadata = read_metadata(&archive_dirs[0]);
    assert!(metadata.contains("file with spaces.txt") && 
            metadata.contains("file-with-dashes.log") && 
            metadata.contains("file_with_underscores.dat"), 
            "Metadata should contain all filenames with special characters. Content:\n{}", metadata);
}

#[test]
fn test_edge_case_empty_files() {
    build_binary();
    
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    
    let test_dir = temp_path.join("empty_files");
    fs::create_dir_all(&test_dir).unwrap();
    
    let empty_file = test_dir.join("empty.txt");
    fs::write(&empty_file, "").unwrap(); // Empty file
    
    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();
    
    create_config(temp_path, &rmrf_dir, &bkup_dir);
    
    let output = run_rkvr_command(&["rmrf", empty_file.to_str().unwrap()], temp_path);
    assert_success(&output, "Empty file archiving");
    assert_no_tar_warnings(&output, "Empty file archiving");
    
    assert!(!empty_file.exists(), "Empty file should be removed");
    
    let archive_dirs = get_archive_dirs(&rmrf_dir);
    assert_eq!(archive_dirs.len(), 1, "Should have one archive directory");
    
    let metadata = read_metadata(&archive_dirs[0]);
    assert!(metadata.contains(&format!("cwd: {}", test_dir.display())), 
            "Metadata should contain correct CWD for empty file");
}

#[test]
fn test_edge_case_very_large_file() {
    build_binary();
    
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    
    let test_dir = temp_path.join("large_files");
    fs::create_dir_all(&test_dir).unwrap();
    
    let large_file = test_dir.join("large.txt");
    // Create a 1MB file
    let large_content = "x".repeat(1024 * 1024);
    fs::write(&large_file, large_content).unwrap();
    
    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();
    
    create_config(temp_path, &rmrf_dir, &bkup_dir);
    
    let output = run_rkvr_command(&["rmrf", large_file.to_str().unwrap()], temp_path);
    assert_success(&output, "Large file archiving");
    assert_no_tar_warnings(&output, "Large file archiving");
    
    assert!(!large_file.exists(), "Large file should be removed");
    
    let archive_dirs = get_archive_dirs(&rmrf_dir);
    assert_eq!(archive_dirs.len(), 1, "Should have one archive directory");
    
    let metadata = read_metadata(&archive_dirs[0]);
    assert!(metadata.contains(&format!("cwd: {}", test_dir.display())), 
            "Metadata should contain correct CWD for large file");
}

#[test]
fn test_default_rmrf_behavior() {
    build_binary();
    
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    
    let test_dir = temp_path.join("default_test");
    fs::create_dir_all(&test_dir).unwrap();
    let test_file = test_dir.join("default.txt");
    fs::write(&test_file, "default behavior test").unwrap();
    
    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();
    
    create_config(temp_path, &rmrf_dir, &bkup_dir);
    
    // Test default behavior (no subcommand, should default to rmrf)
    let output = run_rkvr_command(&[test_file.to_str().unwrap()], temp_path);
    assert_success(&output, "Default rmrf behavior");
    assert_no_tar_warnings(&output, "Default rmrf behavior");
    
    // File should be removed (default is rmrf)
    assert!(!test_file.exists(), "File should be removed with default behavior");
    
    let archive_dirs = get_archive_dirs(&rmrf_dir);
    assert_eq!(archive_dirs.len(), 1, "Should have one archive directory");
    
    let metadata = read_metadata(&archive_dirs[0]);
    assert!(metadata.contains(&format!("cwd: {}", test_dir.display())), 
            "Default behavior metadata should contain correct CWD");
}

#[test]
fn test_symlink_handling() {
    build_binary();
    
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    
    let test_dir = temp_path.join("symlink_test");
    fs::create_dir_all(&test_dir).unwrap();
    
    let original_file = test_dir.join("original.txt");
    let symlink_file = test_dir.join("symlink.txt");
    fs::write(&original_file, "original content").unwrap();
    
    // Create symlink
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&original_file, &symlink_file).unwrap();
    }
    #[cfg(not(unix))]
    {
        // Skip symlink test on non-Unix systems
        return;
    }
    
    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();
    
    create_config(temp_path, &rmrf_dir, &bkup_dir);
    
    // Archive the symlink
    let output = run_rkvr_command(&["rmrf", symlink_file.to_str().unwrap()], temp_path);
    assert_success(&output, "Symlink archiving");
    assert_no_tar_warnings(&output, "Symlink archiving");
    
    // Symlink should be removed, but original should remain
    assert!(!symlink_file.exists(), "Symlink should be removed");
    assert!(original_file.exists(), "Original file should remain");
    
    let archive_dirs = get_archive_dirs(&rmrf_dir);
    assert_eq!(archive_dirs.len(), 1, "Should have one archive directory");
}

#[test]
fn test_concurrent_operations() {
    build_binary();
    
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    
    // Create multiple test files
    let test_dir = temp_path.join("concurrent_test");
    fs::create_dir_all(&test_dir).unwrap();
    
    let mut files = Vec::new();
    for i in 0..5 {
        let file = test_dir.join(format!("file_{}.txt", i));
        fs::write(&file, format!("content {}", i)).unwrap();
        files.push(file);
    }
    
    let rmrf_dir = temp_path.join("rmrf");
    let bkup_dir = temp_path.join("bkup");
    fs::create_dir_all(&rmrf_dir).unwrap();
    fs::create_dir_all(&bkup_dir).unwrap();
    
    create_config(temp_path, &rmrf_dir, &bkup_dir);
    
    // Archive all files in one command
    let mut args = vec!["rmrf"];
    let file_paths: Vec<String> = files.iter().map(|f| f.to_str().unwrap().to_string()).collect();
    for file_path in &file_paths {
        args.push(file_path);
    }
    
    let output = run_rkvr_command(&args, temp_path);
    assert_success(&output, "Concurrent file operations");
    assert_no_tar_warnings(&output, "Concurrent file operations");
    
    // All files should be removed
    for file in &files {
        assert!(!file.exists(), "File {} should be removed", file.display());
    }
    
    // Should have one archive directory (all files in same directory)
    let archive_dirs = get_archive_dirs(&rmrf_dir);
    assert_eq!(archive_dirs.len(), 1, "Should have one archive directory for concurrent operations");
    
    let metadata = read_metadata(&archive_dirs[0]);
    for i in 0..5 {
        assert!(metadata.contains(&format!("- file_{}.txt", i)), 
                "Metadata should contain file_{}.txt", i);
    }
} 