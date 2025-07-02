use std::fs;
use std::process::Command;
use tempfile::TempDir;

#[test]
fn test_rmrf_correct_cwd_for_files_outside_current_dir() {
    // Build the binary first
    let build_output = Command::new("cargo")
        .args(&["build"])
        .output()
        .expect("Failed to build project");
    
    if !build_output.status.success() {
        panic!("Failed to build project: {}", String::from_utf8_lossy(&build_output.stderr));
    }
    
    // Create a temporary directory structure
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    
    // Create test directories and files
    let test_dir1 = temp_path.join("test_logs");
    fs::create_dir_all(&test_dir1).unwrap();
    
    let test_file1 = test_dir1.join("app.log");
    fs::write(&test_file1, "test log content 1").unwrap();
    
    // Set up temporary rmrf directory
    let rmrf_dir = temp_path.join("rmrf");
    fs::create_dir_all(&rmrf_dir).unwrap();
    
    // Create a temporary config file
    let config_dir = temp_path.join(".config").join("rmrf");
    fs::create_dir_all(&config_dir).unwrap();
    let config_file = config_dir.join("rmrf.cfg");
    fs::write(&config_file, format!("[DEFAULT]\nrmrf_path = {}\nsudo = no\nkeep = 21\n", rmrf_dir.display())).unwrap();
    
    // Get the binary path
    let binary_path = std::env::current_dir().unwrap().join("target/debug/rkvr");
    
    // Run rmrf on files in different directories
    let output = Command::new(&binary_path)
        .args(&["rmrf", test_file1.to_str().unwrap()])
        .env("HOME", temp_path)
        .output()
        .expect("Failed to execute rmrf command");
    
    if !output.status.success() {
        panic!("rmrf command failed: {}\nStderr: {}", 
               String::from_utf8_lossy(&output.stdout),
               String::from_utf8_lossy(&output.stderr));
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
    assert!(metadata_content.contains(&format!("cwd: {}", test_dir1.display())), 
            "Metadata should contain correct CWD. Actual content:\n{}", metadata_content);
    
    assert!(metadata_content.contains("- app.log"), 
            "Metadata should contain the filename. Actual content:\n{}", metadata_content);
}

#[test] 
fn test_rmrf_no_tar_warnings_for_absolute_paths() {
    // Build the binary first
    let build_output = Command::new("cargo")
        .args(&["build"])
        .output()
        .expect("Failed to build project");
    
    if !build_output.status.success() {
        panic!("Failed to build project: {}", String::from_utf8_lossy(&build_output.stderr));
    }
    
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    
    // Create test file with absolute path
    let test_dir = temp_path.join("deep").join("nested").join("path");
    fs::create_dir_all(&test_dir).unwrap();
    let test_file = test_dir.join("deep_file.log");
    fs::write(&test_file, "deep nested content").unwrap();
    
    // Set up rmrf directory
    let rmrf_dir = temp_path.join("rmrf");
    fs::create_dir_all(&rmrf_dir).unwrap();
    
    // Create config
    let config_dir = temp_path.join(".config").join("rmrf");
    fs::create_dir_all(&config_dir).unwrap();
    let config_file = config_dir.join("rmrf.cfg");
    fs::write(&config_file, format!("[DEFAULT]\nrmrf_path = {}\nsudo = no\n", rmrf_dir.display())).unwrap();
    
    // Get the binary path
    let binary_path = std::env::current_dir().unwrap().join("target/debug/rkvr");
    
    let output = Command::new(&binary_path)
        .args(&["rmrf", test_file.to_str().unwrap()])
        .env("HOME", temp_path)
        .output()
        .expect("Failed to execute rmrf command");
    
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    
    // Should not contain tar warnings about removing leading '/' from member names
    assert!(!stderr.contains("Removing leading"), 
            "Should not have tar warnings about absolute paths. Stderr:\n{}\nStdout:\n{}", stderr, stdout);
    
    if !output.status.success() {
        panic!("rmrf command should succeed. Stderr:\n{}\nStdout:\n{}", stderr, stdout);
    }
} 