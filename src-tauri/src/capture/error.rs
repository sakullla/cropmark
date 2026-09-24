use serde::Serialize;

use crate::i18n;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureError {
    pub kind: CaptureErrorKind,
    /// 生成时按当时的界面语言解析后的文案;键与参数保留,供语言切换后重解析。
    pub message: String,
    pub hint: Option<String>,
    #[serde(skip)]
    template: Option<Box<ErrorTemplate>>,
}

/// 词条键 + 参数 + 提示键:语言切换后据此重新解析 message/hint。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ErrorTemplate {
    key: String,
    params: Vec<(String, String)>,
    hint_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CaptureErrorKind {
    Permission,
    Api,
    InvalidBuffer,
    Unavailable,
    /// 平台组件在有界等待内未响应(ADR-17):明确报错而不是永久挂起,可重试。
    Timeout,
    Cancelled,
}

impl CaptureError {
    fn from_parts(
        kind: CaptureErrorKind,
        key: &str,
        params: &[(&str, &str)],
        hint_key: Option<&str>,
    ) -> Self {
        Self {
            kind,
            message: i18n::tp(key, params),
            hint: hint_key.map(i18n::t),
            template: Some(Box::new(ErrorTemplate {
                key: key.to_string(),
                params: params
                    .iter()
                    .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                    .collect(),
                hint_key: hint_key.map(str::to_string),
            })),
        }
    }

    /// 按当前语言重新解析(语言切换后刷新已打开的错误窗);无键的历史消息原样返回。
    pub fn localized(&self) -> Self {
        let Some(template) = self.template.as_deref() else {
            return self.clone();
        };
        let params: Vec<(&str, &str)> = template
            .params
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        Self::from_parts(
            self.kind,
            &template.key,
            &params,
            template.hint_key.as_deref(),
        )
    }

    pub fn permission(key: &str, hint_key: &str) -> Self {
        Self::from_parts(CaptureErrorKind::Permission, key, &[], Some(hint_key))
    }

    pub fn api(key: &str) -> Self {
        Self::from_parts(CaptureErrorKind::Api, key, &[], None)
    }

    /// 带系统原始原因的参数化接口错误(`{detail}` 保留原样,不做翻译)。
    pub fn api_detail(key: &str, detail: &str) -> Self {
        Self::from_parts(CaptureErrorKind::Api, key, &[("detail", detail)], None)
    }

    /// 缓冲类错误的原因本身也是词条键,生成时解析为当前语言。
    pub fn invalid_buffer(reason_key: &str) -> Self {
        let reason = i18n::t(reason_key);
        Self::from_parts(
            CaptureErrorKind::InvalidBuffer,
            "error.capture.invalid_buffer",
            &[("reason", &reason)],
            None,
        )
    }

    pub fn unavailable(key: &str) -> Self {
        Self::from_parts(CaptureErrorKind::Unavailable, key, &[], None)
    }

    /// 有界等待超时(ADR-17):message/hint 都走词条,语言切换后可重解析。
    pub fn timeout(key: &str, hint_key: &str) -> Self {
        Self::from_parts(CaptureErrorKind::Timeout, key, &[], Some(hint_key))
    }

    /// 平台层返回的原始系统文案(如 C 层错误串):系统级文案不纳入词条覆盖,
    /// 按原样展示;保持无模板,语言切换时原样保留。
    pub fn platform_message(message: String) -> Self {
        Self {
            kind: CaptureErrorKind::Unavailable,
            message,
            hint: None,
            template: None,
        }
    }

    pub fn cancelled() -> Self {
        Self {
            kind: CaptureErrorKind::Cancelled,
            message: String::new(),
            hint: None,
            template: None,
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
        PlatformFailure::PermissionDenied => {
            CaptureError::permission("error.capture.no_permission", permission_hint_key())
        }
        PlatformFailure::Api(detail) => {
            if detail.is_empty() {
                CaptureError::api("error.capture.api")
            } else {
                CaptureError::api_detail("error.capture.api_detail", &detail)
            }
        }
        PlatformFailure::NoInterface(_detail) => {
            CaptureError::unavailable("error.capture.no_interface")
        }
        PlatformFailure::BufferEmpty => CaptureError::invalid_buffer("error.capture.buffer_empty"),
        PlatformFailure::BufferZeroSize => {
            CaptureError::invalid_buffer("error.capture.buffer_zero_size")
        }
        PlatformFailure::BufferUninitialized => {
            CaptureError::invalid_buffer("error.capture.buffer_uninitialized")
        }
    }
}

pub fn permission_hint_key() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "error.capture.permission_hint_macos"
    }
    #[cfg(windows)]
    {
        "error.capture.permission_hint_windows"
    }
    #[cfg(target_os = "linux")]
    {
        "error.capture.permission_hint_linux"
    }
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    {
        "error.capture.permission_hint_other"
    }
}

#[cfg(test)]
pub fn permission_hint() -> String {
    i18n::t(permission_hint_key())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_and_api_are_failures_not_success() {
        let permission = classify_platform_failure(PlatformFailure::PermissionDenied);
        assert_eq!(permission.kind, CaptureErrorKind::Permission);
        assert!(permission.message.contains("权限"));
        let hint = permission
            .hint
            .as_deref()
            .expect("permission errors include a platform hint");
        assert_eq!(hint, permission_hint());
        assert!(!hint.is_empty());
        assert!(permission.user_message().contains(hint));

        let api = classify_platform_failure(PlatformFailure::Api("BitBlt".into()));
        assert_eq!(api.kind, CaptureErrorKind::Api);
        assert!(api.message.contains("接口"));
        assert!(api.message.contains("BitBlt"));
        assert!(api.hint.is_none());
        assert!(!api.user_message().contains("权限"));
        assert!(!api.user_message().contains("屏幕录制"));
        assert!(!api.user_message().contains("屏幕截图"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_permission_hint_requires_full_quit() {
        let hint = permission_hint();
        assert!(hint.contains("屏幕录制"));
        assert!(hint.contains("菜单栏"));
        assert!(hint.contains("退出"));
        let error = classify_platform_failure(PlatformFailure::PermissionDenied);
        let user = error.user_message();
        assert!(user.contains("屏幕录制"));
        assert!(user.contains("菜单栏"));
        assert!(user.contains("退出"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_permission_hint_points_to_screenshot_settings() {
        let hint = permission_hint();
        assert!(hint.contains("屏幕截图"));
        assert!(hint.contains("屏幕录制"));
        let error = classify_platform_failure(PlatformFailure::PermissionDenied);
        assert!(error.user_message().contains("屏幕截图"));
        assert!(error.user_message().contains("屏幕录制"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_permission_hint_points_to_portal() {
        // R13:门户未安装或被拒时沿用明确文案:说明去向并给出安装建议。
        let hint = permission_hint();
        assert!(hint.contains("门户"));
        assert!(hint.contains("xdg-desktop-portal"));
        let error = classify_platform_failure(PlatformFailure::PermissionDenied);
        let user = error.user_message();
        assert!(user.contains("权限"));
        assert!(user.contains("xdg-desktop-portal"));
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

    #[test]
    fn timeout_errors_keep_kind_message_and_hint() {
        let error =
            CaptureError::timeout("error.capture.sck_timeout", "error.capture.timeout_hint");
        assert_eq!(error.kind, CaptureErrorKind::Timeout);
        assert!(error.message.contains("超时"), "message={}", error.message);
        let hint = error
            .hint
            .as_deref()
            .expect("timeout errors include a hint");
        assert!(hint.contains("重试"), "hint={hint}");
        assert!(error.user_message().contains(hint));

        // 语言切换后按模板重解析:kind 与模板都必须保留。
        let localized = error.localized();
        assert_eq!(localized.kind, CaptureErrorKind::Timeout);
        assert!(localized.message.contains("超时"));
        assert!(localized.hint.is_some());

        // 序列化面给前端:kind 名为 timeout。
        let json = serde_json::to_string(&error).expect("CaptureError serializes");
        assert!(json.contains("\"kind\":\"timeout\""), "json={json}");
    }
}
