// Windows icon and version resource, included by a crate build script.
// The build script defines DESCRIPTION, ORIGINAL_NAME, and INTERNAL_NAME,
// then includes this file. The product version comes from package.json at
// the repository root, so the binaries and the changelog share one source.
//
// windows-gnu targets compile the resource with windres. An MSVC build uses
// rc.exe and fails when it is missing, because a binary without the icon or
// the version resource is not a release artefact.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../build/win-resource.rs");
    println!("cargo:rerun-if-changed=../../../package.json");
    println!("cargo:rerun-if-changed=../../assets/ntfy.ico");
    println!("cargo:rerun-if-env-changed=NTFY_WINDRES");
    println!("cargo:rerun-if-env-changed=NTFY_RC");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = manifest
        .ancestors()
        .nth(3)
        .expect("repository root above the crate")
        .to_path_buf();
    let version = read_version(&root.join("package.json"));
    let icon = root.join("rust").join("assets").join("ntfy.ico");
    assert!(icon.is_file(), "missing {}", icon.display());

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let rc = out.join("ntfy.rc");
    let res = out.join("ntfy.res");
    fs::write(&rc, resource(&version, &icon)).expect("write the resource script");

    let env_name = env::var("CARGO_CFG_TARGET_ENV").expect("CARGO_CFG_TARGET_ENV");
    if env_name == "gnu" {
        let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH");
        let tool = env::var("NTFY_WINDRES").unwrap_or_else(|_| format!("{arch}-w64-mingw32-windres"));
        let status = Command::new(&tool)
            .arg(&rc)
            .arg("-O")
            .arg("coff")
            .arg("-o")
            .arg(&res)
            .status()
            .unwrap_or_else(|err| panic!("run {tool}: {err}"));
        assert!(status.success(), "{tool} failed with {status}");
        println!("cargo:rustc-link-arg-bins={}", res.display());
        return;
    }

    let rc_tool = env::var("NTFY_RC").unwrap_or_else(|_| "rc".to_string());
    match Command::new(&rc_tool)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg(&rc)
        .status()
    {
        Ok(status) => {
            assert!(status.success(), "{rc_tool} failed with {status}");
            println!("cargo:rustc-link-arg-bins={}", res.display());
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            panic!("{rc_tool} not found; install the Windows SDK or set NTFY_RC");
        }
        Err(err) => panic!("run {rc_tool}: {err}"),
    }
}

fn read_version(path: &Path) -> String {
    let text =
        fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let value: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("parse {}: {err}", path.display()));
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("{} has no top-level version", path.display()))
        .to_string();
    let parts: Vec<&str> = version.split('.').collect();
    assert!(
        (2..=4).contains(&parts.len())
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
        "unexpected version {version}"
    );
    version
}

/// One `.rc` string: a backslash, quote, or newline would end the value or the
/// directive early, so each is escaped.
fn rc_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

fn resource(version: &str, icon: &Path) -> String {
    let mut four: Vec<u16> = version
        .split('.')
        .map(|part| part.parse().expect("numeric version"))
        .collect();
    while four.len() < 4 {
        four.push(0);
    }
    let numbers = format!("{},{},{},{}", four[0], four[1], four[2], four[3]);
    let icon = rc_string(&icon.display().to_string());
    let version_text = rc_string(version);
    let description = rc_string(DESCRIPTION);
    let internal_name = rc_string(INTERNAL_NAME);
    let original_name = rc_string(ORIGINAL_NAME);
    format!(
        "1 ICON \"{icon}\"\n\
         1 VERSIONINFO\n\
         FILEVERSION {numbers}\n\
         PRODUCTVERSION {numbers}\n\
         FILEFLAGSMASK 0x3fL\n\
         FILEFLAGS 0x0L\n\
         FILEOS 0x40004L\n\
         FILETYPE 0x1L\n\
         FILESUBTYPE 0x0L\n\
         BEGIN\n\
         BLOCK \"StringFileInfo\"\n\
         BEGIN\n\
         BLOCK \"040904b0\"\n\
         BEGIN\n\
         VALUE \"FileDescription\", \"{description}\"\n\
         VALUE \"FileVersion\", \"{version_text}\"\n\
         VALUE \"InternalName\", \"{internal_name}\"\n\
         VALUE \"OriginalFilename\", \"{original_name}\"\n\
         VALUE \"ProductName\", \"ntfy relay\"\n\
         VALUE \"ProductVersion\", \"{version_text}\"\n\
         VALUE \"CompanyName\", \"chezhenkie\"\n\
         VALUE \"LegalCopyright\", \"MIT License, Copyright (c) 2026 chezhenkie\"\n\
         END\n\
         END\n\
         BLOCK \"VarFileInfo\"\n\
         BEGIN\n\
         VALUE \"Translation\", 0x409, 1200\n\
         END\n\
         END\n"
    )
}
