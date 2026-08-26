//! Collectors: one per source. File-tailers for local logs, interval pollers for
//! hosted APIs. Shared incremental-read logic lives here.

pub mod claude_code;
pub mod codex;
pub mod openrouter;

use anyhow::Result;
use std::io::{Read, Seek, SeekFrom};

use crate::db::{self, DbPool};
use crate::models::Event;

/// Read complete newline-terminated lines appended to `path` after `start`.
/// Returns the lines and the byte offset just past the last complete line
/// (a trailing partial line is left for next time).
pub fn read_appended_lines(path: &str, start: u64) -> Result<(Vec<String>, u64)> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    // If the file shrank (rotation/truncation), restart from 0.
    let start = if start > len { 0 } else { start };
    if start >= len {
        return Ok((Vec::new(), len));
    }
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf)?;

    // Find the last newline; everything after it is an incomplete line.
    let last_nl = buf.iter().rposition(|&b| b == b'\n');
    let complete_end = match last_nl {
        Some(i) => i + 1,
        None => return Ok((Vec::new(), start)), // no complete line yet
    };
    let text = String::from_utf8_lossy(&buf[..complete_end]);
    let lines: Vec<String> = text
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();
    Ok((lines, start + complete_end as u64))
}

/// Scan one file incrementally from its stored offset, parse each appended line
/// with `parse`, insert the resulting events, and advance the offset.
/// Returns the number of newly-inserted (deduped) events.
pub fn scan_file<F>(pool: &DbPool, path: &str, parse: F) -> Result<usize>
where
    F: Fn(&str) -> Option<Event>,
{
    let conn = pool.get()?;
    let start = db::get_file_offset(&conn, path)?;
    drop(conn);

    let (lines, new_offset) = read_appended_lines(path, start)?;
    if lines.is_empty() {
        // Still record the (possibly unchanged) offset.
        let conn = pool.get()?;
        db::set_file_offset(&conn, path, new_offset)?;
        return Ok(0);
    }

    let events: Vec<Event> = lines.iter().filter_map(|l| parse(l)).collect();
    let inserted = db::insert_events(pool, &events)?;

    let conn = pool.get()?;
    db::set_file_offset(&conn, path, new_offset)?;
    Ok(inserted)
}
