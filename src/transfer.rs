use crate::drives::{Volume, identify, is_drive_root};
use crate::transfer_log::{self, FileRecord, LocalTime};
use std::{
    ffi::OsStr,
    fs::{self, FileTimes, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        mpsc,
    },
    thread,
    time::Instant,
};
use tempfile::NamedTempFile;
use xxhash_rust::xxh64::Xxh64;

const CHUNK: usize = 2 * 1024 * 1024;
const BUFFERED_CHUNKS: usize = 2;

#[derive(Clone)]
pub struct Location {
    pub path: PathBuf,
    pub volume: Volume,
}

impl Location {
    pub fn new(path: &Path) -> io::Result<Self> {
        let path = fs::canonicalize(path)?;
        let volume = identify(&path);
        Ok(Self { path, volume })
    }

    pub(crate) fn check(&self) -> io::Result<()> {
        if fs::canonicalize(&self.path)? != self.path || identify(&self.path).id != self.volume.id {
            return Err(io::Error::other(format!(
                "Location changed: {}",
                self.path.display()
            )));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct Job {
    pub source: Location,
    pub destinations: Vec<Location>,
    pub folder: String,
    pub verify: bool,
    pub detect_duplicates: bool,
}

impl Job {
    pub fn output_roots(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.destinations
            .iter()
            .map(|destination| destination.path.join(&self.folder))
    }
}

#[derive(Default)]
pub struct Progress {
    pub canceled: AtomicBool,
    pub total: AtomicU64,
    pub read: AtomicU64,
    pub committed: AtomicU64,
    pub reused: AtomicU64,
    pub detail: Mutex<String>,
}

impl Progress {
    fn check(&self) -> io::Result<()> {
        if self.canceled.load(Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Canceled by user",
            ));
        }
        Ok(())
    }

    fn detail(&self, text: String) {
        *self.detail.lock().unwrap() = text;
    }
}

struct Entry {
    relative: PathBuf,
    size: Option<u64>,
}

pub fn valid_folder(name: &str) -> bool {
    let reserved = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    !name.is_empty()
        && name.encode_utf16().count() <= 255
        && !name.ends_with([' ', '.'])
        && !name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        && !matches!(
            reserved.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        && !["COM", "LPT"].iter().any(|prefix| {
            reserved.strip_prefix(prefix).is_some_and(|n| {
                matches!(
                    n,
                    "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        })
}

fn same_or_within(path: &Path, base: &Path) -> bool {
    let mut parts = path.components();
    base.components().all(|part| {
        parts.next().is_some_and(|other| {
            other.as_os_str().to_string_lossy().to_lowercase()
                == part.as_os_str().to_string_lossy().to_lowercase()
        })
    })
}

pub(crate) fn paths_overlap(left: &Path, right: &Path) -> bool {
    same_or_within(left, right) || same_or_within(right, left)
}

fn ensure_directory(path: &Path, reuse: bool) -> io::Result<bool> {
    match fs::create_dir(path) {
        Ok(()) => Ok(true),
        Err(error) if reuse && error.kind() == io::ErrorKind::AlreadyExists => {
            if check_entry(path)?.is_dir() {
                Ok(false)
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

fn context<T>(result: io::Result<T>, path: &Path) -> io::Result<T> {
    result.map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

fn check_entry(path: &Path) -> io::Result<Metadata> {
    let meta = context(fs::symlink_metadata(path), path)?;
    if is_link(&meta) {
        return Err(io::Error::other(format!(
            "Cannot copy a link or mount point: {}",
            path.display()
        )));
    }
    Ok(meta)
}

pub(crate) fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(target_os = "macos")]
    {
        metadata.file_type().is_symlink()
    }
}

fn open_source(path: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        // Windows can deny concurrent writes or deletion while this handle is open.
        options.share_mode(FILE_SHARE_READ);
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options.open(path)
}

fn scan(job: &Job, progress: &Progress, source: &Metadata) -> io::Result<Vec<Entry>> {
    let base = &job.source.path;
    if !source.is_dir() {
        return Ok(vec![Entry {
            relative: PathBuf::from(
                base.file_name()
                    .ok_or_else(|| io::Error::other("Missing file name"))?,
            ),
            size: Some(source.len()),
        }]);
    }

    let mut entries = Vec::new();
    let mut folders = vec![PathBuf::new()];
    let drive_root = is_drive_root(base);
    while let Some(folder) = folders.pop() {
        progress.check()?;
        let path = base.join(&folder);
        for entry in context(fs::read_dir(&path), &path)? {
            progress.check()?;
            let name = context(entry, &path)?.file_name();
            if folder.as_os_str().is_empty() && drive_root && is_volume_metadata(&name) {
                continue;
            }
            let relative = folder.join(name);
            let meta = check_entry(&base.join(&relative))?;
            #[cfg(target_os = "macos")]
            if !same_filesystem(source, &meta) {
                continue;
            }
            if meta.is_dir() {
                folders.push(relative.clone());
            } else if !meta.is_file() {
                return Err(io::Error::other(format!(
                    "Unsupported entry: {}",
                    base.join(&relative).display()
                )));
            }
            entries.push(Entry {
                relative,
                size: meta.is_file().then(|| meta.len()),
            });
        }
    }
    Ok(entries)
}

#[cfg(target_os = "macos")]
fn same_filesystem(left: &Metadata, right: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev()
}

fn is_volume_metadata(name: &OsStr) -> bool {
    [
        "System Volume Information",
        "$RECYCLE.BIN",
        ".Spotlight-V100",
        ".Trashes",
        ".fseventsd",
        ".DocumentRevisions-V100",
        ".TemporaryItems",
    ]
    .iter()
    .any(|system| name.to_string_lossy().eq_ignore_ascii_case(system))
}

pub fn run(job: &Job, progress: &Progress) -> io::Result<()> {
    let started = LocalTime::now();
    let watch = Instant::now();
    progress.check()?;
    job.source.check()?;
    if job.destinations.is_empty() || !valid_folder(&job.folder) {
        return Err(io::Error::other(
            "Choose destinations and enter a valid output folder name",
        ));
    }

    let source = check_entry(&job.source.path)?;
    let source_dir = source.is_dir();
    if job.verify && job.folder.eq_ignore_ascii_case(transfer_log::DIRECTORY) {
        return Err(io::Error::other(
            "Choose a backup folder name other than Transfer Logs",
        ));
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    for (destination, output) in job.destinations.iter().zip(job.output_roots()) {
        destination.check()?;
        #[cfg(windows)]
        let same_mount = true;
        #[cfg(target_os = "macos")]
        let same_mount = same_filesystem(
            &source,
            &context(fs::metadata(&destination.path), &destination.path)?,
        );
        if source_dir && same_mount && same_or_within(&output, &job.source.path) {
            return Err(io::Error::other(format!(
                "Output is inside the source: {}",
                output.display()
            )));
        }
        if !source_dir
            && job
                .source
                .path
                .file_name()
                .is_some_and(|name| paths_overlap(&output.join(name), &job.source.path))
        {
            return Err(io::Error::other("Output file is the source file"));
        }
        if job.verify
            && source_dir
            && same_mount
            && same_or_within(
                &destination.path.join(transfer_log::DIRECTORY),
                &job.source.path,
            )
        {
            return Err(io::Error::other("Transfer Logs would be inside the source"));
        }
        if roots.iter().any(|root| paths_overlap(&output, root)) {
            return Err(io::Error::other(format!(
                "Outputs overlap: {}",
                output.display()
            )));
        }
        roots.push(output);
    }

    progress.detail("Scanning source…".into());
    let entries = scan(job, progress, &source)?;
    progress
        .total
        .store(entries.iter().filter_map(|entry| entry.size).sum(), Relaxed);

    job.source.check()?;
    for destination in &job.destinations {
        destination.check()?;
    }
    progress.check()?;
    let mut created = Vec::new();
    for root in &roots {
        match context(ensure_directory(root, job.detect_duplicates), root) {
            Ok(true) => created.push(root),
            Ok(false) => {}
            Err(error) => {
                for path in created {
                    let _ = fs::remove_dir(path);
                }
                return Err(error);
            }
        }
    }

    let mut files = Vec::new();
    let outcome = (|| {
        for entry in entries {
            progress.check()?;
            let outputs: Vec<_> = roots
                .iter()
                .map(|root| root.join(&entry.relative))
                .collect();
            if let Some(size) = entry.size {
                let source = if source_dir {
                    job.source.path.join(&entry.relative)
                } else {
                    job.source.path.clone()
                };
                let result = context(copy_file(&source, size, &outputs, job, progress), &source)?;
                progress.committed.fetch_add(1, Relaxed);
                let reused = result.reused.iter().filter(|&&value| value).count() as u64;
                progress.reused.fetch_add(reused, Relaxed);
                if let Some(hash) = result.hash {
                    files.push(FileRecord {
                        relative: entry.relative,
                        bytes: size,
                        hash,
                        reused: result.reused,
                    });
                }
            } else {
                for output in outputs {
                    context(ensure_directory(&output, job.detect_duplicates), &output)?;
                }
            }
        }
        Ok(())
    })();
    if job.verify {
        progress.detail("Writing transfer logs…".into());
        if let Err(error) =
            transfer_log::write_reports(job, &files, &started, watch.elapsed(), &outcome)
        {
            return match outcome {
                Ok(()) => Err(io::Error::other(format!("Files verified. {error}"))),
                Err(copy_error) => Err(io::Error::new(
                    copy_error.kind(),
                    format!("{copy_error}. {error}"),
                )),
            };
        }
    }
    outcome
}

struct CopyResult {
    hash: Option<u64>,
    reused: Vec<bool>,
}

enum DestinationFile {
    New(NamedTempFile),
    Existing {
        file: fs::File,
        modified: std::time::SystemTime,
    },
}

fn file_conflict() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "A file with different contents already exists. Move it or choose another backup folder before retrying.",
    )
}

fn prepare_destination(
    output: &Path,
    size: u64,
    detect_duplicates: bool,
) -> io::Result<DestinationFile> {
    if detect_duplicates {
        match fs::symlink_metadata(output) {
            Ok(metadata) => {
                if !metadata.is_file() || is_link(&metadata) {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "A non-file entry is already at this destination path",
                    ));
                }
                let file = open_source(output)?;
                let metadata = file.metadata()?;
                if !metadata.is_file() || metadata.len() != size {
                    return Err(file_conflict());
                }
                return Ok(DestinationFile::Existing {
                    file,
                    modified: metadata.modified()?,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let parent = output.parent().expect("output has a parent");
    tempfile::Builder::new()
        .prefix(".multicopy-")
        .suffix(".partial")
        .tempfile_in(parent)
        .map(DestinationFile::New)
}

fn process_destination(
    mut destination: DestinationFile,
    chunks: mpsc::Receiver<Arc<Vec<u8>>>,
    modified: std::time::SystemTime,
    progress: &Progress,
) -> io::Result<DestinationFile> {
    match &mut destination {
        DestinationFile::New(file) => {
            for chunk in chunks {
                progress.check()?;
                file.write_all(&chunk)?;
            }
            progress.check()?;
            file.as_file()
                .set_times(FileTimes::new().set_modified(modified))?;
            file.as_file().sync_all()?;
        }
        DestinationFile::Existing { file, .. } => {
            let mut buffer = Vec::new();
            for chunk in chunks {
                progress.check()?;
                buffer.resize(chunk.len(), 0);
                file.read_exact(&mut buffer)?;
                if buffer.as_slice() != chunk.as_slice() {
                    return Err(file_conflict());
                }
            }
            progress.check()?;
        }
    }
    Ok(destination)
}

fn check_existing(
    file: &fs::File,
    path: &Path,
    size: u64,
    modified: std::time::SystemTime,
) -> io::Result<()> {
    let opened = file.metadata()?;
    let named = check_entry(path)?;
    let changed = !named.is_file()
        || opened.len() != size
        || named.len() != size
        || opened.modified()? != modified
        || named.modified()? != modified;
    #[cfg(target_os = "macos")]
    let changed = {
        use std::os::unix::fs::MetadataExt;
        changed || opened.dev() != named.dev() || opened.ino() != named.ino()
    };
    if changed {
        return Err(io::Error::other(
            "Existing destination file changed during comparison",
        ));
    }
    Ok(())
}

fn copy_file(
    source: &Path,
    size: u64,
    outputs: &[PathBuf],
    job: &Job,
    progress: &Progress,
) -> io::Result<CopyResult> {
    progress.detail(format!(
        "Copying {}",
        source.file_name().unwrap_or_default().to_string_lossy()
    ));
    if !check_entry(source)?.is_file() {
        return Err(io::Error::other("Source is no longer a file"));
    }
    let mut input = open_source(source)?;
    let metadata = input.metadata()?;
    if !metadata.is_file() || metadata.len() != size {
        return Err(io::Error::other("Source file changed after scanning"));
    }
    let modified = metadata.modified()?;
    let destinations: Vec<_> = outputs
        .iter()
        .map(|path| context(prepare_destination(path, size, job.detect_duplicates), path))
        .collect::<io::Result<_>>()?;
    if destinations
        .iter()
        .any(|file| matches!(file, DestinationFile::Existing { .. }))
    {
        progress.detail(format!(
            "Checking existing copies of {}",
            source.file_name().unwrap_or_default().to_string_lossy()
        ));
    }

    let (destinations, hash) = thread::scope(|scope| {
        let mut senders = Vec::new();
        let mut writers = Vec::new();
        for (destination, output) in destinations.into_iter().zip(outputs) {
            let (tx, rx) = mpsc::sync_channel::<Arc<Vec<u8>>>(BUFFERED_CHUNKS);
            senders.push(tx);
            writers.push(scope.spawn(move || {
                context(
                    process_destination(destination, rx, modified, progress),
                    output,
                )
            }));
        }

        let mut hash = job.verify.then(|| Xxh64::new(0));
        let read_result = (|| -> io::Result<()> {
            let mut remaining = size;
            while remaining != 0 {
                progress.check()?;
                let mut chunk = vec![0; remaining.min(CHUNK as u64) as usize];
                input.read_exact(&mut chunk)?;
                if let Some(hash) = &mut hash {
                    hash.update(&chunk);
                }
                let count = chunk.len() as u64;
                let chunk = Arc::new(chunk);
                for sender in &senders {
                    sender
                        .send(Arc::clone(&chunk))
                        .map_err(|_| io::Error::other("Destination writer stopped"))?;
                }
                remaining -= count;
                progress.read.fetch_add(count, Relaxed);
            }
            Ok(())
        })();
        drop(senders);

        let mut completed = Vec::new();
        let mut writer_error = None;
        for writer in writers {
            match writer.join() {
                Ok(Ok(file)) => completed.push(file),
                Ok(Err(error)) => {
                    writer_error.get_or_insert(error);
                }
                Err(_) => {
                    writer_error.get_or_insert_with(|| {
                        io::Error::other("Destination writer terminated unexpectedly")
                    });
                }
            }
        }
        if let Some(error) = writer_error {
            return Err(error);
        }
        read_result?;
        Ok((completed, hash.map(|value| value.digest())))
    })?;

    if let Some(expected) = hash {
        progress.detail(format!(
            "Verifying {}",
            source.file_name().unwrap_or_default().to_string_lossy()
        ));
        let mut buffer = vec![0; CHUNK];
        for (destination, output) in destinations.iter().zip(outputs) {
            if let DestinationFile::New(file) = destination {
                context(verify_file(file, &mut buffer, expected, progress), output)?;
            }
        }
    }
    for (destination, output) in destinations.iter().zip(outputs) {
        if let DestinationFile::Existing { file, modified } = destination {
            context(check_existing(file, output, size, *modified), output)?;
        }
    }
    let current = input.metadata()?;
    if current.len() != size || current.modified()? != modified {
        return Err(io::Error::other("Source file changed during the transfer"));
    }
    progress.check()?;
    let mut reused = Vec::with_capacity(outputs.len());
    for (destination, output) in destinations.into_iter().zip(outputs) {
        if let DestinationFile::New(file) = destination {
            context(
                file.persist_noclobber(output).map_err(|error| error.error),
                output,
            )?;
            reused.push(false);
        } else {
            reused.push(true);
        }
    }
    Ok(CopyResult { hash, reused })
}

fn verify_file(
    file: &NamedTempFile,
    buffer: &mut [u8],
    expected: u64,
    progress: &Progress,
) -> io::Result<()> {
    let mut input = file.reopen()?;
    let mut hash = Xxh64::new(0);
    loop {
        progress.check()?;
        let bytes = input.read(buffer)?;
        if bytes == 0 {
            break;
        }
        hash.update(&buffer[..bytes]);
    }
    if hash.digest() != expected {
        return Err(io::Error::other("Checksum mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Job, Vec<u8>) {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("camera");
        fs::create_dir_all(source.join("DCIM/Empty")).unwrap();
        let contents: Vec<_> = (0..CHUNK * 2 + 31)
            .map(|index| (index % 251) as u8)
            .collect();
        fs::write(source.join("DCIM/clip.bin"), &contents).unwrap();
        fs::write(source.join("DCIM/zero"), []).unwrap();

        let destinations = ["one", "two", "three"]
            .iter()
            .map(|name| {
                let path = tmp.path().join(name);
                fs::create_dir(&path).unwrap();
                Location::new(&path).unwrap()
            })
            .collect();
        let job = Job {
            source: Location::new(&source).unwrap(),
            destinations,
            folder: "Card A".into(),
            verify: true,
            detect_duplicates: true,
        };
        (tmp, job, contents)
    }

    #[test]
    fn copies_one_source_stream_to_three_verified_destinations() {
        let (_tmp, job, contents) = fixture();
        let progress = Progress::default();
        run(&job, &progress).unwrap();
        assert_eq!(progress.read.load(Relaxed), contents.len() as u64);
        assert_eq!(progress.committed.load(Relaxed), 2);
        for destination in &job.destinations {
            let folder = destination.path.join(&job.folder).join("DCIM");
            assert_eq!(fs::read(folder.join("clip.bin")).unwrap(), contents);
            assert!(fs::read(folder.join("zero")).unwrap().is_empty());
            assert!(folder.join("Empty").is_dir());
            assert_eq!(fs::read_dir(folder).unwrap().count(), 3);
            let mut logs = fs::read_dir(destination.path.join(transfer_log::DIRECTORY)).unwrap();
            let log = logs.next().unwrap().unwrap().path();
            assert!(logs.next().is_none());
            let filename = log.file_name().unwrap().to_string_lossy();
            assert!(filename.starts_with("MultiCopy Transfer Log "));
            assert!(filename.ends_with(".txt"));
            let report = fs::read_to_string(log).unwrap().replace("\r\n", "\n");
            assert!(report.starts_with("MultiCopy(1.0.0) - "));
            assert!(report.contains("Created with ChatGPT"));
            let output = crate::display_path(&destination.path.join(&job.folder));
            assert!(
                report
                    .lines()
                    .any(|line| line.starts_with("Destination: ") && line.ends_with(&output))
            );
            assert!(report.contains("Total Files Transferred: 2\n"));
            assert!(report.contains(&format!("({} Bytes)", contents.len())));
            assert!(
                report.contains("Hash type: XXH64BE\nVerification mode: Source\n\nStatus: Success")
            );
            let empty_file = Path::new("DCIM").join("zero");
            assert!(report.contains(&format!(
                "Path: {}\nSize: 0 bytes\nHash:  ef46db3751d8e999\nStatus: verified",
                empty_file.display()
            )));
            assert_eq!(
                fs::metadata(destination.path.join(&job.folder).join("DCIM/clip.bin"))
                    .unwrap()
                    .modified()
                    .unwrap(),
                fs::metadata(job.source.path.join("DCIM/clip.bin"))
                    .unwrap()
                    .modified()
                    .unwrap()
            );
        }
    }

    #[test]
    fn existing_backup_is_preserved_and_empty_claims_are_released() {
        let (_tmp, mut job, _) = fixture();
        job.detect_duplicates = false;
        let existing = job.destinations[1].path.join(&job.folder);
        fs::create_dir(&existing).unwrap();
        fs::write(existing.join("original"), b"keep").unwrap();
        let progress = Progress::default();
        assert_eq!(
            run(&job, &progress).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(existing.join("original")).unwrap(), b"keep");
        assert!(!job.destinations[0].path.join(&job.folder).exists());
        assert!(!job.destinations[2].path.join(&job.folder).exists());
        assert_eq!(progress.read.load(Relaxed), 0);
    }

    #[test]
    fn canceled_job_creates_no_output() {
        let (_tmp, job, _) = fixture();
        let progress = Progress::default();
        progress.canceled.store(true, Relaxed);
        assert_eq!(
            run(&job, &progress).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(
            job.destinations
                .iter()
                .all(|destination| !destination.path.join(&job.folder).exists())
        );
    }

    #[test]
    fn output_cannot_be_inside_source() {
        let (_tmp, mut job, _) = fixture();
        job.destinations = vec![Location::new(&job.source.path.join("DCIM")).unwrap()];
        assert!(
            run(&job, &Progress::default())
                .unwrap_err()
                .to_string()
                .contains("inside the source")
        );
        assert!(!job.destinations[0].path.join(&job.folder).exists());
    }

    #[test]
    fn single_file_copy_can_disable_verification() {
        let (_tmp, mut job, contents) = fixture();
        job.source = Location::new(&job.source.path.join("DCIM/clip.bin")).unwrap();
        job.verify = false;
        let progress = Progress::default();
        run(&job, &progress).unwrap();
        assert_eq!(progress.committed.load(Relaxed), 1);
        for destination in &job.destinations {
            assert!(!destination.path.join(transfer_log::DIRECTORY).exists());
            assert_eq!(
                fs::read(destination.path.join(&job.folder).join("clip.bin")).unwrap(),
                contents
            );
        }
    }

    #[test]
    fn resumes_each_destination_and_preserves_matching_files() {
        let (_tmp, job, contents) = fixture();
        let first = job.destinations[0].path.join(&job.folder).join("DCIM");
        fs::create_dir_all(first.join("Empty")).unwrap();
        let existing = first.join("clip.bin");
        fs::write(&existing, &contents).unwrap();
        let fixed_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        OpenOptions::new()
            .write(true)
            .open(&existing)
            .unwrap()
            .set_times(FileTimes::new().set_modified(fixed_time))
            .unwrap();
        let modified = fs::metadata(&existing).unwrap().modified().unwrap();

        let progress = Progress::default();
        run(&job, &progress).unwrap();
        assert_eq!(progress.read.load(Relaxed), contents.len() as u64);
        assert_eq!(progress.reused.load(Relaxed), 1);
        assert_eq!(
            fs::metadata(&existing).unwrap().modified().unwrap(),
            modified
        );
        for (index, destination) in job.destinations.iter().enumerate() {
            assert_eq!(
                fs::read(destination.path.join(&job.folder).join("DCIM/clip.bin")).unwrap(),
                contents
            );
            let log = fs::read_dir(destination.path.join(transfer_log::DIRECTORY))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let report = fs::read_to_string(log).unwrap();
            if index == 0 {
                assert!(report.contains("Total Files Transferred: 1"));
                assert!(report.contains("-- Existing files matched (1) --"));
            } else {
                assert!(report.contains("Total Files Transferred: 2"));
                assert!(!report.contains("Existing files matched"));
            }
        }
        let retry = Progress::default();
        run(&job, &retry).unwrap();
        assert_eq!(retry.reused.load(Relaxed), 6);
        assert_eq!(retry.read.load(Relaxed), contents.len() as u64);
    }

    #[test]
    fn rejects_a_same_size_conflict_even_without_verification() {
        let (_tmp, mut job, mut contents) = fixture();
        job.verify = false;
        job.source = Location::new(&job.source.path.join("DCIM/clip.bin")).unwrap();
        let folder = job.destinations[1].path.join(&job.folder);
        fs::create_dir(&folder).unwrap();
        *contents.last_mut().unwrap() ^= 1;
        let existing = folder.join("clip.bin");
        fs::write(&existing, &contents).unwrap();

        let error = run(&job, &Progress::default()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("different contents"));
        assert_eq!(fs::read(&existing).unwrap(), contents);
        for destination in [&job.destinations[0], &job.destinations[2]] {
            assert_eq!(
                fs::read_dir(destination.path.join(&job.folder))
                    .unwrap()
                    .count(),
                0
            );
        }
    }

    #[test]
    fn verification_detects_mismatched_contents() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"changed").unwrap();
        let mut buffer = [0; 1024];
        let result = verify_file(
            &file,
            &mut buffer,
            xxhash_rust::xxh64::xxh64(b"original", 0),
            &Progress::default(),
        );
        assert_eq!(result.unwrap_err().to_string(), "Checksum mismatch");
    }
}
