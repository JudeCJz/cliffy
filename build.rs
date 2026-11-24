use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rustc-link-arg=/EXPORT:RTSSHooksCompatibility");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    let Some(out_dir) = env::var_os("OUT_DIR").map(PathBuf::from) else {
        println!("cargo:warning=OUT_DIR is not set; skipping Windows icon resource");
        return;
    };
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let icon = manifest_dir.join("assets").join("icon.ico");
    if !icon.is_file() {
        println!(
            "cargo:warning=Icon not found at {}; skipping Windows icon resource",
            icon.display()
        );
        return;
    }
    let Some(rc_exe) = find_rc_exe() else {
        println!("cargo:warning=rc.exe not found; skipping Windows icon resource");
        return;
    };

    // RC accepts forward slashes; they avoid backslash escaping in the script.
    let icon_for_rc = icon.to_string_lossy().replace('\\', "/");
    let rc_file = out_dir.join("cliffy_icon.rc");
    let res_file = out_dir.join("cliffy_icon.res");
    let rc_source = format!("1 ICON \"{}\"\n", icon_for_rc);
    if let Err(e) = fs::write(&rc_file, rc_source) {
        println!("cargo:warning=Could not write {}: {}", rc_file.display(), e);
        return;
    }

    match Command::new(&rc_exe)
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .output()
    {
        Ok(output) if output.status.success() && res_file.is_file() => {
            println!("cargo:rustc-link-arg={}", res_file.display());
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!(
                "cargo:warning=rc.exe failed to compile icon resource: {}",
                stderr.trim()
            );
        }
        Err(e) => println!(
            "cargo:warning=Could not run rc.exe at {}: {}",
            rc_exe.display(),
            e
        ),
    }
}

fn find_rc_exe() -> Option<PathBuf> {
    if let Some(lib) = env::var_os("LIB") {
        for dir in env::split_paths(&lib) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let sdk_bin = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut version_dirs: Vec<PathBuf> = fs::read_dir(sdk_bin)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir())
                .map(|_| entry.path())
        })
        .collect();
    // Newer SDK versions sort last by name (10.0.26100.0 > 10.0.22621.0).
    version_dirs.sort();
    for dir in version_dirs.into_iter().rev() {
        let candidate = dir.join("x64").join("rc.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
