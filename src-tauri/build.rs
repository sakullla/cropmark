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
    let source = ensure_directml_dll();
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

    #[test]
    fn pinned_ort_cache_matches_ort_sys_windows_none() {
        let local = Path::new("C:/Users/runneradmin/AppData/Local");
        let hash_dir = super::pinned_hash_dir(local, "x86_64-pc-windows-msvc")
            .expect("x86_64-pc-windows-msvc is the pinned ort archive");
        assert_eq!(
            hash_dir.join("onnxruntime").join("lib").join("DirectML.dll"),
            Path::new(
                "C:/Users/runneradmin/AppData/Local/ort.pyke.io/dfbin/x86_64-pc-windows-msvc/540D19B3379FDA6FB8F7280D8C15EFDE20ED225A67A357A6DAE38C4300FE190D/onnxruntime/lib/DirectML.dll"
            )
        );
        assert!(super::pinned_hash_dir(local, "aarch64-pc-windows-msvc").is_none());
    }

    #[test]
    fn certutil_sha256_accepts_spaced_and_compact_lines() {
        let digest = "540D19B3379FDA6FB8F7280D8C15EFDE20ED225A67A357A6DAE38C4300FE190D";
        let spaced = digest
            .as_bytes()
            .chunks(2)
            .map(|pair| std::str::from_utf8(pair).unwrap())
            .collect::<Vec<_>>()
            .join(" ");
        let output = format!("SHA256 hash of archive.tgz:\n{spaced}\nCertUtil: -hashfile command completed successfully.\n");
        assert_eq!(
            super::sha256_from_certutil_output(&output).as_deref(),
            Some(digest)
        );
        let compact = format!("SHA256 hash of archive.tgz:\n{digest}\n");
        assert_eq!(
            super::sha256_from_certutil_output(&compact).as_deref(),
            Some(digest)
        );
    }
}

/// ort-sys 2.0.0-rc.10、feature `none` 的 Windows 预编译包。
/// 与 ort-sys `dist.txt` 的 URL 和 SHA256 保持一致。链接参数会把
/// `%LOCALAPPDATA%\ort.pyke.io\dfbin\<target>\<hash>` 写进 `target/`。
/// CI 只缓存 `target/`，下一台机器上这个目录是空的，而 ort-sys 缓存命中后
/// 不会重新下载。
const ORT_WIN_TARGET: &str = "x86_64-pc-windows-msvc";
const ORT_WIN_URL: &str = "https://cdn.pyke.io/0/pyke:ort-rs/ms@1.22.0/x86_64-pc-windows-msvc.tgz";
const ORT_WIN_SHA256: &str = "540D19B3379FDA6FB8F7280D8C15EFDE20ED225A67A357A6DAE38C4300FE190D";

fn ensure_directml_dll() -> std::path::PathBuf {
    if let Some(dll) = directml_from_env().or_else(pinned_directml_dll) {
        return dll;
    }
    restore_pinned_ort_binaries();
    pinned_directml_dll()
        .or_else(find_cached_directml)
        .unwrap_or_else(|| {
            panic!(
                "DirectML.dll was not found. Looked in ORT_LIB_LOCATION, ORT_LIB_PATH, \
                 and %LOCALAPPDATA%\\ort.pyke.io\\dfbin. The pinned archive is {ORT_WIN_URL}."
            )
        })
}

fn pinned_hash_dir(local_app_data: &std::path::Path, target: &str) -> Option<std::path::PathBuf> {
    if target != ORT_WIN_TARGET {
        return None;
    }
    Some(
        local_app_data
            .join("ort.pyke.io")
            .join("dfbin")
            .join(target)
            .join(ORT_WIN_SHA256),
    )
}

fn pinned_directml_dll() -> Option<std::path::PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    let target = std::env::var("TARGET").ok()?;
    let lib_dir = pinned_hash_dir(std::path::Path::new(&local), &target)?
        .join("onnxruntime")
        .join("lib");
    let dll = lib_dir.join("DirectML.dll");
    if dll.is_file() && lib_dir.join("onnxruntime.lib").is_file() {
        Some(dll)
    } else {
        None
    }
}

fn restore_pinned_ort_binaries() {
    let Ok(target) = std::env::var("TARGET") else {
        return;
    };
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let Some(hash_dir) = pinned_hash_dir(std::path::Path::new(&local), &target) else {
        return;
    };
    if std::env::var("CARGO_NET_OFFLINE").as_deref() == Ok("true") {
        panic!(
            "DirectML.dll is missing from {} and CARGO_NET_OFFLINE=true",
            hash_dir.display()
        );
    }
    if hash_dir.exists() {
        std::fs::remove_dir_all(&hash_dir).unwrap_or_else(|error| {
            panic!(
                "failed to remove incomplete ONNX Runtime cache {}: {error}",
                hash_dir.display()
            );
        });
    }
    let parent = hash_dir.parent().expect("ort cache parent");
    std::fs::create_dir_all(parent).unwrap_or_else(|error| {
        panic!("failed to create {}: {error}", parent.display());
    });
    let archive = parent.join(format!("cropmark-ort-{}.tgz", std::process::id()));
    let extract = parent.join(format!("cropmark-ort-extract-{}", std::process::id()));
    let _ = std::fs::remove_file(&archive);
    let _ = std::fs::remove_dir_all(&extract);
    println!(
        "cargo:warning=restoring ONNX Runtime Windows binaries into {}",
        hash_dir.display()
    );
    download_file(ORT_WIN_URL, &archive);
    let actual = file_sha256(&archive);
    if !actual.eq_ignore_ascii_case(ORT_WIN_SHA256) {
        panic!("ONNX Runtime archive hash {actual} does not match {ORT_WIN_SHA256}");
    }
    extract_tgz(&archive, &extract);
    std::fs::rename(&extract, &hash_dir).unwrap_or_else(|error| {
        panic!(
            "failed to move extracted ONNX Runtime into {}: {error}",
            hash_dir.display()
        );
    });
    let _ = std::fs::remove_file(&archive);
    let dll = hash_dir
        .join("onnxruntime")
        .join("lib")
        .join("DirectML.dll");
    let static_lib = hash_dir
        .join("onnxruntime")
        .join("lib")
        .join("onnxruntime.lib");
    if !dll.is_file() || !static_lib.is_file() {
        panic!(
            "ONNX Runtime archive did not contain {} and {}",
            dll.display(),
            static_lib.display()
        );
    }
}

fn download_file(url: &str, dest: &std::path::Path) {
    let status = std::process::Command::new("curl.exe")
        .args([
            "--proto",
            "=https",
            "--tlsv1.2",
            "--retry",
            "5",
            "--retry-connrefused",
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--output",
        ])
        .arg(dest)
        .arg(url)
        .status()
        .unwrap_or_else(|error| panic!("curl.exe failed to start: {error}"));
    if !status.success() {
        panic!("failed to download {url}: curl exited {status}");
    }
}

fn file_sha256(path: &std::path::Path) -> String {
    let output = std::process::Command::new("certutil.exe")
        .args(["-hashfile"])
        .arg(path)
        .arg("SHA256")
        .output()
        .unwrap_or_else(|error| panic!("certutil.exe failed to start: {error}"));
    if !output.status.success() {
        panic!(
            "certutil.exe failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    sha256_from_certutil_output(&text)
        .unwrap_or_else(|| panic!("certutil.exe returned no SHA256 for {}", path.display()))
}

fn sha256_from_certutil_output(text: &str) -> Option<String> {
    text.lines()
        .map(|line| {
            line.chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>()
        })
        .find(|line| {
            line.len() == 64 && line.chars().all(|character| character.is_ascii_hexdigit())
        })
}

fn extract_tgz(archive: &std::path::Path, dest: &std::path::Path) {
    std::fs::create_dir_all(dest).unwrap_or_else(|error| {
        panic!("failed to create {}: {error}", dest.display());
    });
    let status = std::process::Command::new("tar.exe")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .status()
        .unwrap_or_else(|error| panic!("tar.exe failed to start: {error}"));
    if !status.success() {
        panic!(
            "failed to extract {}: tar exited {status}",
            archive.display()
        );
    }
}

fn directml_from_env() -> Option<std::path::PathBuf> {
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
    None
}

fn find_cached_directml() -> Option<std::path::PathBuf> {
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
