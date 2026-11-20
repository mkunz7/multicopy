use std::path::PathBuf;

#[cfg_attr(windows, path = "drives_windows.rs")]
#[cfg_attr(target_os = "macos", path = "drives_macos.rs")]
mod platform;

#[cfg(target_os = "macos")]
pub(crate) use platform::{disk_info, mounted_disks, resolve_volume};
#[cfg(windows)]
pub(crate) use platform::{disk_number, disk_number_wide};
pub use platform::{identify, is_drive_root, list, volume_label};
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Volume {
    pub id: String,
    pub lane: String,
    pub disk: Option<u32>,
}

pub struct Drive {
    pub path: PathBuf,
    pub label: String,
    pub free: Option<u64>,
}
