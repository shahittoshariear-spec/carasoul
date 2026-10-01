//! Build script: embeds the .ico and the version info into the executable.
//!
//! rustc cannot attach a Windows resource on its own, so a `.rc` file is written
//! into `OUT_DIR`, compiled with the SDK's `rc.exe` and the resulting `.res`
//! handed straight to the linker. None of this is needed for the app to *work*:
//! the tray icon is built at runtime from an embedded PNG (see `src/tray.rs`), so
//! if `rc.exe` is missing the build still succeeds and only the executable's own
//! icon in Explorer is left as the default.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/carasoul.ico");
    println!("cargo:rerun-if-changed=build.rs");

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let ico = manifest.join("assets").join("carasoul.ico");
    if !ico.is_file() {
        println!(
            "cargo:warning={} is missing; building without an app icon",
            ico.display()
        );
        return;
    }
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        println!("cargo:warning=icon resource skipped: only wired up for the MSVC toolchain");
        return;
    }

    let Some(rc) = find_rc() else {
        println!(
            "cargo:warning=rc.exe not found (needs the Windows SDK that ships with the VS \
             build tools); building without an app icon"
        );
        return;
    };

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let script = out.join("carasoul.rc");
    let res = out.join("carasoul.res");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    if std::fs::write(&script, rc_source(&ico, &version)).is_err() {
        println!(
            "cargo:warning=could not write {}; building without an app icon",
            script.display()
        );
        return;
    }

    let output = Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg(&script)
        .output();
    match output {
        Ok(o) if o.status.success() && res.is_file() => {
            println!("cargo:rustc-link-arg={}", res.display());
        }
        Ok(o) => println!(
            "cargo:warning=rc.exe failed, building without an app icon: {} {}",
            String::from_utf8_lossy(&o.stdout).trim(),
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => {
            println!("cargo:warning=could not run rc.exe ({e}); building without an app icon")
        }
    }
}

/// The four comma-separated numbers `VERSIONINFO` insists on, whatever the cargo
/// version happens to look like.
fn version_quad(version: &str) -> String {
    let mut parts: Vec<String> = version
        .split('.')
        .map(|p| {
            p.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .map(|p| if p.is_empty() { "0".to_string() } else { p })
        .collect();
    parts.truncate(4);
    while parts.len() < 4 {
        parts.push("0".to_string());
    }
    parts.join(",")
}

/// The resource script: the icon group (ID 1, which is what Explorer picks up) and
/// the version block that names the app in Task Manager's process list.
fn rc_source(ico: &Path, version: &str) -> String {
    // rc treats backslashes as escapes inside a string, so a Windows path has to
    // be written doubled up or it eats the letters after them.
    let ico = ico.display().to_string().replace('\\', "\\\\");
    let quad = version_quad(version);
    format!(
        "1 ICON \"{ico}\"\n\
         1 VERSIONINFO\n\
         \x20FILEVERSION {quad}\n\
         \x20PRODUCTVERSION {quad}\n\
         \x20FILEFLAGSMASK 0x3fL\n\
         \x20FILEFLAGS 0x0L\n\
         \x20FILEOS 0x40004L\n\
         \x20FILETYPE 0x1L\n\
         \x20FILESUBTYPE 0x0L\n\
         BEGIN\n\
         \x20 BLOCK \"StringFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   BLOCK \"040904b0\"\n\
         \x20   BEGIN\n\
         \x20     VALUE \"FileDescription\", \"carasoul\"\n\
         \x20     VALUE \"FileVersion\", \"{version}\"\n\
         \x20     VALUE \"InternalName\", \"carasoul\"\n\
         \x20     VALUE \"OriginalFilename\", \"carasoul.exe\"\n\
         \x20     VALUE \"ProductName\", \"carasoul\"\n\
         \x20     VALUE \"ProductVersion\", \"{version}\"\n\
         \x20   END\n\
         \x20 END\n\
         \x20 BLOCK \"VarFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   VALUE \"Translation\", 0x409, 1200\n\
         \x20 END\n\
         END\n",
        ico = ico,
        version = version,
        quad = quad,
    )
}

/// `rc.exe` lives on the PATH only inside a developer command prompt, so look
/// there first and then in the Windows SDK, newest version first.
fn find_rc() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86") => "x86",
        _ => "x64",
    };
    let kits = ["ProgramFiles(x86)", "ProgramW6432", "ProgramFiles"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .map(|p| p.join("Windows Kits").join("10").join("bin"))
        .find(|p| p.is_dir())?;

    let mut versions: Vec<(Vec<u32>, PathBuf)> = std::fs::read_dir(kits)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let key: Vec<u32> = name.split('.').filter_map(|p| p.parse().ok()).collect();
            if key.is_empty() {
                return None;
            }
            let rc = e.path().join(arch).join("rc.exe");
            rc.is_file().then_some((key, rc))
        })
        .collect();
    versions.sort_by(|a, b| a.0.cmp(&b.0));
    versions.pop().map(|(_, rc)| rc)
}
