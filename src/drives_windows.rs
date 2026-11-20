use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDiskFreeSpaceExW, GetDriveTypeW,
        GetLogicalDrives, GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
        GetVolumePathNameW, OPEN_EXISTING,
    },
    System::IO::DeviceIoControl,
};

use super::{Drive, Volume};
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn length(value: &[u16]) -> usize {
    value.iter().position(|c| *c == 0).unwrap_or(value.len())
}

pub fn volume_label(volume: &Volume) -> String {
    read_label(&wide(Path::new(&volume.id)))
}

fn read_label(root: &[u16]) -> String {
    let mut label = [0u16; 261];
    let found = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if found != 0 {
        String::from_utf16_lossy(&label[..length(&label)])
    } else {
        String::new()
    }
}

pub fn list() -> Vec<Drive> {
    let drives = unsafe { GetLogicalDrives() };
    (0..26)
        .filter(|index| drives & (1 << index) != 0)
        .filter_map(|index| {
            let path = PathBuf::from(format!("{}:\\", char::from(b'A' + index)));
            let root = wide(&path);
            // Removable, fixed and network drives.
            if !matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, 2..=4) {
                return None;
            }
            let mut bytes_available = 0;
            let label = read_label(&root);
            let free = unsafe {
                GetDiskFreeSpaceExW(
                    root.as_ptr(),
                    &mut bytes_available,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            } != 0;
            Some(Drive {
                path,
                label,
                free: free.then_some(bytes_available),
            })
        })
        .collect()
}

pub fn identify(path: &Path) -> Volume {
    let path_wide = wide(path);
    let mut root = vec![0u16; 32768];
    let found =
        unsafe { GetVolumePathNameW(path_wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) }
            != 0;
    if !found {
        let id = path
            .components()
            .next()
            .map(|p| p.as_os_str().to_string_lossy().to_lowercase())
            .unwrap_or_default();
        return Volume {
            lane: id.clone(),
            disk: None,
            id,
        };
    }

    let mut guid = [0u16; 50];
    let has_guid = unsafe {
        GetVolumeNameForVolumeMountPointW(root.as_ptr(), guid.as_mut_ptr(), guid.len() as u32)
    } != 0;
    let root_name = OsString::from_wide(&root[..length(&root)]);
    let id = if has_guid {
        String::from_utf16_lossy(&guid[..length(&guid)]).to_lowercase()
    } else {
        root_name.to_string_lossy().to_lowercase()
    };

    // Volume handles use the volume GUID without its trailing backslash.
    let disk = has_guid.then(|| disk_number(&id)).flatten();
    let lane = disk.map_or_else(|| id.clone(), |number| format!("Disk {number}"));
    Volume { id, lane, disk }
}

pub fn disk_number(volume: &str) -> Option<u32> {
    let address = wide(Path::new(volume.trim_end_matches('\\')));
    disk_number_wide(&address)
}

pub(crate) fn disk_number_wide(address: &[u16]) -> Option<u32> {
    use windows_sys::Win32::System::Ioctl::{
        IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER,
    };
    let handle = unsafe {
        CreateFileW(
            address.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut device: STORAGE_DEVICE_NUMBER = unsafe { std::mem::zeroed() };
    let size = size_of::<STORAGE_DEVICE_NUMBER>() as u32;
    let mut bytes = 0;
    let ok = unsafe {
        let result = DeviceIoControl(
            handle,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            std::ptr::null(),
            0,
            (&mut device as *mut STORAGE_DEVICE_NUMBER).cast(),
            size,
            &mut bytes,
            std::ptr::null_mut(),
        );
        CloseHandle(handle);
        result != 0 && bytes == size
    };
    const FILE_DEVICE_DISK: u32 = 7;
    (ok && device.DeviceType == FILE_DEVICE_DISK && device.DeviceNumber != u32::MAX)
        .then_some(device.DeviceNumber)
}
pub fn is_drive_root(path: &Path) -> bool {
    path.parent().is_none() && path.has_root()
}
