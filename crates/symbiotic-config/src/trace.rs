//! The I/O trace window's commands (ui/trace.js): the same switch and records as `symbiotic-ctl trace`, through
//! `symbiotic_core::iotrace`.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use symbiotic_core::iotrace::{self, Cursor, Record};
use tauri::Manager;

/// Where the window is in each process's file: what it has shown already.
#[derive(Default)]
pub struct TraceCursor(Mutex<Cursor>);

#[derive(Serialize)]
pub struct TraceState {
    on: bool,
    dir: String,
}

#[derive(Serialize)]
pub struct TraceRecord {
    at_ms: f64,
    pid: u32,
    depth: u32,
    vi: u32,
    op: String,
    status: i32,
    /// The status's name (`VI_ERROR_TMO`), or None for one the header does not have.
    status_name: Option<&'static str>,
    dur_us: u64,
    resource: String,
    count: Option<u32>,
    /// The bytes kept, escaped as `symbiotic-ctl trace show` writes them.
    data: String,
    /// More bytes moved than were kept.
    more: bool,
}

impl From<Record> for TraceRecord {
    fn from(r: Record) -> Self {
        TraceRecord {
            at_ms: r.at_us as f64 / 1000.0,
            pid: r.pid,
            depth: r.depth,
            vi: r.vi,
            status_name: iotrace::status_name(r.status),
            op: r.op,
            status: r.status,
            dur_us: r.dur_us,
            resource: r.resource,
            more: r.count.is_some_and(|c| c as usize > r.data.len()),
            count: r.count,
            data: iotrace::escape(&r.data),
        }
    }
}

#[derive(Serialize)]
pub struct TraceNews {
    records: Vec<TraceRecord>,
    /// The programs that wrote, by process id (their paths).
    programs: std::collections::HashMap<u32, String>,
}

#[tauri::command]
pub fn trace_state() -> TraceState {
    let dir = iotrace::dir();
    TraceState { on: iotrace::is_on(&dir), dir: dir.display().to_string() }
}

/// On starts a new capture (the earlier records go), as `symbiotic-ctl trace on` does.
#[tauri::command]
pub fn trace_set(app: tauri::AppHandle, on: bool) -> Result<TraceState, String> {
    let dir = iotrace::dir();
    iotrace::set_on(&dir, on).map_err(|e| format!("{}: {e}", dir.display()))?;
    if on {
        *app.state::<Arc<TraceCursor>>().0.lock().unwrap() = Cursor::default();
    }
    Ok(trace_state())
}

#[tauri::command]
pub fn trace_clear(app: tauri::AppHandle) -> Result<(), String> {
    let dir = iotrace::dir();
    iotrace::clear(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    *app.state::<Arc<TraceCursor>>().0.lock().unwrap() = Cursor::default();
    Ok(())
}

/// The records written since the last call, in time order; `from_start`: all of them (the window opening).
#[tauri::command]
pub async fn trace_read(app: tauri::AppHandle, from_start: bool) -> Result<TraceNews, String> {
    let cursor = app.state::<Arc<TraceCursor>>().inner().clone();
    crate::blocking(move || {
        let dir = iotrace::dir();
        let mut c = cursor.0.lock().unwrap();
        if from_start {
            *c = Cursor::default();
        }
        let records: Vec<TraceRecord> = c.next(&dir).into_iter().map(TraceRecord::from).collect();
        let programs = if records.is_empty() { Default::default() } else { iotrace::programs(&dir) };
        Ok(TraceNews { records, programs })
    })
    .await
}

/// Every record now in the files, as `symbiotic-ctl trace show` prints them, in a file in the user's
/// Downloads folder (else home); its path.
#[tauri::command]
pub async fn trace_export() -> Result<String, String> {
    crate::blocking(|| {
        let dir = iotrace::dir();
        let text = export_text(&dir);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let place = symbiotic_core::paths::downloads().ok_or("no Downloads folder or home directory")?;
        let path = place.join(format!("symbiotic-trace-{now}.txt"));
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path.display().to_string())
    })
    .await
}

fn export_text(dir: &std::path::Path) -> String {
    let records = Cursor::default().next(dir);
    let programs = iotrace::programs(dir);
    let offset = iotrace::local_offset_s();
    let mut named = std::collections::HashSet::new();
    let mut out = String::new();
    for r in &records {
        if named.insert(r.pid) {
            out += &format!("# process {} is {}\n", r.pid, programs.get(&r.pid).map(String::as_str).unwrap_or("?"));
        }
        out += &iotrace::show(r, offset, iotrace::status_name);
        out.push('\n');
    }
    out
}

/// The trace window: one, brought forward if it is open.
#[tauri::command]
pub async fn open_trace(app: tauri::AppHandle, title: String) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("trace") {
        let _ = w.unminimize();
        let _ = w.show();
        return w.set_focus().map_err(|e| e.to_string());
    }
    crate::own_title_bar(
        tauri::WebviewWindowBuilder::new(&app, "trace", tauri::WebviewUrl::App("trace.html".into()))
            .title(&title)
            .inner_size(1000.0, 640.0)
            .min_inner_size(560.0, 320.0),
    )
    .build()
    .map(|_| ())
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_export_names_each_program_once_and_keeps_time_order() {
        let d = std::env::temp_dir().join(format!("symbiotic-config-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let rec = |pid: u32, at_us: u64, op: &str, data: &[u8]| Record { at_us, pid, depth: 1, vi: 1, op: op.into(), status: 0, dur_us: 5, resource: "GPIB0::15::INSTR".into(), count: Some(data.len() as u32), data: data.to_vec() };
        std::fs::write(d.join("7.trace"), format!("# pid 7 C:\\apps\\one.exe\n{}\n{}\n", rec(7, 10, "viWrite", b"*IDN?\n").line(), rec(7, 30, "viRead", b"K\x01\n").line())).unwrap();
        std::fs::write(d.join("9.trace"), format!("# pid 9 /usr/bin/two\n{}\n", rec(9, 20, "viClear", b"").line())).unwrap();
        let text = export_text(&d);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.iter().filter(|l| l.starts_with("# process")).count(), 2, "{text}");
        assert!(lines[0].starts_with("# process 7 is ") && lines[0].ends_with("one.exe"), "{text}");
        let ops: Vec<&str> = ["viWrite", "viClear", "viRead"].into_iter().filter(|op| text.contains(op)).collect();
        assert_eq!(ops.len(), 3);
        let at = |op: &str| text.find(op).unwrap();
        assert!(at("viWrite") < at("viClear") && at("viClear") < at("viRead"), "merged by time: {text}");
        assert!(text.contains("\\x01") && !text.contains('\x01'), "bytes escaped as the window shows them: {text}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_record_as_the_page_gets_it() {
        let r = Record { at_us: 1_500, pid: 9, depth: 2, vi: 4, op: "viRead".into(), status: 0x3FFF0005, dur_us: 7, resource: "GPIB0::15::INSTR".into(), count: Some(100), data: b"AB\n".to_vec() };
        let p = TraceRecord::from(r);
        assert_eq!((p.at_ms, p.status_name, p.data.as_str(), p.more), (1.5, Some("VI_SUCCESS_TERM_CHAR"), "AB\\n", true));
    }
}
