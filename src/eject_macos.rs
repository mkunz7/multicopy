use super::{Report, Request};
use crate::drives::{disk_info, mounted_disks, resolve_volume};
use plist::Value;
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    process::Command,
};

pub fn run(requests: Vec<Request>) -> Report {
    let mut report = Report::default();
    let mut targets = BTreeMap::<u32, Vec<Request>>::new();
    for request in requests {
        if let Some((disk, _)) = resolve_volume(&request.volume_id) {
            targets.entry(disk).or_default().push(request);
        } else {
            report.skipped.push(format!(
                "{} — unavailable or not an identifiable local disk",
                request.label
            ));
        }
    }
    if targets.is_empty() {
        return report;
    }
    let mounts = match mounted_disks() {
        Ok(mounts) => mounts,
        Err(error) => {
            report
                .skipped
                .push(format!("Could not inspect mounted disks: {error}"));
            return report;
        }
    };
    let Some(system) = mounts
        .iter()
        .find(|mount| mount.mount == Path::new("/"))
        .and_then(|mount| mount.volume.disk)
        .or_else(|| {
            mounts
                .iter()
                .find(|mount| mount.mount == Path::new("/System/Volumes/Data"))
                .and_then(|mount| mount.volume.disk)
        })
    else {
        report
            .skipped
            .push("Could not identify the macOS startup disk. Ejection stopped.".into());
        return report;
    };
    for (disk, requests) in targets {
        let device = format!("disk{disk}");
        let selected: HashSet<_> = requests
            .iter()
            .map(|request| request.volume_id.as_str())
            .collect();
        let result = (|| -> Result<(), String> {
            if disk == system {
                return Err("macOS startup disk".into());
            }
            let info = disk_info(&device).ok_or("could not identify the physical disk")?;
            let flag = |key| info.get(key).and_then(Value::as_boolean);
            let external = flag("Internal") == Some(false);
            let removable = flag("RemovableMedia") == Some(true) || flag("Removable") == Some(true);
            if flag("WholeDisk") != Some(true) || (!external && !removable) {
                return Err("macOS marks this disk as fixed".into());
            }
            if mounts.iter().any(|mount| {
                mount.volume.disk == Some(disk) && !selected.contains(mount.volume.id.as_str())
            }) {
                return Err("the disk has a mounted volume outside this eject request".into());
            }
            if requests.iter().any(|request| {
                resolve_volume(&request.volume_id).is_none_or(|(current, _)| current != disk)
            }) {
                return Err("drive disconnected or changed".into());
            }
            let output = Command::new("/usr/sbin/diskutil")
                .args(["eject", &device])
                .output()
                .map_err(|error| error.to_string())?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let reason = if stderr.trim().is_empty() {
                    stdout.trim()
                } else {
                    stderr.trim()
                };
                return Err(if reason.is_empty() {
                    "macOS refused removal"
                } else {
                    reason
                }
                .to_owned());
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                report.ejected.insert(disk);
            }
            Err(error) => report
                .skipped
                .push(format!("{} — {error}", requests[0].label)),
        }
    }
    report
}
