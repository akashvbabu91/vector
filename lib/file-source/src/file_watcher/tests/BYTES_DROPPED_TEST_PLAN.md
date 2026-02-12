# Bytes Dropped Metric Test Plan

This document describes the test coverage for the `bytes_dropped` metric in the file source component.

## Overview

The `bytes_dropped` metric tracks how many bytes were not read from a file before it was unwatched. This helps identify data loss scenarios such as:
- Files deleted before Vector finished reading them
- Pod/DaemonSet crashes
- Aggressive log rotation

### Metric Details

- **Name**: `files_unwatched_bytes_dropped_total`
- **Type**: Counter
- **Labels**:
  - `reached_eof`: "true" or "false"
  - `file`: (optional, when `include_file_metric_tag` is enabled)

When `reached_eof=true`, `bytes_dropped` will always be 0.
When `reached_eof=false`, `bytes_dropped` indicates data loss.

---

## Unit Tests

These tests are in `lib/file-source/src/file_watcher/tests/bytes_dropped.rs`.

| Test Name | Scenario | Expected Behavior |
|-----------|----------|-------------------|
| `test_bytes_dropped_zero_when_eof_reached` | File fully read to EOF | `bytes_dropped=0`, `reached_eof=true` |
| `test_bytes_dropped_nonzero_when_not_fully_read` | Read 1 of 3 lines, then check | `bytes_dropped=20` (unread bytes) |
| `test_bytes_dropped_zero_when_read_from_end` | `read_from: end` configuration | `bytes_dropped=0` (intentionally skipped) |
| `test_bytes_dropped_with_checkpoint_resume` | Resume from checkpoint position | `bytes_dropped` calculated from checkpoint |
| `test_bytes_dropped_file_deleted_before_eof` | File marked not findable mid-read | `bytes_dropped > 0`, `reached_eof=false` |
| `test_update_path_same_inode_no_bytes_dropped` | File renamed (same inode) | No `FileInodeChangeInfo` returned |
| `test_update_path_different_inode_returns_bytes_dropped` | File replaced (different inode) | `FileInodeChangeInfo` with old file's `bytes_dropped` |
| `test_bytes_dropped_empty_file` | Empty file | `bytes_dropped=0` |
| `test_bytes_dropped_large_file_partial_read` | Read 100 of 1000 lines | `bytes_dropped=90000` |
| `test_bytes_dropped_file_grows_after_watcher_created` | File appended after watcher created | `bytes_dropped` based on initial size only |

---

## Integration Tests Required

These scenarios cannot be fully tested with unit tests and require integration testing:

### 1. DaemonSet/Pod Killed Mid-Read

**Scenario**: Vector process is killed (SIGKILL) while reading a file that is subsequently deleted.

**Why unit test is insufficient**:
- Process termination prevents any metric emission
- Checkpoint may not be written before crash
- Requires actual process lifecycle testing

**Expected behavior**:
- No metric emitted (process dead)
- On restart, if file still exists: resume from checkpoint
- On restart, if file deleted: data permanently lost (no metric possible)

**Test approach**:
- Integration test with process supervision
- Kill Vector mid-read, delete file, restart Vector
- Verify checkpoint behavior

---

### 2. `rotate_wait` Timeout

**Scenario**: File disappears and isn't found for longer than `rotate_wait` duration (default 1s).

**Why unit test is insufficient**:
- Requires actual timing and the file server's main loop
- `set_dead()` is called in `file_server.rs:333` based on elapsed time

**Expected behavior**:
- After `rotate_wait` elapses with file not findable
- Watcher marked dead
- `emit_file_unwatched` called with `reached_eof=false` if not fully read

**Test approach**:
- Integration test with `FileServer`
- Create file, start reading, delete file
- Wait for `rotate_wait` + buffer
- Verify metric emitted with correct `bytes_dropped`

---

### 3. Aggressive Log Rotation

**Scenario**: Log rotator deletes old files faster than Vector can read them.

**Why unit test is insufficient**:
- Requires concurrent file operations
- Timing-dependent behavior
- Involves interaction between glob discovery and reading

**Expected behavior**:
- Old rotated file deleted while Vector still reading
- `bytes_dropped` metric emitted for the deleted file

**Test approach**:
- Integration test with rapid rotation
- Create file, rotate multiple times quickly, delete old files
- Verify metrics capture all dropped bytes

---

### 4. Kubernetes Container Log Deletion

**Scenario**: kubelet deletes old container log files based on retention policy.

**Why unit test is insufficient**:
- Requires actual Kubernetes environment
- File deletion happens externally

**Expected behavior**:
- Same as "File deleted before EOF"
- `bytes_dropped` metric should capture unread bytes

**Test approach**:
- End-to-end test in Kubernetes
- Deploy Vector, generate logs, trigger log cleanup
- Verify Prometheus metrics show `bytes_dropped`

---

### 5. Inode Change During Active Reading

**Scenario**: File is replaced (new inode) while Vector is actively reading.

**Why unit test is insufficient**:
- `update_path()` is called from `file_server.rs` during glob cycle
- Requires coordination between discovery and reading

**Expected behavior**:
- `update_path()` returns `FileInodeChangeInfo`
- `emit_file_unwatched` called for old file
- Watcher continues with new file, metrics reset

**Test approach**:
- Integration test with `FileServer`
- Create file, start reading, replace file (rm + create)
- Verify two separate metric emissions

---

## Edge Cases to Consider

### File Truncation
- File is truncated (not deleted)
- Current position becomes invalid
- FileWatcher detects via size comparison and resets
- **Note**: `bytes_dropped` may not accurately reflect this case

### Gzipped Files
- Cannot seek in gzipped files
- `update_path()` with inode change on gzipped file uses `null_reader`
- May result in no data read from new file

### Checkpoint Corruption
- Checkpoint file corrupted or deleted
- Vector starts from `read_from` config (beginning or end)
- Previous progress lost, but no `bytes_dropped` metric (nothing was "dropped")

---

## Running Tests

```bash
# Run unit tests only
cargo test -p file-source bytes_dropped

# Run all file-source tests
cargo test -p file-source

# Run with verbose output
cargo test -p file-source bytes_dropped -- --nocapture
```

---

## Metrics Interpretation

### Healthy System
```
files_unwatched_bytes_dropped_total{reached_eof="true"} = 0
files_unwatched_bytes_dropped_total{reached_eof="false"} = 0
```

### Data Loss Detected
```
files_unwatched_bytes_dropped_total{reached_eof="true"} = 0
files_unwatched_bytes_dropped_total{reached_eof="false"} = 1048576  # 1MB lost
```

### Alert Suggestion
```yaml
# Prometheus alert for data loss
- alert: VectorFileDataLoss
  expr: increase(files_unwatched_bytes_dropped_total{reached_eof="false"}[5m]) > 0
  for: 1m
  labels:
    severity: warning
  annotations:
    summary: "Vector dropped {{ $value }} bytes from files"
```
