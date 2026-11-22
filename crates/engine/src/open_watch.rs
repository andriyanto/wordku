//! Guards around reading a document file (`file.open`, `file.newFromTemplate`) on the desktop.
//!
//! - **A time limit.** The file is read on a thread of its own with a large stack, and the
//!   command waits at most [`OPEN_LIMIT`] for it. A file that keeps the reader busy longer (damaged
//!   or built to make it loop) fails with an error the app shows, instead of freezing every open
//!   window. The thread can't be stopped from outside: it is left to finish on its own, and what
//!   it reads is dropped.
//! - **A record of the file being read.** While it is read, a marker naming it is kept in the
//!   app's settings ([`enable`]), locked by this process. A crash that takes the whole app down
//!   while reading leaves the marker behind with its lock released; the next start finds it
//!   ([`interrupted`]) and the app says which file it was, so the person doesn't open it again
//!   unawares. Unsaved documents are in File › Recover, as after any crash.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// How long a document may take to read before the open fails.
pub const OPEN_LIMIT: Duration = Duration::from_secs(60);

/// The reader's stack: room for deeply nested documents, so they fail with an error rather than
/// overflow it (which ends the process).
const STACK: usize = 64 << 20;

/// Begins every error [`read_within`] gives for a read that ran out of time: how the app tells it
/// from other failures ([`is_time_limit`]).
const TIMED_OUT: &str = "Reading stopped after";

/// Where the markers go: set once by the desktop app.
static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Keep markers of the files being read in `dir` (the app's settings). Without it (tests,
/// headless use) nothing is recorded.
pub fn enable(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// Most markers looked at on start: more can only be junk.
const MAX_MARKERS: usize = 64;
/// Longest file name a marker keeps, in bytes.
const MAX_NAME: u64 = 4096;
/// A marker younger than this is being written by a starting read, not left by a crash.
const SETTLING: Duration = Duration::from_secs(2);

/// The files whose reading a crash interrupted, from earlier runs (of this or any instance of
/// the app): their markers are unlocked. Each is reported once: its marker is removed.
pub fn interrupted() -> Vec<String> {
    let Some(dir) = DIR.get() else { return Vec::new() };
    interrupted_in(dir)
}

fn interrupted_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut found = Vec::new();
    for entry in entries.flatten().take(MAX_MARKERS) {
        let path = entry.path();
        let is_marker = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("opening-") && n.ends_with(".txt"));
        if !is_marker {
            continue;
        }
        let young = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_none_or(|age| age < SETTLING);
        if young {
            continue;
        }
        let Ok(mut file) = File::open(&path) else { continue };
        // Locked: another running instance is reading that file now.
        if file.try_lock().is_err() {
            continue;
        }
        let mut name = String::new();
        let _ = Read::by_ref(&mut file).take(MAX_NAME).read_to_string(&mut name);
        drop(file);
        let _ = fs::remove_file(&path);
        let name = name.trim();
        if !name.is_empty() {
            found.push(name.to_string());
        }
    }
    found
}

/// A marker naming the file being read, removed when the read ends (unless the process does).
pub(crate) struct Reading {
    path: Option<PathBuf>,
    file: Option<File>,
}

impl Reading {
    /// Record that `name` is being read, where [`enable`] says. Never fails the open: without a
    /// marker the read just isn't recorded.
    pub(crate) fn start(name: &str) -> Reading {
        let Some(dir) = DIR.get() else { return Reading { path: None, file: None } };
        Self::start_in(dir, name)
    }

    fn start_in(dir: &Path, name: &str) -> Reading {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = dir.join(format!("opening-{}-{}.txt", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let file = fs::create_dir_all(dir).and_then(|()| File::create(&path)).and_then(|mut f| {
            f.try_lock().map_err(std::io::Error::other)?;
            f.write_all(name.as_bytes())?;
            f.flush()?;
            Ok(f)
        });
        match file {
            Ok(f) => Reading { path: Some(path), file: Some(f) },
            Err(e) => {
                log::debug!("open marker not written ({e})");
                let _ = fs::remove_file(&path);
                Reading { path: None, file: None }
            }
        }
    }
}

impl Drop for Reading {
    fn drop(&mut self) {
        // Closed (and so unlocked) before removing: Windows doesn't remove an open file.
        drop(self.file.take());
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

/// Run `read` on a thread of its own with a large stack, and wait at most `limit` for it. A read
/// that panics fails with the panic's message; one still running after `limit` fails with an
/// error naming `name` and is left to finish on its own ([`is_time_limit`] tells that error).
pub(crate) fn read_within<T: Send + 'static>(
    name: &str,
    limit: Duration,
    read: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = mpsc::sync_channel(1);
    // The folders the command may read (automation roots) hold on the reader's thread too.
    let read = crate::file_access::carried(read);
    let handle = std::thread::Builder::new()
        .name("wordcraft-open".into())
        .stack_size(STACK)
        .spawn(move || {
            let _ = tx.send(read());
        })
        .map_err(|e| format!("couldn't start reading the file: {e}"))?;
    match rx.recv_timeout(limit) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            log::warn!("reading {name} took longer than {} s; stopped waiting", limit.as_secs());
            Err(format!(
                "{TIMED_OUT} {} seconds: WordKu couldn't finish reading {}. The file may be damaged, or too complex to open.",
                limit.as_secs(),
                file_name(name)
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let why = match handle.join() {
                Err(panic) => panic.downcast_ref::<&str>().map(|s| (*s).to_string()).or_else(|| panic.downcast_ref::<String>().cloned()),
                Ok(()) => None,
            };
            Err(format!("WordKu couldn't read {}: {}", file_name(name), why.unwrap_or_else(|| "the reader stopped".into())))
        }
    }
}

/// Whether `error` is the one [`read_within`] gives for a read that ran out of time.
pub fn is_time_limit(error: &str) -> bool {
    error.starts_with(TIMED_OUT)
}

fn file_name(name: &str) -> String {
    Path::new(name).file_name().map_or_else(|| name.to_string(), |n| n.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wc-open-watch-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_read_that_takes_too_long_fails_and_the_app_can_tell() {
        let slow = read_within("/docs/Loop.docx", Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_millis(400));
            Ok(1)
        });
        let error = slow.unwrap_err();
        assert!(is_time_limit(&error), "{error}");
        assert!(error.contains("Loop.docx") && !error.contains("/docs/"), "{error}");
        assert_eq!(read_within("a.docx", Duration::from_secs(5), || Ok(7)), Ok(7));
        assert_eq!(read_within("a.docx", Duration::from_secs(5), || Err::<u8, _>("bad zip".into())), Err("bad zip".into()));
        assert!(!is_time_limit("bad zip"));
    }

    #[test]
    fn a_read_that_panics_fails_with_its_message() {
        let error = read_within::<u8>("b.docx", Duration::from_secs(5), || panic!("index out of bounds")).unwrap_err();
        assert!(error.contains("b.docx") && error.contains("index out of bounds") && !is_time_limit(&error), "{error}");
    }

    #[test]
    fn the_reader_has_room_for_deep_nesting() {
        // Deeper than the default 2 MB stack of a spawned thread allows.
        fn depth(n: u32) -> u32 {
            let pad = [n as u8; 512];
            if n == 0 { u32::from(pad[0]) } else { depth(n - 1) + u32::from(std::hint::black_box(pad)[1] & 1) }
        }
        assert!(read_within("deep.docx", Duration::from_secs(30), || Ok(depth(20_000))).is_ok());
    }

    #[test]
    fn a_crash_while_reading_leaves_a_marker_the_next_start_reports_once() {
        let dir = scratch("crash");
        let reading = Reading::start_in(&dir, "C:\\Docs\\Broken.docx");
        let marker = reading.path.clone().unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(60);
        File::options().write(true).open(&marker).unwrap().set_modified(old).unwrap();
        // Still locked by the read going on: not reported, however old.
        assert!(interrupted_in(&dir).is_empty());
        assert!(marker.exists());
        // A crash: the lock goes with the process, the marker stays.
        let mut gone = reading;
        drop(gone.file.take());
        gone.path = None;
        drop(gone);
        assert!(marker.exists());
        assert_eq!(interrupted_in(&dir), vec!["C:\\Docs\\Broken.docx".to_string()]);
        assert!(interrupted_in(&dir).is_empty(), "reported once");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_read_that_ends_leaves_nothing_behind() {
        let dir = scratch("clean");
        // A marker too new to be from a crash: a read that is only starting.
        let starting = Reading::start_in(&dir, "new.docx");
        let mut left = starting;
        drop(left.file.take());
        let young = left.path.take().unwrap();
        assert!(interrupted_in(&dir).is_empty());
        fs::remove_file(young).unwrap();
        {
            let reading = Reading::start_in(&dir, "a.docx");
            assert!(reading.path.as_ref().is_some_and(|p| p.exists()));
        }
        assert_eq!(fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0), 0);
        // Unwritable folder: the read goes on unrecorded.
        let blocked = dir.join("file-not-folder");
        fs::write(&blocked, b"x").unwrap();
        let reading = Reading::start_in(&blocked, "a.docx");
        assert!(reading.path.is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
