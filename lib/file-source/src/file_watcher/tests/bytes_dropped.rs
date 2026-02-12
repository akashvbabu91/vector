//! Tests for bytes_dropped metric tracking in FileWatcher.
//!
//! These tests verify that the bytes_dropped metric is correctly calculated
//! in various scenarios including:
//! - Normal EOF reached (bytes_dropped should be 0)
//! - File deleted before EOF (bytes_dropped should be > 0)
//! - File rotation with same inode (simple rename)
//! - File rotation with different inode (file replaced)
//! - Checkpoint resume scenarios
//! - read_from: end configuration

use std::{
    fs::{self, File},
    io::Write,
};

use bytes::Bytes;

use crate::{
    file_watcher::{FileWatcher, RawLineResult},
    ReadFrom,
};

/// Helper to read all available lines from a FileWatcher
fn read_all_lines(fw: &mut FileWatcher) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        match fw.read_line() {
            Ok(RawLineResult {
                raw_line: Some(line),
                ..
            }) => {
                if !line.bytes.is_empty() {
                    lines.push(String::from_utf8_lossy(&line.bytes).to_string());
                }
            }
            Ok(RawLineResult { raw_line: None, .. }) => break,
            Err(_) => break,
        }
    }
    lines
}

/// Test: Normal EOF reached - bytes_dropped should be 0
#[test]
fn test_bytes_dropped_zero_when_eof_reached() {
    let dir = tempfile::TempDir::new().expect("could not create tempdir");
    let path = dir.path().join("test.log");

    // Create file with known content
    let content = "line1\nline2\nline3\n";
    fs::write(&path, content).expect("could not write file");

    let mut fw = FileWatcher::new(
        path.clone(),
        ReadFrom::Beginning,
        None,
        100_000,
        Bytes::from("\n"),
    )
    .expect("must be able to create");

    // Read all lines
    let lines = read_all_lines(&mut fw);
    assert_eq!(lines.len(), 3);
    assert!(fw.reached_eof());

    // bytes_dropped should be 0 since we read everything
    assert_eq!(fw.get_bytes_dropped(), 0);
}

/// Test: File not fully read - bytes_dropped should reflect unread bytes
#[test]
fn test_bytes_dropped_nonzero_when_not_fully_read() {
    let dir = tempfile::TempDir::new().expect("could not create tempdir");
    let path = dir.path().join("test.log");

    // Create file with known content: 30 bytes total (10 bytes per line including newline)
    let content = "aaaaaaaaa\nbbbbbbbbb\nccccccccc\n";
    assert_eq!(content.len(), 30);
    fs::write(&path, content).expect("could not write file");

    let mut fw = FileWatcher::new(
        path.clone(),
        ReadFrom::Beginning,
        None,
        100_000,
        Bytes::from("\n"),
    )
    .expect("must be able to create");

    // Read only one line (10 bytes)
    match fw.read_line() {
        Ok(RawLineResult {
            raw_line: Some(line),
            ..
        }) => {
            assert_eq!(line.bytes.as_ref(), b"aaaaaaaaa");
        }
        _ => panic!("expected to read a line"),
    }

    // bytes_dropped should be 20 (we read 10 of 30 bytes)
    assert_eq!(fw.get_bytes_dropped(), 20);
}

/// Test: read_from: end - bytes_dropped should be 0 (intentionally skipped bytes)
#[test]
fn test_bytes_dropped_zero_when_read_from_end() {
    let dir = tempfile::TempDir::new().expect("could not create tempdir");
    let path = dir.path().join("test.log");

    // Create file with existing content
    let content = "old_line1\nold_line2\n";
    fs::write(&path, content).expect("could not write file");

    let mut fw = FileWatcher::new(
        path.clone(),
        ReadFrom::End, // Start from end
        None,
        100_000,
        Bytes::from("\n"),
    )
    .expect("must be able to create");

    // No lines to read since we started at the end
    let lines = read_all_lines(&mut fw);
    assert!(lines.is_empty());

    // bytes_dropped should be 0 because we intentionally started at end
    // The initial_file_position equals initial_file_size in this case
    assert_eq!(fw.get_bytes_dropped(), 0);
}

/// Test: Checkpoint resume - bytes_dropped calculated from checkpoint position
#[test]
fn test_bytes_dropped_with_checkpoint_resume() {
    let dir = tempfile::TempDir::new().expect("could not create tempdir");
    let path = dir.path().join("test.log");

    // Create file with known content: 30 bytes
    let content = "aaaaaaaaa\nbbbbbbbbb\nccccccccc\n";
    fs::write(&path, content).expect("could not write file");

    // Resume from position 10 (after first line)
    let mut fw = FileWatcher::new(
        path.clone(),
        ReadFrom::Checkpoint(10),
        None,
        100_000,
        Bytes::from("\n"),
    )
    .expect("must be able to create");

    // Read one line (line 2)
    match fw.read_line() {
        Ok(RawLineResult {
            raw_line: Some(line),
            ..
        }) => {
            assert_eq!(line.bytes.as_ref(), b"bbbbbbbbb");
        }
        _ => panic!("expected to read a line"),
    }

    // bytes_dropped should be 10 (30 total - 20 position after reading line 2)
    // We started at 10, read 10 more bytes (line 2), position is now 20, 10 bytes remaining
    assert_eq!(fw.get_bytes_dropped(), 10);
}

/// Test: File deleted before EOF - simulates the main data loss scenario
#[test]
fn test_bytes_dropped_file_deleted_before_eof() {
    let dir = tempfile::TempDir::new().expect("could not create tempdir");
    let path = dir.path().join("test.log");

    // Create file with 50 bytes of content
    let content = "aaaaaaaaaa\nbbbbbbbbbb\ncccccccccc\ndddddddddd\neeeeeeeeee\n";
    assert_eq!(content.len(), 55);
    fs::write(&path, content).expect("could not write file");

    let mut fw = FileWatcher::new(
        path.clone(),
        ReadFrom::Beginning,
        None,
        100_000,
        Bytes::from("\n"),
    )
    .expect("must be able to create");

    // Read only 2 lines (22 bytes)
    for _ in 0..2 {
        match fw.read_line() {
            Ok(RawLineResult {
                raw_line: Some(_), ..
            }) => {}
            _ => panic!("expected to read a line"),
        }
    }

    // Mark file as not findable (simulating file deletion)
    fw.set_file_findable(false);

    // bytes_dropped should be 33 (55 - 22)
    assert_eq!(fw.get_bytes_dropped(), 33);
    assert!(!fw.reached_eof());
}

/// Test: update_path with same inode (simple rename) - no bytes_dropped emission
#[test]
#[cfg(unix)] // inode behavior is Unix-specific
fn test_update_path_same_inode_no_bytes_dropped() {
    let dir = tempfile::TempDir::new().expect("could not create tempdir");
    let path = dir.path().join("test.log");
    let rotated_path = dir.path().join("test.log.1");

    // Create and partially read file
    let content = "line1\nline2\nline3\n";
    fs::write(&path, content).expect("could not write file");

    let mut fw = FileWatcher::new(
        path.clone(),
        ReadFrom::Beginning,
        None,
        100_000,
        Bytes::from("\n"),
    )
    .expect("must be able to create");

    // Read one line
    match fw.read_line() {
        Ok(RawLineResult {
            raw_line: Some(_), ..
        }) => {}
        _ => panic!("expected to read a line"),
    }

    // Rename file (same inode)
    fs::rename(&path, &rotated_path).expect("could not rename");

    // update_path should return None (no inode change)
    let result = fw.update_path(rotated_path.clone());
    assert!(result.is_ok());
    assert!(result.unwrap().is_none()); // No inode change info
}

/// Test: update_path with different inode (file replaced) - should return bytes_dropped info
#[test]
#[cfg(unix)] // inode behavior is Unix-specific
fn test_update_path_different_inode_returns_bytes_dropped() {
    let dir = tempfile::TempDir::new().expect("could not create tempdir");
    let path = dir.path().join("test.log");

    // Create and partially read file
    let content = "line1\nline2\nline3\n"; // 18 bytes
    fs::write(&path, content).expect("could not write file");

    let mut fw = FileWatcher::new(
        path.clone(),
        ReadFrom::Beginning,
        None,
        100_000,
        Bytes::from("\n"),
    )
    .expect("must be able to create");

    // Read one line (6 bytes including newline)
    match fw.read_line() {
        Ok(RawLineResult {
            raw_line: Some(_), ..
        }) => {}
        _ => panic!("expected to read a line"),
    }

    // Delete and recreate file (different inode)
    fs::remove_file(&path).expect("could not remove file");
    let new_content = "new_line1\nnew_line2\n";
    fs::write(&path, new_content).expect("could not write new file");

    // update_path should return inode change info
    let result = fw.update_path(path.clone());
    assert!(result.is_ok());

    let inode_change = result.unwrap();
    assert!(inode_change.is_some());

    let info = inode_change.unwrap();
    // bytes_dropped should be 12 (18 - 6)
    assert_eq!(info.bytes_dropped, 12);
    assert!(!info.reached_eof);
}

/// Test: Empty file - bytes_dropped should be 0
#[test]
fn test_bytes_dropped_empty_file() {
    let dir = tempfile::TempDir::new().expect("could not create tempdir");
    let path = dir.path().join("test.log");

    // Create empty file
    File::create(&path).expect("could not create file");

    let mut fw = FileWatcher::new(
        path.clone(),
        ReadFrom::Beginning,
        None,
        100_000,
        Bytes::from("\n"),
    )
    .expect("must be able to create");

    // Try to read (should get nothing)
    let lines = read_all_lines(&mut fw);
    assert!(lines.is_empty());

    // bytes_dropped should be 0
    assert_eq!(fw.get_bytes_dropped(), 0);
}

/// Test: Large file partially read - bytes_dropped should be accurate
#[test]
fn test_bytes_dropped_large_file_partial_read() {
    let dir = tempfile::TempDir::new().expect("could not create tempdir");
    let path = dir.path().join("test.log");

    // Create file with 1000 lines of 100 bytes each (including newline)
    let line = "x".repeat(99) + "\n";
    let mut content = String::new();
    for _ in 0..1000 {
        content.push_str(&line);
    }
    let total_size = content.len() as u64; // 100,000 bytes
    fs::write(&path, &content).expect("could not write file");

    let mut fw = FileWatcher::new(
        path.clone(),
        ReadFrom::Beginning,
        None,
        100_000,
        Bytes::from("\n"),
    )
    .expect("must be able to create");

    // Read 100 lines (10,000 bytes)
    for _ in 0..100 {
        match fw.read_line() {
            Ok(RawLineResult {
                raw_line: Some(_), ..
            }) => {}
            _ => panic!("expected to read a line"),
        }
    }

    // bytes_dropped should be 90,000
    assert_eq!(fw.get_bytes_dropped(), total_size - 10_000);
}

/// Test: File grows after watcher created - bytes_dropped based on initial size
#[test]
fn test_bytes_dropped_file_grows_after_watcher_created() {
    let dir = tempfile::TempDir::new().expect("could not create tempdir");
    let path = dir.path().join("test.log");

    // Create file with initial content
    let initial_content = "line1\nline2\n"; // 12 bytes
    fs::write(&path, initial_content).expect("could not write file");

    let mut fw = FileWatcher::new(
        path.clone(),
        ReadFrom::Beginning,
        None,
        100_000,
        Bytes::from("\n"),
    )
    .expect("must be able to create");

    // Append more content
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("could not open for append");
    file.write_all(b"line3\nline4\n").expect("could not append");
    file.flush().expect("could not flush");

    // Read only first 2 lines
    for _ in 0..2 {
        match fw.read_line() {
            Ok(RawLineResult {
                raw_line: Some(_), ..
            }) => {}
            _ => panic!("expected to read a line"),
        }
    }

    // bytes_dropped is based on initial_file_size (12), not current size (24)
    // We read 12 bytes, initial size was 12, so bytes_dropped = 0
    assert_eq!(fw.get_bytes_dropped(), 0);
}
