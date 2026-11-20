use std::collections::HashSet;

#[cfg_attr(windows, path = "eject_windows.rs")]
#[cfg_attr(target_os = "macos", path = "eject_macos.rs")]
mod platform;

pub use platform::run;
pub struct Request {
    pub volume_id: String,
    pub label: String,
}

#[derive(Default)]
pub struct Report {
    pub ejected: HashSet<u32>,
    pub skipped: Vec<String>,
}
