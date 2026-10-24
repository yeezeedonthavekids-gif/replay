//! Embeds the icons, application manifest and version information into replay.exe.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=assets");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let assets = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets");
    let path = |name: &str| assets.join(name).to_string_lossy().replace('\\', "/");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<u32> = version.split(['.', '-']).filter_map(|p| p.parse().ok()).collect();
    parts.resize(4, 0);
    let commas = parts.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    let rc = format!(
        r#"#include <winver.h>
1 ICON "{app}"
2 ICON "{idle}"
3 ICON "{rec}"
1 24 "{manifest}"
VS_VERSION_INFO VERSIONINFO
FILEVERSION {commas}
PRODUCTVERSION {commas}
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Replay"
      VALUE "FileDescription", "Replay - instant replay recorder"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "replay"
      VALUE "OriginalFilename", "replay.exe"
      VALUE "ProductName", "Replay"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        app = path("replay.ico"),
        idle = path("tray_idle.ico"),
        rec = path("tray_rec.ico"),
        manifest = path("replay.manifest"),
    );
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("replay.rc");
    std::fs::write(&out, rc).unwrap();
    embed_resource::compile(&out, embed_resource::NONE).manifest_required().unwrap();
}
