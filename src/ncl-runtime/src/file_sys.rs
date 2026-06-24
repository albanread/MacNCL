//! File I/O. Adapted from NewCP's `host_file_sys.rs` (sister repo
//! at E:/NewCP) — the handle-table + std::fs pattern is identical;
//! only the path encoding (Word::String here, UTF-32 buffer there)
//! and the Lisp-level surface differ.
//!
//! Design:
//!
//! - File handles are opaque `i64` values — small fixnums Lisp
//!   passes around. Handle 0 is reserved for "invalid", returned
//!   when an operation fails.
//! - Paths are Word-tagged Strings (UTF-32 internally); we decode
//!   to UTF-8 at the boundary.
//! - All operations grab a process-global mutex on the handle
//!   table. This is fine for the kind of low-throughput I/O Lisp
//!   programs do; if it ever matters we'll switch to
//!   per-handle locks.
//! - Text I/O is UTF-8 on disk, UTF-32 in memory (matches CL's
//!   character semantics — see project_strings_and_chars.md).
//!
//! Errors surface as i64 sentinel returns (0 for "invalid handle"
//! return paths, -1 for "operation failed", nil/0 for "EOF") rather
//! than panics. The condition system is what'll turn these into
//! proper Lisp errors when it lands.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::word::{Tag, Word};

/// Open mode: matches the constants we expose on the Lisp side
/// via OPEN-INPUT-FILE / OPEN-OUTPUT-FILE / OPEN-APPEND-FILE.
#[derive(Clone, Copy)]
pub enum Mode {
    Input,
    Output,
    Append,
}

/// Per-handle state. We keep a `BufReader` for read modes so
/// `read-line` works in O(line length) rather than reading
/// byte-by-byte through the kernel.
enum FileState {
    Read(BufReader<File>),
    Write(File),
    Append(File),
}

impl FileState {
    fn underlying(&mut self) -> &mut File {
        match self {
            FileState::Read(r) => r.get_mut(),
            FileState::Write(f) | FileState::Append(f) => f,
        }
    }
}

struct FileSysState {
    next_handle: i64,
    files: HashMap<i64, FileState>,
}

impl FileSysState {
    fn new() -> Self {
        Self { next_handle: 1, files: HashMap::new() }
    }
}

static FILES: OnceLock<Mutex<FileSysState>> = OnceLock::new();

fn files() -> &'static Mutex<FileSysState> {
    FILES.get_or_init(|| Mutex::new(FileSysState::new()))
}

/// Decode a Lisp-tagged String Word into a Rust PathBuf.
/// Returns None if `path_word` is not a String.
fn decode_path(path_word: Word) -> Option<PathBuf> {
    if path_word.tag() != Tag::String {
        return None;
    }
    let s: String = crate::gc_string::chars_of(path_word).collect();
    Some(PathBuf::from(s))
}

/// Open a file. Returns a positive i64 handle on success, 0 on
/// failure.
pub fn open_file(path_word: Word, mode: Mode) -> i64 {
    let Some(path) = decode_path(path_word) else { return 0 };
    let mut opts = OpenOptions::new();
    let state = match mode {
        Mode::Input => match opts.read(true).open(&path) {
            Ok(f) => FileState::Read(BufReader::new(f)),
            Err(_) => return 0,
        },
        Mode::Output => match opts.write(true).create(true).truncate(true).open(&path) {
            Ok(f) => FileState::Write(f),
            Err(_) => return 0,
        },
        Mode::Append => match opts.append(true).create(true).open(&path) {
            Ok(f) => FileState::Append(f),
            Err(_) => return 0,
        },
    };
    let mut tbl = files().lock().expect("file_sys mutex poisoned");
    let h = tbl.next_handle;
    tbl.next_handle = tbl.next_handle.wrapping_add(1);
    if tbl.next_handle == 0 {
        tbl.next_handle = 1;
    }
    tbl.files.insert(h, state);
    h
}

/// Close a handle. Idempotent on already-closed / invalid handles.
pub fn close_file(handle: i64) {
    if handle == 0 {
        return;
    }
    let mut tbl = files().lock().expect("file_sys mutex poisoned");
    tbl.files.remove(&handle);
}

/// Read one line (UTF-8) from `handle`. Strips a trailing `\n`
/// (and `\r\n`). Returns `Some(String)` on success, `None` at EOF
/// or on error.
pub fn read_line(handle: i64) -> Option<String> {
    if handle == 0 {
        return None;
    }
    let mut tbl = files().lock().expect("file_sys mutex poisoned");
    let entry = tbl.files.get_mut(&handle)?;
    let FileState::Read(reader) = entry else { return None };
    let mut buf = String::new();
    match reader.read_line(&mut buf) {
        Ok(0) => None, // EOF.
        Ok(_) => {
            // Strip trailing \n and \r\n.
            if buf.ends_with('\n') {
                buf.pop();
                if buf.ends_with('\r') {
                    buf.pop();
                }
            }
            Some(buf)
        }
        Err(_) => None,
    }
}

/// Read one Unicode character (UTF-8 decoded) from `handle`.
/// Returns `Some(char)` on success, `None` at EOF or on error.
pub fn read_char(handle: i64) -> Option<char> {
    if handle == 0 {
        return None;
    }
    let mut tbl = files().lock().expect("file_sys mutex poisoned");
    let entry = tbl.files.get_mut(&handle)?;
    let FileState::Read(reader) = entry else { return None };
    // Peek the first byte to determine the UTF-8 sequence length.
    let mut first = [0u8; 1];
    match reader.read(&mut first) {
        Ok(0) => return None,
        Ok(_) => {}
        Err(_) => return None,
    }
    let need = utf8_len(first[0]);
    let mut buf = [0u8; 4];
    buf[0] = first[0];
    if need > 1 {
        match reader.read_exact(&mut buf[1..need]) {
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    let s = std::str::from_utf8(&buf[..need]).ok()?;
    s.chars().next()
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 { 1 }
    else if b < 0xC0 { 1 } // continuation byte at start — invalid; treat as one byte
    else if b < 0xE0 { 2 }
    else if b < 0xF0 { 3 }
    else { 4 }
}

/// Write a string's UTF-8 encoding to `handle`. Returns the byte
/// count written, or -1 on error.
pub fn write_string(handle: i64, s: &str) -> i64 {
    if handle == 0 {
        return -1;
    }
    let mut tbl = files().lock().expect("file_sys mutex poisoned");
    let Some(entry) = tbl.files.get_mut(&handle) else { return -1 };
    let f = entry.underlying();
    match f.write_all(s.as_bytes()) {
        Ok(_) => s.len() as i64,
        Err(_) => -1,
    }
}

/// Read one byte (0..255) from `handle`. Returns -1 at EOF or on
/// error.
pub fn read_byte(handle: i64) -> i64 {
    if handle == 0 {
        return -1;
    }
    let mut tbl = files().lock().expect("file_sys mutex poisoned");
    let Some(entry) = tbl.files.get_mut(&handle) else { return -1 };
    let FileState::Read(reader) = entry else { return -1 };
    let mut buf = [0u8; 1];
    match reader.read(&mut buf) {
        Ok(1) => buf[0] as i64,
        _ => -1,
    }
}

/// Write one byte to `handle`. Returns 1 on success, -1 on error.
pub fn write_byte(handle: i64, byte: i64) -> i64 {
    if handle == 0 {
        return -1;
    }
    let buf = [(byte & 0xFF) as u8];
    let mut tbl = files().lock().expect("file_sys mutex poisoned");
    let Some(entry) = tbl.files.get_mut(&handle) else { return -1 };
    let f = entry.underlying();
    match f.write_all(&buf) {
        Ok(_) => 1,
        Err(_) => -1,
    }
}

/// File length in bytes, or -1 on error.
pub fn file_length(handle: i64) -> i64 {
    if handle == 0 {
        return -1;
    }
    let tbl = files().lock().expect("file_sys mutex poisoned");
    let Some(entry) = tbl.files.get(&handle) else { return -1 };
    let f = match entry {
        FileState::Read(r) => r.get_ref(),
        FileState::Write(f) | FileState::Append(f) => f,
    };
    match f.metadata() {
        Ok(md) => md.len() as i64,
        Err(_) => -1,
    }
}

/// Current read/write byte position, or -1 on error.
pub fn file_position(handle: i64) -> i64 {
    if handle == 0 {
        return -1;
    }
    let mut tbl = files().lock().expect("file_sys mutex poisoned");
    let Some(entry) = tbl.files.get_mut(&handle) else { return -1 };
    let f = entry.underlying();
    match f.stream_position() {
        Ok(p) => p as i64,
        Err(_) => -1,
    }
}

/// Seek to a byte offset from start. Returns 1 on success, -1 on error.
pub fn set_position(handle: i64, pos: i64) -> i64 {
    if handle == 0 || pos < 0 {
        return -1;
    }
    let mut tbl = files().lock().expect("file_sys mutex poisoned");
    let Some(entry) = tbl.files.get_mut(&handle) else { return -1 };
    let f = entry.underlying();
    match f.seek(SeekFrom::Start(pos as u64)) {
        Ok(_) => 1,
        Err(_) => -1,
    }
}

/// Flush any buffered writes. Returns 1 on success, -1 on error.
pub fn flush_file(handle: i64) -> i64 {
    if handle == 0 {
        return -1;
    }
    let mut tbl = files().lock().expect("file_sys mutex poisoned");
    let Some(entry) = tbl.files.get_mut(&handle) else { return -1 };
    let f = entry.underlying();
    match f.flush() {
        Ok(_) => 1,
        Err(_) => -1,
    }
}

/// Whether `path` exists on the filesystem.
pub fn file_exists(path_word: Word) -> bool {
    match decode_path(path_word) {
        Some(p) => p.exists(),
        None => false,
    }
}

/// Delete `path`. Returns true on success.
pub fn delete_file(path_word: Word) -> bool {
    let Some(path) = decode_path(path_word) else { return false };
    std::fs::remove_file(&path).is_ok()
}

/// Rename `old` to `new`. Returns true on success.
pub fn rename_file(old_word: Word, new_word: Word) -> bool {
    let Some(old) = decode_path(old_word) else { return false };
    let Some(new) = decode_path(new_word) else { return false };
    std::fs::rename(&old, &new).is_ok()
}
