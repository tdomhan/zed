#![cfg(target_os = "android")]
mod android;
mod sbix;

pub use android::dispatcher::AndroidQueueReceiver;
pub use android::ffi::{
    create_platform, display_scale, keyboard_height, system_inset_bottom, system_inset_top,
    with_platform,
};
pub use android::platform::{AndroidCommon, AndroidPlatform};
pub use android::window::{AndroidWindow, AndroidWindowState};
