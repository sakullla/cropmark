//! Unix(macOS/Linux)单实例:flock 锁文件 + Unix socket(ADR-1)。
//!
//! 首实例在 `$XDG_RUNTIME_DIR`(缺失时回退 `$TMPDIR`,再回退 `/tmp`)下
//! 持有 `cropmark-<uid>.lock` 的 flock,并监听 `cropmark-<uid>.sock`;
//! 后续进程发现锁被占用时,连接 socket 转发 argv/cwd 后退出(成功与否均退出)。
//! 首实例正常退出或崩溃由内核释放锁,下一次启动清理遗留 socket 后可重新
//! 成为唯一实例;文件不含版本号,跨版本升级仍互斥。

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::{LaunchHandler, SecondLaunch};

/// 向首实例写启动参数的写入超时:首实例挂起也不会让第二进程无限等待。
const WRITE_TIMEOUT: Duration = Duration::from_millis(1000);
/// 读取单条转发消息的超时:避免半开连接占住监听线程。
const READ_TIMEOUT: Duration = Duration::from_millis(1000);
const ACCEPT_POLL: Duration = Duration::from_millis(50);

/// 首实例守卫:持有锁文件与监听线程的停止标记。
pub struct Primary {
    _lock: File,
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
}

impl Drop for Primary {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// 按 ADR-1 使用 `cropmark-<uid>` 命名实例文件(同用户仅一个实例,
/// 与版本无关);`app_id` 仅 Windows 命名互斥量需要,这里不参与命名。
pub fn acquire(_app_id: &str, handler: LaunchHandler) -> Result<super::Acquire, String> {
    let uid = unsafe { libc::getuid() };
    let directory = runtime_dir();
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("无法创建运行时目录 {}:{error}", directory.display()))?;
    let lock_path = directory.join(lock_file_name(uid));
    let socket_path = directory.join(socket_file_name(uid));

    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| format!("无法打开实例锁 {}:{error}", lock_path.display()))?;
    let _ = std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600));

    let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if locked != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            forward_launch(&socket_path);
            return Ok(super::Acquire::Forwarded);
        }
        return Err(format!("无法锁定实例文件 {}:{error}", lock_path.display()));
    }

    // 持有锁即可确定上一次实例已退出,可安全清理其遗留的 socket 文件。
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .map_err(|error| format!("无法绑定实例套接字 {}:{error}", socket_path.display()))?;
    let stop = Arc::new(AtomicBool::new(false));
    listen(listener, Arc::clone(&stop), handler);

    Ok(super::Acquire::Primary(Primary {
        _lock: lock,
        socket_path,
        stop,
    }))
}

fn lock_file_name(uid: u32) -> String {
    format!("cropmark-{uid}.lock")
}

fn socket_file_name(uid: u32) -> String {
    format!("cropmark-{uid}.sock")
}

fn runtime_dir() -> PathBuf {
    pick_runtime_dir(non_empty_env("XDG_RUNTIME_DIR"), non_empty_env("TMPDIR"))
}

fn non_empty_env(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

fn pick_runtime_dir(preferred: Option<PathBuf>, fallback: Option<PathBuf>) -> PathBuf {
    preferred
        .or(fallback)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// 监听 socket;收到消息即交给回调(回调实现负责切回主线程)。
fn listen(listener: UnixListener, stop: Arc<AtomicBool>, handler: LaunchHandler) {
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    let _ = thread::Builder::new()
        .name("cropmark-single-instance".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => deliver(stream, &handler),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(ACCEPT_POLL);
                    }
                    Err(_) => break,
                }
            }
        });
}

fn deliver(stream: UnixStream, handler: &LaunchHandler) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return;
    }
    if let Some(launch) = SecondLaunch::from_bytes(line.trim().as_bytes()) {
        handler(launch);
    }
}

/// 第二实例路径:连接首实例 socket 并写出启动参数;连接失败(如首实例仍在
/// 绑定 socket)也直接返回,由调用方退出。首实例无响应时内核仍会完成连接,
/// 小报文写入不会阻塞超过 [`WRITE_TIMEOUT`]。
fn forward_launch(socket_path: &Path) {
    let payload = SecondLaunch::current().to_bytes();
    let Ok(mut stream) = UnixStream::connect(socket_path) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let _ = stream.write_all(&payload);
    let _ = stream.write_all(b"\n");
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_prefers_xdg_then_tmp_then_root_tmp() {
        let xdg = PathBuf::from("/run/user/1000");
        let tmp = PathBuf::from("/var/folders/aa/bb/T");
        assert_eq!(pick_runtime_dir(Some(xdg.clone()), Some(tmp.clone())), xdg);
        assert_eq!(pick_runtime_dir(None, Some(tmp.clone())), tmp);
        assert_eq!(pick_runtime_dir(None, None), PathBuf::from("/tmp"));
    }

    #[test]
    fn instance_file_names_are_version_independent() {
        assert_eq!(socket_file_name(501), "cropmark-501.sock");
        assert_eq!(lock_file_name(501), "cropmark-501.lock");
        assert!(!socket_file_name(501).contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn second_flock_on_the_same_file_is_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "cropmark-single-instance-{}-{}",
            std::process::id(),
            std::line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lock");

        let first = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(first.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "首个锁应成功"
        );

        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1,
            "第二个描述符不应拿到同一把锁"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EWOULDBLOCK)
        );

        drop(second);
        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
