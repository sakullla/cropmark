//! 单实例运行(R1,ADR-1)。
//!
//! 重复启动只保留首实例:第二次启动在把启动参数转发给首实例后立即退出,
//! 不创建托盘、窗口,也不注册热键。首实例通过平台原语判定唯一性——
//! Windows 为命名互斥量 + 隐藏消息窗,Unix 为 flock 锁文件 + Unix socket;
//! 实例标识只由应用标识派生、不含版本号,跨版本升级仍互斥。

#[cfg(unix)]
mod unix;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(target_os = "windows")]
use windows as platform;

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// 应用标识:与 `tauri.conf.json` 的 identifier 一致。仅用于派生实例名,
/// 不含版本号(实例判定不随版本变化);由单测与配置文件对账。
pub const INSTANCE_ID: &str = "app.cropmark.desktop";

/// 第二实例转发给首实例的启动上下文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecondLaunch {
    /// 启动参数(不含可执行文件自身)。
    pub args: Vec<String>,
    /// 启动时的工作目录。
    pub cwd: Option<String>,
}

impl SecondLaunch {
    /// 采集当前进程的启动上下文;非 UTF-8 参数按替换字符降级,避免 panic。
    pub fn current() -> Self {
        Self {
            args: std::env::args_os()
                .skip(1)
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            cwd: std::env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().into_owned()),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// 首实例收到第二次启动时的回调。可能在非主线程被调用,
/// 实现需自行切回主线程后再操作窗口。
pub type LaunchHandler = Arc<dyn Fn(SecondLaunch) + Send + Sync + 'static>;

/// 单实例判定结果。
pub enum Acquire {
    /// 本进程是首实例;守卫必须存活到进程退出。
    Primary(Primary),
    /// 已有实例在运行:启动参数已尽力转发,调用方应立即退出。
    Forwarded,
}

// 首实例守卫(平台实现):持有互斥量/锁与消息接收端,drop 时释放。
#[cfg(unix)]
pub use unix::Primary;
#[cfg(target_os = "windows")]
pub use windows::Primary;

/// 实例名:仅由应用标识派生,不含版本号。
pub fn instance_key(app_id: &str) -> String {
    format!("cropmark-{app_id}")
}

/// 尝试成为首实例。已有实例时转发启动参数并返回 [`Acquire::Forwarded`];
/// 单实例机制本身不可用时返回 `Err`,由调用方决定是否继续启动。
pub fn acquire(app_id: &str, handler: LaunchHandler) -> Result<Acquire, String> {
    platform::acquire(app_id, handler)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_id_matches_tauri_config_identifier() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../../tauri.conf.json")).expect("tauri.conf.json");
        assert_eq!(config["identifier"], INSTANCE_ID);
    }

    #[test]
    fn instance_key_does_not_embed_version() {
        let key = instance_key(INSTANCE_ID);
        assert!(key.contains(INSTANCE_ID));
        assert!(!key.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn second_launch_round_trips_args_and_cwd() {
        let launch = SecondLaunch {
            args: vec!["--flag".into(), "带空格 参数".into()],
            cwd: Some("C:\\工作 目录".into()),
        };
        let bytes = launch.to_bytes();
        assert_eq!(SecondLaunch::from_bytes(&bytes), Some(launch));
    }

    #[test]
    fn second_launch_ignores_malformed_payload() {
        assert_eq!(SecondLaunch::from_bytes(b""), None);
        assert_eq!(SecondLaunch::from_bytes(b"{not json"), None);
        assert_eq!(SecondLaunch::from_bytes(&[0xff, 0xfe, 0x00]), None);
    }

    #[test]
    fn second_launch_accepts_missing_cwd() {
        let launch = SecondLaunch::from_bytes(br#"{"args":["a"]}"#).expect("args only");
        assert_eq!(launch.args, vec!["a"]);
        assert_eq!(launch.cwd, None);
    }
}
