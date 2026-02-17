use std::{
    fs::{self, File},
    io::{self, BufRead, Read, Seek},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Utc};
use flate2::bufread::MultiGzDecoder;
use tracing::debug;
use vector_common::constants::GZIP_MAGIC;

use crate::{
    buffer::{read_until_with_max_size, ReadResult},
    metadata_ext::PortableFileExt,
    FilePosition, ReadFrom,
};
#[cfg(test)]
mod tests;

/// Wrapper to allow shared access to a File via Arc.
/// This enables sharing a single file descriptor between the BufReader (for reading)
/// and a separate handle (for metadata access), without duplicating the fd via dup().
struct SharedFile(Arc<File>);

impl Read for SharedFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (&*self.0).read(buf)
    }
}

impl Seek for SharedFile {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        (&*self.0).seek(pos)
    }
}

/// The `RawLine` struct is a thin wrapper around the bytes that have been read
/// in order to retain the context of where in the file they have been read from.
///
/// The offset field contains the byte offset of the beginning of the line within
/// the file that it was read from.
#[derive(Debug)]
pub(super) struct RawLine {
    pub offset: u64,
    pub bytes: Bytes,
}

#[derive(Debug)]
pub struct RawLineResult {
    pub raw_line: Option<RawLine>,
    pub discarded_for_size_and_truncated: Vec<BytesMut>,
}

/// Information about a file when it is unwatched.
/// Used for metric emission when Vector stops watching a file for any reason:
/// - File deleted
/// - File rotated and old file removed
/// - Inode changed (file replaced)
/// - `rotate_wait` timeout
#[derive(Debug, Clone)]
pub struct FileUnwatchInfo {
    /// The path of the file
    pub path: PathBuf,
    /// Number of bytes that were not read (dropped) from the file
    pub bytes_dropped: u64,
    /// Whether the file reached EOF before being unwatched
    pub reached_eof: bool,
}

/// The `FileWatcher` struct defines the polling based state machine which reads
/// from a file path, transparently updating the underlying file descriptor when
/// the file has been rolled over, as is common for logs.
///
/// The `FileWatcher` is expected to live for the lifetime of the file
/// path. `FileServer` is responsible for clearing away `FileWatchers` which no
/// longer exist.
pub struct FileWatcher {
    pub path: PathBuf,
    findable: bool,
    reader: Box<dyn BufRead>,
    /// Shared file handle for metadata access (e.g., current file size).
    /// Uses Arc to share the fd with the reader without duplicating it.
    /// None for gzipped files where we can't track accurate position.
    file_handle: Option<Arc<File>>,
    file_position: FilePosition,
    devno: u64,
    inode: u64,
    is_dead: bool,
    reached_eof: bool,
    last_read_attempt: Instant,
    last_read_success: Instant,
    last_seen: Instant,
    max_line_bytes: usize,
    line_delimiter: Bytes,
    buf: BytesMut,
    /// The file size when the watcher was created. Used as fallback for
    /// bytes dropped calculation when file_handle is unavailable.
    initial_file_size: u64,
}

impl FileWatcher {
    /// Create a new `FileWatcher`
    ///
    /// The input path will be used by `FileWatcher` to prime its state
    /// machine. A `FileWatcher` tracks _only one_ file. This function returns
    /// None if the path does not exist or is not readable by the current process.
    pub fn new(
        path: PathBuf,
        read_from: ReadFrom,
        ignore_before: Option<DateTime<Utc>>,
        max_line_bytes: usize,
        line_delimiter: Bytes,
    ) -> Result<FileWatcher, io::Error> {
        let f = Arc::new(fs::File::open(&path)?);
        let (devno, ino) = (f.portable_dev()?, f.portable_ino()?);
        let metadata = f.metadata()?;

        let shared_file = SharedFile(Arc::clone(&f));
        let mut reader = io::BufReader::new(shared_file);

        let too_old = if let (Some(ignore_before), Ok(modified_time)) = (
            ignore_before,
            metadata.modified().map(DateTime::<Utc>::from),
        ) {
            modified_time < ignore_before
        } else {
            false
        };

        let gzipped = is_gzipped(&mut reader)?;

        // Determine the actual position at which we should start reading.
        // For non-gzipped files, we keep the Arc<File> handle for metadata access.
        // For gzipped files, we don't track the handle since position tracking is not meaningful.
        let (reader, file_position, file_handle): (Box<dyn BufRead>, FilePosition, Option<Arc<File>>) =
            match (gzipped, too_old, read_from) {
                (true, true, _) => {
                    debug!(
                        message = "Not reading gzipped file older than `ignore_older`.",
                        ?path,
                    );
                    (Box::new(null_reader()), 0, None)
                }
                (true, _, ReadFrom::Checkpoint(file_position)) => {
                    debug!(
                        message = "Not re-reading gzipped file with existing stored offset.",
                        ?path,
                        %file_position
                    );
                    (Box::new(null_reader()), file_position, None)
                }
                // TODO: This may become the default, leading us to stop reading gzipped files that
                // we were reading before. Should we merge this and the next branch to read
                // compressed file from the beginning even when `read_from = "end"` (implicitly via
                // default or explicitly via config)?
                (true, _, ReadFrom::End) => {
                    debug!(
                        message = "Can't read from the end of already-compressed file.",
                        ?path,
                    );
                    (Box::new(null_reader()), 0, None)
                }
                (true, false, ReadFrom::Beginning) => {
                    (Box::new(io::BufReader::new(MultiGzDecoder::new(reader))), 0, None)
                }
                (false, true, _) => {
                    let pos = reader.seek(io::SeekFrom::End(0)).unwrap();
                    (Box::new(reader), pos, Some(f))
                }
                (false, false, ReadFrom::Checkpoint(file_position)) => {
                    let pos = reader.seek(io::SeekFrom::Start(file_position)).unwrap();
                    (Box::new(reader), pos, Some(f))
                }
                (false, false, ReadFrom::Beginning) => {
                    let pos = reader.seek(io::SeekFrom::Start(0)).unwrap();
                    (Box::new(reader), pos, Some(f))
                }
                (false, false, ReadFrom::End) => {
                    let pos = reader.seek(io::SeekFrom::End(0)).unwrap();
                    (Box::new(reader), pos, Some(f))
                }
            };

        let ts = metadata
            .modified()
            .ok()
            .and_then(|mtime| mtime.elapsed().ok())
            .and_then(|diff| Instant::now().checked_sub(diff))
            .unwrap_or_else(Instant::now);

        let initial_file_size = metadata.len();

        Ok(FileWatcher {
            path,
            findable: true,
            reader,
            file_handle,
            file_position,
            devno,
            inode: ino,
            is_dead: false,
            reached_eof: false,
            last_read_attempt: ts,
            last_read_success: ts,
            last_seen: ts,
            max_line_bytes,
            line_delimiter,
            buf: BytesMut::new(),
            initial_file_size,
        })
    }

    /// Update the path being watched.
    ///
    /// If the file at the new path has a different inode, this indicates the file
    /// was replaced (not just renamed). In this case, returns `FileUnwatchInfo`
    /// containing metrics about the old file so the caller can emit appropriate events.
    pub fn update_path(&mut self, path: PathBuf) -> io::Result<Option<FileUnwatchInfo>> {
        let new_file = Arc::new(File::open(&path)?);
        let unwatch_info = if (new_file.portable_dev()?, new_file.portable_ino()?) != (self.devno, self.inode) {
            // Capture metrics from the old file before switching
            let old_info = self.get_unwatch_info();

            let shared_file = SharedFile(Arc::clone(&new_file));
            let mut reader = io::BufReader::new(shared_file);
            let gzipped = is_gzipped(&mut reader)?;
            let (new_reader, new_file_handle): (Box<dyn BufRead>, Option<Arc<File>>) = if gzipped {
                if self.file_position != 0 {
                    (Box::new(null_reader()), None)
                } else {
                    (Box::new(io::BufReader::new(MultiGzDecoder::new(reader))), None)
                }
            } else {
                reader.seek(io::SeekFrom::Start(self.file_position))?;
                (Box::new(reader), Some(new_file))
            };
            self.reader = new_reader;
            self.file_handle = new_file_handle;
            self.devno = self.file_handle.as_ref().map(|f| f.portable_dev()).transpose()?.unwrap_or(0);
            self.inode = self.file_handle.as_ref().map(|f| f.portable_ino()).transpose()?.unwrap_or(0);
            // Reset initial_file_size for the new file
            self.initial_file_size = self.file_handle
                .as_ref()
                .and_then(|f| f.metadata().ok())
                .map(|m| m.len())
                .unwrap_or(0);

            Some(old_info)
        } else {
            None
        };
        self.path = path;
        Ok(unwatch_info)
    }

    pub fn set_file_findable(&mut self, f: bool) {
        self.findable = f;
        if f {
            self.last_seen = Instant::now();
        }
    }

    pub fn file_findable(&self) -> bool {
        self.findable
    }

    pub fn set_dead(&mut self) {
        self.is_dead = true;
    }

    pub fn dead(&self) -> bool {
        self.is_dead
    }

    pub fn get_file_position(&self) -> FilePosition {
        self.file_position
    }

    /// Returns the number of bytes that were not read (dropped).
    /// Uses the current file size from the file handle if available (works even after
    /// file deletion since the fd remains valid), falling back to initial_file_size.
    /// When the file reaches EOF, this will be 0. When the file is unwatched before EOF,
    /// this represents the bytes that were never read.
    pub fn get_bytes_dropped(&self) -> u64 {
        let current_size = self
            .file_handle
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map(|m| m.len())
            .unwrap_or(self.initial_file_size);

        current_size.saturating_sub(self.file_position)
    }

    /// Returns information about this file for metric emission when unwatching.
    /// This provides a consistent interface for all unwatch scenarios.
    pub fn get_unwatch_info(&self) -> FileUnwatchInfo {
        FileUnwatchInfo {
            path: self.path.clone(),
            bytes_dropped: self.get_bytes_dropped(),
            reached_eof: self.reached_eof,
        }
    }

    /// Read a single line from the underlying file
    ///
    /// This function will attempt to read a new line from its file, blocking,
    /// up to some maximum but unspecified amount of time. `read_line` will open
    /// a new file handler as needed, transparently to the caller.
    pub(super) fn read_line(&mut self) -> io::Result<RawLineResult> {
        self.track_read_attempt();

        let reader = &mut self.reader;
        let file_position = &mut self.file_position;
        let initial_position = *file_position;
        match read_until_with_max_size(
            reader,
            file_position,
            self.line_delimiter.as_ref(),
            &mut self.buf,
            self.max_line_bytes,
        ) {
            Ok(ReadResult {
                successfully_read: Some(_),
                discarded_for_size_and_truncated,
            }) => {
                self.track_read_success();
                Ok(RawLineResult {
                    raw_line: Some(RawLine {
                        offset: initial_position,
                        bytes: self.buf.split().freeze(),
                    }),
                    discarded_for_size_and_truncated,
                })
            }
            Ok(ReadResult {
                successfully_read: None,
                discarded_for_size_and_truncated,
            }) => {
                if !self.file_findable() {
                    self.set_dead();
                    // File has been deleted, so return what we have in the buffer, even though it
                    // didn't end with a newline. This is not a perfect signal for when we should
                    // give up waiting for a newline, but it's decent.
                    let buf = self.buf.split().freeze();
                    if buf.is_empty() {
                        // EOF
                        self.reached_eof = true;
                        Ok(RawLineResult {
                            raw_line: None,
                            discarded_for_size_and_truncated,
                        })
                    } else {
                        Ok(RawLineResult {
                            raw_line: Some(RawLine {
                                offset: initial_position,
                                bytes: buf,
                            }),
                            discarded_for_size_and_truncated,
                        })
                    }
                } else {
                    self.reached_eof = true;
                    Ok(RawLineResult {
                        raw_line: None,
                        discarded_for_size_and_truncated,
                    })
                }
            }
            Err(e) => {
                if let io::ErrorKind::NotFound = e.kind() {
                    self.set_dead();
                }
                Err(e)
            }
        }
    }

    #[inline]
    fn track_read_attempt(&mut self) {
        self.last_read_attempt = Instant::now();
    }

    #[inline]
    fn track_read_success(&mut self) {
        self.last_read_success = Instant::now();
    }

    #[inline]
    pub fn last_read_success(&self) -> Instant {
        self.last_read_success
    }

    #[inline]
    pub fn should_read(&self) -> bool {
        self.last_read_success.elapsed() < Duration::from_secs(10)
            || self.last_read_attempt.elapsed() > Duration::from_secs(10)
    }

    #[inline]
    pub fn last_seen(&self) -> Instant {
        self.last_seen
    }

}

fn is_gzipped<R: Read>(r: &mut io::BufReader<R>) -> io::Result<bool> {
    let header_bytes = r.fill_buf()?;
    // WARN: The paired `BufReader::consume` is not called intentionally. If we
    // do we'll chop a decent part of the potential gzip stream off.
    Ok(header_bytes.starts_with(GZIP_MAGIC))
}

fn null_reader() -> impl BufRead {
    io::Cursor::new(Vec::new())
}
