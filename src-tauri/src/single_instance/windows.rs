//! Windows 单实例:命名互斥量 + 隐藏消息窗(ADR-1)。
//!
//! 首实例创建由应用标识派生的命名互斥量(不含版本号),并注册一个
//! `WS_EX_TOOLWINDOW` 隐藏窗口接收实例消息;后续进程发现互斥量已存在时,
//! 用 `FindWindowW` 定位窗口,再以 `SendMessageTimeoutW`(2s、
//! `SMTO_ABORTIFHUNG`)转发 argv/cwd,无论发送结果如何都退出。
//! 首实例主线程挂起时,第二进程仍会在有界时间内退出。

use std::ffi::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE,
    HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowW, PostMessageW, RegisterClassW,
    SendMessageTimeoutW, SMTO_ABORTIFHUNG, WINDOW_STYLE, WM_APP, WM_COPYDATA, WNDCLASSW,
    WS_EX_TOOLWINDOW,
};

use super::{instance_key, LaunchHandler, SecondLaunch};

/// 隐藏消息窗的类名(跨进程约定,固定值)。
const MESSAGE_WINDOW_CLASS: &str = "CropmarkSingleInstanceWindow";
/// 首实例等待消息窗就绪的上限:覆盖互斥量已建、窗口尚未创建的极短启动窗口。
const FIND_WINDOW_TIMEOUT: Duration = Duration::from_millis(500);
const FIND_WINDOW_POLL: Duration = Duration::from_millis(25);
/// 向首实例发送实例消息的超时;超时/挂起时照常退出。
const FORWARD_TIMEOUT_MS: u32 = 2000;
const LAUNCH_MESSAGE: u32 = WM_APP + 1;

static CALLBACK: Mutex<Option<LaunchHandler>> = Mutex::new(None);
static PENDING: Mutex<Option<SecondLaunch>> = Mutex::new(None);

/// 首实例守卫:持有互斥量与隐藏消息窗。
pub struct Primary {
    mutex: HANDLE,
    window: HWND,
}

impl Drop for Primary {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.window);
            let _ = CloseHandle(self.mutex);
        }
    }
}

pub fn acquire(app_id: &str, handler: LaunchHandler) -> Result<super::Acquire, String> {
    *lock(&CALLBACK) = Some(handler);

    let name = mutex_name(app_id);
    let name_wide = wide(&name);
    let (mutex, already_exists) = unsafe {
        // GetLastError 只在 CreateMutexW 建新对象时未定义,先清零再判定。
        SetLastError(ERROR_SUCCESS);
        let mutex = CreateMutexW(None, false, PCWSTR::from_raw(name_wide.as_ptr()))
            .map_err(|error| format!("无法创建实例互斥量 {name}:{error}"))?;
        (mutex, GetLastError() == ERROR_ALREADY_EXISTS)
    };

    if already_exists {
        unsafe {
            let _ = CloseHandle(mutex);
        }
        forward_launch(app_id);
        return Ok(super::Acquire::Forwarded);
    }

    match create_message_window(app_id) {
        Ok(window) => Ok(super::Acquire::Primary(Primary { mutex, window })),
        Err(error) => {
            // 无法接收后续启动请求时释放互斥量,由调用方按普通启动继续,
            // 避免单实例能力不可用导致应用无法启动。
            unsafe {
                let _ = CloseHandle(mutex);
            }
            Err(format!("无法创建实例消息窗口:{error}"))
        }
    }
}

fn mutex_name(app_id: &str) -> String {
    format!("Local\\{}", instance_key(app_id))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn create_message_window(app_id: &str) -> windows::core::Result<HWND> {
    let class_name = wide(MESSAGE_WINDOW_CLASS);
    let title = wide(&instance_key(app_id));
    let module = unsafe { GetModuleHandleW(PCWSTR::null())? };
    let instance = HINSTANCE(module.0);
    let class = WNDCLASSW {
        hInstance: instance,
        lpfnWndProc: Some(message_window_proc),
        lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
        ..Default::default()
    };
    // 同名类重复注册返回 0 但类仍然可用,错误留给 CreateWindowExW 判定。
    unsafe {
        RegisterClassW(&class);
    }
    unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    }
}

/// 隐藏消息窗的消息处理:WM_COPYDATA 复制启动参数并转投 WM_APP,
/// 避免在跨进程同步消息的调用栈里创建窗口;WM_APP 再交给主线程回调。
unsafe extern "system" fn message_window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COPYDATA => {
            unsafe { copy_launch_payload(lparam) };
            let _ = unsafe { PostMessageW(Some(window), LAUNCH_MESSAGE, WPARAM(0), LPARAM(0)) };
            LRESULT(1)
        }
        LAUNCH_MESSAGE => {
            dispatch_pending();
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

/// 复制 WM_COPYDATA 携带的启动参数;缓冲区只在本消息内有效。
unsafe fn copy_launch_payload(lparam: LPARAM) {
    let pointer = lparam.0 as *const COPYDATASTRUCT;
    if pointer.is_null() {
        return;
    }
    let data = unsafe { &*pointer };
    if data.lpData.is_null() || data.cbData == 0 {
        return;
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(data.lpData as *const u8, data.cbData as usize) };
    if let Some(launch) = SecondLaunch::from_bytes(bytes) {
        *lock(&PENDING) = Some(launch);
    }
}

fn dispatch_pending() {
    let Some(launch) = lock(&PENDING).take() else {
        return;
    };
    if let Some(handler) = lock(&CALLBACK).clone() {
        handler(launch);
    }
}

/// 第二实例路径:定位首实例窗口并转发当前启动参数;找不到窗口时直接返回,
/// 由调用方退出(进程不会因此继续创建托盘)。
fn forward_launch(app_id: &str) {
    let payload = SecondLaunch::current().to_bytes();
    let _ = send_payload(app_id, &payload);
}

fn send_payload(app_id: &str, payload: &[u8]) -> bool {
    let Some(window) = find_message_window(app_id) else {
        return false;
    };
    let data = COPYDATASTRUCT {
        dwData: 0,
        cbData: payload.len() as u32,
        lpData: payload.as_ptr() as *mut c_void,
    };
    unsafe {
        let _ = SendMessageTimeoutW(
            window,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&data as *const COPYDATASTRUCT as isize),
            SMTO_ABORTIFHUNG,
            FORWARD_TIMEOUT_MS,
            None,
        );
    }
    true
}

fn find_message_window(app_id: &str) -> Option<HWND> {
    let class_name = wide(MESSAGE_WINDOW_CLASS);
    let title = wide(&instance_key(app_id));
    let deadline = Instant::now() + FIND_WINDOW_TIMEOUT;
    loop {
        let found = unsafe {
            FindWindowW(
                PCWSTR::from_raw(class_name.as_ptr()),
                PCWSTR::from_raw(title.as_ptr()),
            )
        };
        if let Ok(window) = found {
            return Some(window);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(FIND_WINDOW_POLL);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, MSG, PM_REMOVE};

    use super::*;

    #[test]
    fn mutex_name_is_derived_from_app_id_only() {
        let name = mutex_name("app.cropmark.desktop");
        assert!(name.contains("app.cropmark.desktop"));
        assert!(!name.contains(env!("CARGO_PKG_VERSION")));
        assert_ne!(
            mutex_name("app.cropmark.desktop"),
            mutex_name("app.cropmark.other")
        );
    }

    #[test]
    fn named_mutex_flags_second_creation_until_first_is_gone() {
        let name = mutex_name(&format!("app.cropmark.mutex-{}", std::process::id()));
        let name_wide = wide(&name);
        let create = || unsafe {
            SetLastError(ERROR_SUCCESS);
            let handle = CreateMutexW(None, false, PCWSTR::from_raw(name_wide.as_ptr())).unwrap();
            (handle, GetLastError() == ERROR_ALREADY_EXISTS)
        };

        let (first, first_exists) = create();
        assert!(!first_exists, "首个互斥量为新建");
        let (second, second_exists) = create();
        assert!(second_exists, "同进程再次创建应看到已有实例");

        unsafe {
            let _ = CloseHandle(second);
            let _ = CloseHandle(first);
        }
        let (third, third_exists) = create();
        assert!(!third_exists, "首实例退出(句柄关闭)后应能重新成为唯一实例");
        unsafe {
            let _ = CloseHandle(third);
        }
    }

    #[test]
    fn forwarded_launch_reaches_the_handler() {
        let app_id = format!("app.cropmark.window-{}", std::process::id());
        let received = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&received);
        let acquired = super::super::acquire(
            &app_id,
            Arc::new(move |launch| {
                *seen.lock().unwrap() = Some(launch);
            }),
        )
        .expect("首实例获取成功");
        let super::super::Acquire::Primary(primary) = acquired else {
            panic!("首个 acquire 不应是转发路径");
        };
        assert!(!received.lock().unwrap().is_some(), "转发前不应触发回调");

        let payload = SecondLaunch {
            args: vec!["--forwarded".into()],
            cwd: Some("C:\\tmp".into()),
        };
        assert!(send_payload(&app_id, &payload.to_bytes()));
        pump_thread_messages();

        assert_eq!(
            received.lock().unwrap().clone(),
            Some(payload),
            "第二个实例的参数应送达回调"
        );
        drop(primary);
    }

    /// 只有消息泵转起来,WM_APP 才会到达回调;这里模拟主线程消息循环。
    fn pump_thread_messages() {
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() } {
            unsafe {
                DispatchMessageW(&message);
            }
        }
    }
}
