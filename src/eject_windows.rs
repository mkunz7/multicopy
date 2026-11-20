use crate::drives::{disk_number, disk_number_wide, identify};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io,
    os::windows::ffi::OsStringExt,
    path::Path,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Devices::DeviceAndDriverInstallation::{
        CM_DEVCAP_REMOVABLE, CM_DRP_CAPABILITIES, CM_Get_DevNode_Registry_PropertyW, CM_Get_Parent,
        CM_Request_Device_EjectW, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
        SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
        SetupDiGetDeviceInterfaceDetailW,
    },
    Foundation::{ERROR_NO_MORE_ITEMS, INVALID_HANDLE_VALUE},
    System::{Ioctl::GUID_DEVINTERFACE_DISK, SystemInformation::GetSystemWindowsDirectoryW},
};

use super::{Report, Request};
struct Device {
    disk: Option<u32>,
    ancestry: Vec<u32>,
}

struct DeviceList(HDEVINFO);

impl Drop for DeviceList {
    fn drop(&mut self) {
        unsafe {
            SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

pub fn run(requests: Vec<Request>) -> Report {
    let mut report = Report::default();
    let mut targets = BTreeMap::new();
    for request in requests {
        if let Some(disk) = request
            .volume_id
            .starts_with(r"\\?\volume{")
            .then(|| disk_number(&request.volume_id))
            .flatten()
        {
            targets.entry(disk).or_insert(request);
        } else {
            report.skipped.push(format!(
                "{} — unavailable or not a local disk",
                request.label
            ));
        }
    }
    if targets.is_empty() {
        return report;
    }
    let Some(system) = windows_disk() else {
        report
            .skipped
            .push("Could not identify the Windows system disk. Ejection stopped.".into());
        return report;
    };
    let devices = match devices() {
        Ok(devices) => devices,
        Err(error) => {
            report
                .skipped
                .push(format!("Could not enumerate storage devices: {error}"));
            return report;
        }
    };

    for (&disk, request) in &targets {
        if report.ejected.contains(&disk) {
            continue;
        }
        let result = (|| -> Result<(), String> {
            let node = removable_node(disk, system, &devices, &targets)?;
            if disk_number(&request.volume_id) != Some(disk) {
                return Err("drive disconnected or changed".into());
            }
            request_eject(node)?;
            report.ejected.extend(
                devices
                    .iter()
                    .filter(|device| device.ancestry.contains(&node))
                    .filter_map(|device| device.disk),
            );
            Ok(())
        })();
        if let Err(reason) = result {
            report.skipped.push(format!("{} — {reason}", request.label));
        }
    }
    report
}

fn removable_node(
    disk: u32,
    system: u32,
    devices: &[Device],
    targets: &BTreeMap<u32, Request>,
) -> Result<u32, String> {
    if disk == system {
        return Err("Windows system disk".into());
    }
    let device = devices
        .iter()
        .find(|device| device.disk == Some(disk))
        .ok_or("Windows could not identify the device")?;
    let node = device
        .ancestry
        .iter()
        .copied()
        .find(|node| is_removable(*node))
        .ok_or("Windows marks this device as fixed")?;

    if devices.iter().any(|device| {
        device.ancestry.contains(&node)
            && device
                .disk
                .is_none_or(|disk| disk == system || !targets.contains_key(&disk))
    }) {
        return Err("the device shares hardware with a disk outside this eject request".into());
    }
    Ok(node)
}

fn windows_disk() -> Option<u32> {
    let mut buffer = [0u16; 261];
    let count =
        unsafe { GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if count == 0 || count >= buffer.len() {
        return None;
    }
    identify(Path::new(&OsString::from_wide(&buffer[..count]))).disk
}

fn devices() -> io::Result<Vec<Device>> {
    let list = DeviceList(unsafe {
        SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_DISK,
            null(),
            null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    });
    if list.0 == INVALID_HANDLE_VALUE as HDEVINFO {
        return Err(io::Error::last_os_error());
    }
    let mut devices = Vec::new();
    for index in 0.. {
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..unsafe { std::mem::zeroed() }
        };
        if unsafe {
            SetupDiEnumDeviceInterfaces(
                list.0,
                null(),
                &GUID_DEVINTERFACE_DISK,
                index,
                &mut interface,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_NO_MORE_ITEMS as i32) {
                Ok(devices)
            } else {
                Err(error)
            };
        }

        let mut bytes = 0;
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                list.0,
                &interface,
                null_mut(),
                0,
                &mut bytes,
                null_mut(),
            );
        }
        if (bytes as usize) < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() {
            return Err(io::Error::last_os_error());
        }
        let mut data = vec![0u32; (bytes as usize).div_ceil(size_of::<u32>())];
        let detail = data
            .as_mut_ptr()
            .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        let mut info = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..unsafe { std::mem::zeroed() }
        };
        let path = unsafe {
            // SetupAPI writes a variable-length, NUL-terminated UTF-16 path into this buffer.
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(
                list.0,
                &interface,
                detail,
                bytes,
                null_mut(),
                &mut info,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let offset = std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
            std::slice::from_raw_parts(
                std::ptr::addr_of!((*detail).DevicePath).cast::<u16>(),
                (bytes as usize - offset) / 2,
            )
        };
        let mut ancestry = vec![info.DevInst];
        let mut parent = 0;
        while unsafe { CM_Get_Parent(&mut parent, *ancestry.last().unwrap(), 0) } == CR_SUCCESS {
            ancestry.push(parent);
        }
        devices.push(Device {
            disk: disk_number_wide(path),
            ancestry,
        });
    }
    unreachable!()
}

fn is_removable(node: u32) -> bool {
    let mut capabilities = 0u32;
    let mut bytes = size_of::<u32>() as u32;
    let status = unsafe {
        CM_Get_DevNode_Registry_PropertyW(
            node,
            CM_DRP_CAPABILITIES,
            null_mut(),
            (&mut capabilities as *mut u32).cast(),
            &mut bytes,
            0,
        )
    };
    status == CR_SUCCESS && capabilities & CM_DEVCAP_REMOVABLE != 0
}

fn request_eject(node: u32) -> Result<(), String> {
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        PNP_VetoDevice as VETO_DEVICE, PNP_VetoDriver as VETO_DRIVER,
        PNP_VetoInsufficientRights as VETO_INSUFFICIENT_RIGHTS,
        PNP_VetoOutstandingOpen as VETO_OUTSTANDING_OPEN,
        PNP_VetoPendingClose as VETO_PENDING_CLOSE, PNP_VetoWindowsApp as VETO_WINDOWS_APP,
        PNP_VetoWindowsService as VETO_WINDOWS_SERVICE,
    };
    let mut veto = 0;
    let mut name = [0u16; 260];
    let status = unsafe {
        CM_Request_Device_EjectW(node, &mut veto, name.as_mut_ptr(), name.len() as u32, 0)
    };
    if status == CR_SUCCESS {
        return Ok(());
    }
    let reason = match veto {
        VETO_OUTSTANDING_OPEN | VETO_PENDING_CLOSE | VETO_WINDOWS_APP | VETO_WINDOWS_SERVICE => {
            "device is in use"
        }
        VETO_INSUFFICIENT_RIGHTS => "Windows denied permission",
        VETO_DEVICE | VETO_DRIVER => "device or driver refused removal",
        _ => "Windows refused removal",
    };
    Err(format!("{reason} (code {status})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_windows_volume_to_a_disk_and_protects_it() {
        let system = windows_disk().expect("Windows volume should identify its disk");
        let devices = devices().unwrap();
        assert!(devices.iter().any(|device| device.disk == Some(system)));
        assert_eq!(
            removable_node(system, system, &devices, &BTreeMap::new()).unwrap_err(),
            "Windows system disk"
        );
    }
}
