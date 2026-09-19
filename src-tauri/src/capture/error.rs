use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureError {
    pub kind: CaptureErrorKind,
    pub message: String,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CaptureErrorKind {
    Permission,
    Api,
    InvalidBuffer,
    Unavailable,
    Cancelled,
}

impl CaptureError {
    pub fn permission(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            kind: CaptureErrorKind::Permission,
            message: message.into(),
            hint: Some(hint.into()),
        }
    }

    pub fn api(message: impl Into<String>) -> Self {
        Self {
            kind: CaptureErrorKind::Api,
            message: message.into(),
            hint: None,
        }
    }

    pub fn invalid_buffer(reason: &str) -> Self {
        Self {
            kind: CaptureErrorKind::InvalidBuffer,
            message: format!("截屏缓冲无效：{reason}。"),
            hint: None,
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: CaptureErrorKind::Unavailable,
            message: message.into(),
            hint: None,
        }
    }

    pub fn cancelled() -> Self {
        Self {
            kind: CaptureErrorKind::Cancelled,
            message: String::new(),
            hint: None,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.kind == CaptureErrorKind::Cancelled
    }

    pub fn user_message(&self) -> String {
        match &self.hint {
            Some(hint) if !hint.is_empty() => format!("{}\n{hint}", self.message),
            _ => self.message.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformFailure {
    #[cfg_attr(windows, allow(dead_code))]
    PermissionDenied,
    Api(String),
    #[allow(dead_code)]
    NoInterface(String),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    BufferEmpty,
    BufferZeroSize,
    BufferUninitialized,
}

pub fn classify_platform_failure(failure: PlatformFailure) -> CaptureError {
    match failure {
        PlatformFailure::PermissionDenied => CaptureError::permission(
            "没有截屏权限，未能截取。",
            permission_hint(),
        ),
        PlatformFailure::Api(detail) => {
            if detail.is_empty() {
                CaptureError::api("截屏接口调用失败。")
            } else {
                CaptureError::api(format!("截屏接口调用失败：{detail}"))
            }
        }
        PlatformFailure::NoInterface(detail) => CaptureError::unavailable(detail),
        PlatformFailure::BufferEmpty => CaptureError::invalid_buffer("空缓冲"),
        PlatformFailure::BufferZeroSize => CaptureError::invalid_buffer("尺寸为 0"),
        PlatformFailure::BufferUninitialized => CaptureError::invalid_buffer("未初始化"),
    }
}

pub fn permission_hint() -> String {
    #[cfg(target_os = "macos")]
    let hint = "请在系统设置 › 隐私与安全性 › 屏幕录制中打开 Cropmark。打开后必须从菜单栏图标完全退出再打开，权限才会生效。若开关已打开仍弹出授权，先点减号移除 Cropmark，完全退出后再截取。";
    #[cfg(windows)]
    let hint = "请在 Windows 设置 › 隐私和安全性 › 屏幕截图和屏幕录制中允许 Cropmark，然后重新截取。";
    #[cfg(target_os = "linux")]
    let hint = "请在系统门户提示中允许截屏，并确认已安装 xdg-desktop-portal。";
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    let hint = "当前系统没有可用的截屏接口。";
    hint.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_and_api_are_failures_not_success() {
        let permission = classify_platform_failure(PlatformFailure::PermissionDenied);
        assert_eq!(permission.kind, CaptureErrorKind::Permission);
        assert!(permission.message.contains("权限"));
        let api = classify_platform_failure(PlatformFailure::Api("BitBlt".into()));
        assert_eq!(api.kind, CaptureErrorKind::Api);
        assert!(api.message.contains("接口"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_permission_hint_requires_full_quit() {
        let hint = permission_hint();
        assert!(hint.contains("屏幕录制"));
        assert!(hint.contains("菜单栏"));
        assert!(hint.contains("退出"));
    }

    #[test]
    fn buffer_failures_do_not_mention_black_pixels() {
        for failure in [
            PlatformFailure::BufferEmpty,
            PlatformFailure::BufferZeroSize,
            PlatformFailure::BufferUninitialized,
        ] {
            let error = classify_platform_failure(failure);
            assert_eq!(error.kind, CaptureErrorKind::InvalidBuffer);
            assert!(!error.message.contains("黑"));
            assert!(!error.message.to_ascii_lowercase().contains("black"));
        }
    }
}
