use crate::{VERSION, display_path, transfer::Job};
use std::{
    fmt::Write as _,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};
pub const DIRECTORY: &str = "Transfer Logs";
const DIVIDER: &str = "------------------------------------------------------------------";

pub struct FileRecord {
    pub relative: PathBuf,
    pub bytes: u64,
    pub hash: u64,
    pub reused: Vec<bool>,
}

pub struct LocalTime(chrono::NaiveDateTime);

impl LocalTime {
    pub fn now() -> Self {
        Self(chrono::Local::now().naive_local())
    }

    fn filename(&self) -> String {
        self.0.format("%Y-%m-%d %H-%M-%S").to_string()
    }
}

impl std::fmt::Display for LocalTime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.format("%-m/%-d/%Y %-I:%M:%S %p").fmt(formatter)
    }
}

pub fn os_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(read_os_version)
}

#[cfg(target_os = "macos")]
fn read_os_version() -> String {
    use std::process::Command;
    let read = |flag| {
        let output = Command::new("/usr/bin/sw_vers").arg(flag).output().ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    match (read("-productVersion"), read("-buildVersion")) {
        (Some(version), Some(build)) => format!("macOS {version} ({build})"),
        (Some(version), None) => format!("macOS {version}"),
        _ => "macOS".into(),
    }
}

#[cfg(windows)]
fn read_os_version() -> String {
    use windows_sys::{
        Wdk::System::SystemServices::RtlGetVersion,
        Win32::System::SystemInformation::OSVERSIONINFOW,
    };
    let mut version: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    version.dwOSVersionInfoSize = size_of::<OSVERSIONINFOW>() as u32;
    if unsafe { RtlGetVersion(&mut version) } != 0 {
        return "Windows".into();
    }
    let build = version.dwBuildNumber;
    match (version.dwMajorVersion, version.dwMinorVersion) {
        (10, 0) if build >= 22000 => format!("Windows 11 ({build})"),
        (10, 0) => format!("Windows 10 ({build})"),
        (major, minor) => format!("Windows {major}.{minor} ({build})"),
    }
}

pub fn write_reports(
    job: &Job,
    files: &[FileRecord],
    started: &LocalTime,
    elapsed: Duration,
    outcome: &io::Result<()>,
) -> io::Result<()> {
    let finished = LocalTime::now();
    let mut failures = Vec::new();
    for (index, destination) in job.destinations.iter().enumerate() {
        let directory = destination.path.join(DIRECTORY);
        let result = destination.check().and_then(|()| {
            let target = display_path(&destination.path.join(&job.folder));
            let label = crate::drives::volume_label(&destination.volume);
            let target = if label.is_empty() {
                target
            } else {
                format!("{label} {target}")
            };
            let report = render(
                job, &target, files, index, started, &finished, elapsed, outcome,
            );
            write_file(&directory, &finished, &report)
        });
        if let Err(error) = result {
            failures.push(format!("{}: {error}", display_path(&directory)));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "Could not write transfer log: {}",
            failures.join("; ")
        )))
    }
}

fn render(
    job: &Job,
    destination: &str,
    files: &[FileRecord],
    destination_index: usize,
    started: &LocalTime,
    finished: &LocalTime,
    elapsed: Duration,
    outcome: &io::Result<()>,
) -> String {
    let source = display_path(&job.source.path);
    let name = job
        .source
        .path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| source.as_str().into());
    let (duplicates, transferred): (Vec<_>, Vec<_>) = files
        .iter()
        .partition(|file| file.reused[destination_index]);
    let bytes: u64 = transferred.iter().map(|file| file.bytes).sum();
    let amount = if bytes >= 1024 * 1024 {
        format!("{} MB ", bytes / (1024 * 1024))
    } else if bytes >= 1024 {
        format!("{} KB ", bytes / 1024)
    } else {
        String::new()
    };
    let status = match outcome {
        Ok(()) => "Success".to_owned(),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => format!("Canceled — {error}"),
        Err(error) => format!("Failed — {error}"),
    };
    let mut text = format!(
        "MultiCopy({VERSION}) - {}\n\n{DIVIDER}\n\nCreated with ChatGPT\n\n{DIVIDER}\n\n\
         Source:  {source}\nDestination: {destination}\n\n\
         Source Name: {name}\nStarted: {started}\nFinished: {finished}\nDuration: {:.1} seconds\n\n\
         Total Files Transferred: {}\nTotal Size: {amount}({bytes} Bytes)\n\
         Hash type: XXH64BE\nVerification mode: Source\n\nStatus: {status}\n\n-- Transferred files --\n",
        os_version(),
        elapsed.as_secs_f64(),
        transferred.len(),
    );
    write_entries(&mut text, &transferred, "verified");
    if !duplicates.is_empty() {
        let _ = writeln!(
            text,
            "\n-- Existing files matched ({}) --",
            duplicates.len()
        );
        write_entries(&mut text, &duplicates, "matched, skipped");
    }
    text
}

fn write_entries(text: &mut String, files: &[&FileRecord], status: &str) {
    for (index, file) in files.iter().enumerate() {
        let name = file
            .relative
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let _ = write!(
            text,
            "\n#{}: {name}\nPath: {}\nSize: {} bytes\nHash:  {:016x}\nStatus: {status}\n",
            index + 1,
            file.relative.display(),
            file.bytes,
            file.hash,
        );
    }
}

fn write_file(directory: &Path, finished: &LocalTime, text: &str) -> io::Result<()> {
    match fs::create_dir(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let info = fs::symlink_metadata(directory)?;
    if !info.is_dir() || crate::transfer::is_link(&info) {
        return Err(io::Error::other(
            "Transfer Logs must be a regular directory",
        ));
    }
    let mut staged = tempfile::NamedTempFile::new_in(directory)?;
    staged.write_all(text.replace('\n', "\r\n").as_bytes())?;
    staged.as_file().sync_all()?;
    let base = format!("MultiCopy Transfer Log {}", finished.filename());
    // Parallel transfers can finish at the same second.
    for count in 1.. {
        let filename = if count == 1 {
            format!("{base}.txt")
        } else {
            format!("{base} ({count}).txt")
        };
        match staged.persist_noclobber(directory.join(filename)) {
            Ok(_) => return Ok(()),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => staged = error.file,
            Err(error) => return Err(error.error),
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_logs_with_matching_timestamps() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join(DIRECTORY);
        let finished = LocalTime(
            chrono::NaiveDate::from_ymd_opt(2026, 11, 20)
                .unwrap()
                .and_hms_opt(1, 3, 20)
                .unwrap(),
        );
        assert_eq!(finished.to_string(), "11/20/2026 1:03:20 AM");
        write_file(&directory, &finished, "first\n").unwrap();
        write_file(&directory, &finished, "second\n").unwrap();
        assert_eq!(
            fs::read_to_string(directory.join("MultiCopy Transfer Log 2026-11-20 01-03-20.txt"))
                .unwrap(),
            "first\r\n",
        );
        assert_eq!(
            fs::read_to_string(
                directory.join("MultiCopy Transfer Log 2026-11-20 01-03-20 (2).txt")
            )
            .unwrap(),
            "second\r\n",
        );
    }
}
