// Windows icon and version resource, included by a crate build script.
// The build script defines DESCRIPTION, ORIGINAL_NAME, and INTERNAL_NAME,
// then includes this file. The product version comes from package.json at
// the repository root, so the binaries and the changelog share one source.
//
// windows-gnu targets compile the resource with windres, which the release
// build uses. An MSVC build uses rc.exe when it is on PATH and otherwise
// builds without the resource, so CI test builds stay green.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../build/win-resource.rs");
    println!("cargo:rerun-if-changed=../../../package.json");
    println!("cargo:rerun-if-changed=../../assets/ntfy.ico");

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
            println!("cargo:warning=rc.exe not found, {DESCRIPTION} has no icon or version resource");
        }
        Err(err) => panic!("run {rc_tool}: {err}"),
    }
}

fn read_version(path: &Path) -> String {
    let text =
        fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let key = "\"version\"";
    let at = text
        .find(key)
        .unwrap_or_else(|| panic!("{} has no version", path.display()))
        + key.len();
    let rest = &text[at..];
    let open = rest.find('"').expect("version is not a string") + 1;
    let rest = &rest[open..];
    let close = rest.find('"').expect("version is not closed");
    let version = rest[..close].to_string();
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

fn resource(version: &str, icon: &Path) -> String {
    let mut four: Vec<u16> = version
        .split('.')
        .map(|part| part.parse().expect("numeric version"))
        .collect();
    while four.len() < 4 {
        four.push(0);
    }
    let numbers = format!("{},{},{},{}", four[0], four[1], four[2], four[3]);
    let icon = icon.display().to_string().replace('\\', "\\\\");
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
         VALUE \"FileDescription\", \"{DESCRIPTION}\"\n\
         VALUE \"FileVersion\", \"{version}\"\n\
         VALUE \"InternalName\", \"{INTERNAL_NAME}\"\n\
         VALUE \"OriginalFilename\", \"{ORIGINAL_NAME}\"\n\
         VALUE \"ProductName\", \"ntfy relay\"\n\
         VALUE \"ProductVersion\", \"{version}\"\n\
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
