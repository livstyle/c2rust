use std::{
    env,
    fs::{self, File},
    path::{Path, PathBuf},
    process::Command,
};

use c2rust_build_paths::find_llvm_config;

#[derive(Default)]
pub struct Analyze;

impl Analyze {
    pub fn new() -> Self {
        Self
    }

    fn run_(&self, rs_path: &Path) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let lib_dir = Path::new(env!("C2RUST_TARGET_LIB_DIR"));

        let manifest_path = dir.join("Cargo.toml");
        let rs_path = dir.join(rs_path); // allow relative paths, or override with an absolute path
        let output_path = {
            let mut file_name = rs_path.file_name().unwrap().to_owned();
            file_name.push(".analysis.txt");
            rs_path.with_file_name(file_name)
        };
        let output_stdout = File::create(&output_path).unwrap();
        let output_stderr = File::try_clone(&output_stdout).unwrap();

        let mut cmd = Command::new("cargo");
        cmd.arg("run")
            .arg("--manifest-path")
            .arg(&manifest_path)
            .arg("--")
            .arg(&rs_path)
            .arg("-L")
            .arg(lib_dir)
            .arg("--crate-type")
            .arg("rlib")
            .stdout(output_stdout)
            .stderr(output_stderr);
        let status = cmd.status().unwrap();
        if !status.success() {
            let message = format!(
                "c2rust-analyze failed with status {status}:\n> {cmd:?} > {output_path:?} 2>&1\n"
            );
            let output = fs::read_to_string(&output_path).unwrap();
            panic!("\n{message}\n{output}\n{message}");
        }
        output_path
    }

    pub fn run(&self, rs_path: impl AsRef<Path>) -> PathBuf {
        self.run_(rs_path.as_ref())
    }
}

pub struct FileCheck {
    path: PathBuf,
}

impl FileCheck {
    pub fn resolve() -> Self {
        let path = env::var_os("FILECHECK")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let llvm_config = find_llvm_config().expect("llvm-config not found");
                let output = Command::new(llvm_config)
                    .args(&["--bindir"])
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
                    .expect("llvm-config error");
                let bin_dir =
                    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim().to_owned());
                bin_dir.join("FileCheck")
            });
        Self { path }
    }

    fn run_(&self, path: &Path, input: &Path) {
        let mut cmd = Command::new(&self.path);
        let input_file = File::open(input).unwrap();
        cmd.arg(path).stdin(input_file);
        let status = cmd.status().unwrap();
        assert!(
            status.success(),
            "\nFileCheck failed with status {status}:\n> {cmd:?} > {input:?}\n",
        );
    }

    pub fn run(&self, path: impl AsRef<Path>, input: impl AsRef<Path>) {
        self.run_(path.as_ref(), input.as_ref())
    }
}