use super::{Drive, Volume};
use plist::{Dictionary, Value};
use std::{
    collections::HashSet,
    ffi::{CStr, CString, OsString},
    io,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::Command,
    ptr,
};

const DISKUTIL: &str = "/usr/sbin/diskutil";

pub(crate) struct MountedDisk {
    pub mount: PathBuf,
    pub volume: Volume,
}

fn stat(path: &Path) -> Option<libc::statfs> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut result = std::mem::MaybeUninit::uninit();
    // statfs initializes its output structure on success.
    unsafe { (libc::statfs(path.as_ptr(), result.as_mut_ptr()) == 0).then(|| result.assume_init()) }
}

fn mounts() -> io::Result<Vec<libc::statfs>> {
    loop {
        let count = unsafe { libc::getfsstat(ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let capacity = count as usize + 16;
        let mut entries = Vec::<libc::statfs>::with_capacity(capacity);
        let bytes =
            i32::try_from(capacity * size_of::<libc::statfs>()).map_err(io::Error::other)?;
        let written = unsafe { libc::getfsstat(entries.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        if (written as usize) < capacity {
            // The kernel wrote this many initialized entries.
            unsafe { entries.set_len(written as usize) };
            return Ok(entries);
        }
    }
}

fn mount_path(info: &libc::statfs) -> PathBuf {
    let name = unsafe { CStr::from_ptr(info.f_mntonname.as_ptr()) };
    PathBuf::from(OsString::from_vec(name.to_bytes().to_vec()))
}

fn mounted_from(info: &libc::statfs) -> &str {
    unsafe { CStr::from_ptr(info.f_mntfromname.as_ptr()) }
        .to_str()
        .unwrap_or_default()
}

pub fn is_drive_root(path: &Path) -> bool {
    path == Path::new("/")
        || (path.parent() == Some(Path::new("/Volumes"))
            && stat(path).is_some_and(|info| mount_path(&info) == path))
}

pub fn list() -> Vec<Drive> {
    let mut drives: Vec<_> = mounts()
        .unwrap_or_default()
        .iter()
        .filter_map(|info| {
            let path = mount_path(info);
            if path != Path::new("/") && !path.starts_with("/Volumes") {
                return None;
            }
            let label = if path == Path::new("/") {
                "Startup disk".into()
            } else {
                path.file_name()?.to_string_lossy().into_owned()
            };
            let free = info.f_bavail.saturating_mul(u64::from(info.f_bsize));
            Some(Drive {
                path,
                label,
                free: Some(free),
            })
        })
        .collect();
    drives.sort_by(|left, right| left.path.cmp(&right.path));
    drives
}

pub(crate) fn mounted_disks() -> io::Result<Vec<MountedDisk>> {
    Ok(mounts()?
        .iter()
        .filter(|mount| device_number(mounted_from(mount)).is_some())
        .map(|info| MountedDisk {
            mount: mount_path(info),
            volume: mounted_volume(info),
        })
        .collect())
}

pub fn identify(path: &Path) -> Volume {
    if let Some(info) = stat(path) {
        return mounted_volume(&info);
    }
    fallback(format!("unavailable:{}", path.display()))
}

fn fallback(id: String) -> Volume {
    Volume {
        lane: id.clone(),
        id,
        disk: None,
    }
}

fn mounted_volume(info: &libc::statfs) -> Volume {
    let device = mounted_from(info);
    if device_number(device).is_none() {
        return fallback(format!("mount:{}|{device}", mount_path(info).display()));
    }
    let Some(details) = disk_info(device) else {
        return fallback(device.into());
    };
    let disk = physical_disk(&details);
    let id = volume_id(device, &details);
    let lane = disk.map_or_else(
        || {
            field(&details, "APFSContainerReference")
                .unwrap_or(device)
                .to_owned()
        },
        |disk| format!("disk{disk}"),
    );
    Volume { id, lane, disk }
}

pub fn volume_label(volume: &Volume) -> String {
    resolve_volume(&volume.id).map_or_else(String::new, |(_, info)| {
        field(&info, "VolumeName").unwrap_or_default().to_owned()
    })
}

pub(crate) fn resolve_volume(id: &str) -> Option<(u32, Dictionary)> {
    let (device, _) = id.split_once('|')?;
    let info = disk_info(device)?;
    if volume_id(device, &info) != id {
        return None;
    }
    Some((physical_disk(&info)?, info))
}

fn volume_id(device: &str, info: &Dictionary) -> String {
    ["VolumeUUID", "DiskUUID", "APFSVolumeUUID"]
        .into_iter()
        .filter_map(|key| field(info, key))
        .find(|value| !value.is_empty())
        .map_or_else(|| device.to_owned(), |uuid| format!("{device}|{uuid}"))
}

pub(crate) fn disk_info(device: &str) -> Option<Dictionary> {
    device_number(device)?;
    let output = Command::new(DISKUTIL)
        .args(["info", "-plist", device])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Value::from_reader_xml(output.stdout.as_slice())
        .ok()?
        .into_dictionary()
}

fn field<'a>(info: &'a Dictionary, key: &str) -> Option<&'a str> {
    info.get(key)?.as_string()
}

fn device_number(device: &str) -> Option<u32> {
    let suffix = device
        .strip_prefix("/dev/")
        .unwrap_or(device)
        .strip_prefix("disk")?;
    let mut parts = suffix.split('s');
    let disk = parts.next()?.parse().ok()?;
    parts
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some(disk)
}

fn physical_disk(info: &Dictionary) -> Option<u32> {
    if let Some(container) = field(info, "APFSContainerReference") {
        if let Some(disk) = apfs_disk(info) {
            return Some(disk);
        }
        return disk_info(container).and_then(|info| apfs_disk(&info));
    }
    if field(info, "VirtualOrPhysical") == Some("Virtual") {
        return None;
    }
    field(info, "ParentWholeDisk")
        .or_else(|| field(info, "DeviceIdentifier"))
        .and_then(device_number)
}

fn apfs_disk(info: &Dictionary) -> Option<u32> {
    let stores: HashSet<_> = info
        .get("APFSPhysicalStores")?
        .as_array()?
        .iter()
        .map(|store| {
            let store = store.as_dictionary()?;
            device_number(field(store, "APFSPhysicalStore")?)
        })
        .collect::<Option<_>>()?;
    if stores.len() == 1 {
        stores.into_iter().next()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_apfs_partitions_to_one_physical_disk() {
        let value = Value::from_reader_xml(
            br#"<?xml version="1.0"?>
        <plist version="1.0"><dict>
        <key>APFSContainerReference</key><string>disk7</string>
        <key>APFSPhysicalStores</key><array><dict>
        <key>APFSPhysicalStore</key><string>disk4s2</string>
        </dict></array></dict></plist>"#
                .as_slice(),
        )
        .unwrap();
        assert_eq!(physical_disk(value.as_dictionary().unwrap()), Some(4));
        assert_eq!(device_number("/dev/disk3s1s1"), Some(3));
        assert_eq!(device_number("-force"), None);
    }
}
