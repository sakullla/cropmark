fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        stage_directml();
    }
    tauri_build::build();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=framework=ScreenCaptureKit");
        println!("cargo:rustc-link-lib=framework=CoreGraphics");
        println!("cargo:rustc-link-lib=framework=AppKit");
        // R7:元素级吸附的 macOS 控件级命中(AXUIElement/AXIsProcessTrusted),
        // 由 snap-macos 任务实现;这里先预留 ApplicationServices 链接。
        println!("cargo:rustc-link-lib=framework=ApplicationServices");
        println!("cargo:rerun-if-changed=src/capture/macos_sck.m");
        cc::Build::new()
            .file("src/capture/macos_sck.m")
            .flag("-fobjc-arc")
            .compile("cropmark_sck");
    }
}

/// Cargo 1.100 的构建目录是 `target/<profile>/build/<package>/<hash>/out`。
/// 测试可执行文件在各自的 `out` 里，而 Windows 会先于 PATH 加载 exe 旁边的
/// `DirectML.dll`。这里把 ort 下载的 DLL 放到 profile 目录；测试启动前由
/// `windows/directml-runner.ps1` 再拷到 exe 旁边。
fn stage_directml() {
    let source = find_directml().unwrap_or_else(|| {
        panic!(
            "DirectML.dll was not found after the ort-sys download. \
             Looked in ORT_LIB_LOCATION and %LOCALAPPDATA%\\ort.pyke.io\\dfbin"
        );
    });
    let profile = profile_dir_from(&std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()));
    let staged = profile.join("DirectML.dll");
    place_dll(&source, &staged);
    // Cargo 1.99 及更早把测试和示例放在这两个目录里。
    for extra in ["deps", "examples"] {
        let directory = profile.join(extra);
        std::fs::create_dir_all(&directory).unwrap_or_else(|error| {
            panic!("failed to create {}: {error}", directory.display());
        });
        place_dll(&source, &directory.join("DirectML.dll"));
    }
    println!("cargo:rerun-if-changed={}", source.display());
}

/// `target/<profile>`，同时兼容 Cargo 1.100 前后两种 OUT_DIR。
fn profile_dir_from(out_dir: &std::path::Path) -> std::path::PathBuf {
    let mut ancestors = out_dir.ancestors().skip(2);
    let next = ancestors
        .next()
        .expect("OUT_DIR should sit under target/<profile>/build");
    if next.ends_with("cropmark") {
        ancestors.nth(1).expect("profile directory").to_path_buf()
    } else {
        ancestors.next().expect("profile directory").to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::profile_dir_from;
    use std::path::Path;

    #[test]
    fn profile_dir_accepts_cargo_1_100_and_earlier_layouts() {
        let new_layout = Path::new("/work/src-tauri/target/debug/build/cropmark/abc123/out");
        let old_layout = Path::new("/work/src-tauri/target/debug/build/cropmark-abc123/out");
        assert_eq!(
            profile_dir_from(new_layout),
            Path::new("/work/src-tauri/target/debug")
        );
        assert_eq!(
            profile_dir_from(old_layout),
            Path::new("/work/src-tauri/target/debug")
        );
        let new_layout_target = Path::new(
            "/work/src-tauri/target/x86_64-pc-windows-msvc/release/build/cropmark/abc123/out",
        );
        assert_eq!(
            profile_dir_from(new_layout_target),
            Path::new("/work/src-tauri/target/x86_64-pc-windows-msvc/release")
        );
    }
}

fn find_directml() -> Option<std::path::PathBuf> {
    for variable in ["ORT_LIB_LOCATION", "ORT_LIB_PATH"] {
        if let Some(location) = std::env::var_os(variable) {
            let location = std::path::PathBuf::from(location);
            for candidate in [
                location.join("DirectML.dll"),
                location.join("lib").join("DirectML.dll"),
            ] {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    let local = std::env::var_os("LOCALAPPDATA")?;
    let root = std::path::PathBuf::from(local)
        .join("ort.pyke.io")
        .join("dfbin");
    find_named_file(&root, "DirectML.dll", 6)
}

fn find_named_file(dir: &std::path::Path, name: &str, depth: usize) -> Option<std::path::PathBuf> {
    if depth == 0 || !dir.is_dir() {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().and_then(|file| file.to_str()) == Some(name) {
            return Some(path);
        }
        if path.is_dir() {
            subdirs.push(path);
        }
    }
    for subdir in subdirs {
        if let Some(found) = find_named_file(&subdir, name, depth - 1) {
            return Some(found);
        }
    }
    None
}

fn place_dll(source: &std::path::Path, dest: &std::path::Path) {
    if let Ok(meta) = std::fs::symlink_metadata(dest) {
        if meta.file_type().is_symlink() {
            std::fs::remove_file(dest).unwrap_or_else(|error| {
                panic!("failed to replace {}: {error}", dest.display());
            });
        } else if meta.is_file()
            && std::fs::metadata(source)
                .map(|source_meta| source_meta.len() == meta.len())
                .unwrap_or(false)
        {
            return;
        }
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).unwrap_or_else(|error| {
            panic!("failed to create {}: {error}", parent.display());
        });
    }
    std::fs::copy(source, dest).unwrap_or_else(|error| {
        panic!(
            "failed to copy {} to {}: {error}",
            source.display(),
            dest.display()
        );
    });
}
