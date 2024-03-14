use std::env;
use std::fs::{read_to_string, write, File};
use std::io::Write;
use std::path::Path;
use std::process::Command;

fn git_describe_value() -> String {
    env::var("GIT_DESCRIBE").unwrap_or_else(|_| {
        let output = Command::new("git")
            .args(&["describe"])
            .output()
            .expect("Failed to execute `git describe`");

        String::from_utf8(output.stdout).expect("Not UTF-8").trim().to_string()
    })
}

fn main() {
    let out_dir = env::var("OUT_DIR").unwrap();
    let git_describe_path = Path::new(&out_dir).join("GIT_DESCRIBE");

    let old_value = read_to_string(&git_describe_path)
        .unwrap_or_default()
        .trim()
        .to_string();
    let new_value = git_describe_value();

    if new_value != old_value {
        println!("BUILD_RS: Version changed from '{}' to '{}'", old_value, new_value);
        write(&git_describe_path, &new_value).unwrap();

        let git_describe_rs = Path::new(&out_dir).join("git_describe.rs");
        let mut f = File::create(&git_describe_rs).unwrap();
        write!(f, "pub const GIT_DESCRIBE: &'static str = \"{}\";", new_value).unwrap();
    }

    println!("cargo:rerun-if-env-changed=GIT_DESCRIBE");
    println!("cargo:rerun-if-changed={}", git_describe_path.display());
}
