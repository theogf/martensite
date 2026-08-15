// `--status` / `--status=live` dashboard: renders the conductor's in-memory
// worker/client/pressure state as a tree. No worker-protocol round trips —
// everything here comes from state the conductor already has on hand
// (mirrors upstream: status.zig reads straight from Conductor fields too).
use crate::conductor::Conductor;
use crate::worker::Worker;

pub const LIVE_HEARTBEAT_MS: u64 = 1000;
pub const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
pub const SHOW_CURSOR: &[u8] = b"\x1b[?25h\r\n";

/// Build the escape sequence to redraw in place: DEC 2026 synchronized
/// update, move up `prev_lines` and clear to end, then the new frame.
pub fn redraw_sequence(prev_lines: usize, frame: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() + 32);
    out.extend_from_slice(b"\x1b[?2026h");
    if prev_lines > 0 {
        out.extend_from_slice(format!("\x1b[{}F", prev_lines).as_bytes());
    }
    out.extend_from_slice(b"\x1b[0J");
    out.extend_from_slice(frame.as_bytes());
    out.extend_from_slice(b"\x1b[?2026l");
    out
}

// --- Health/state colors (flat ANSI — see CLAUDE.md note on the skipped
// truecolor/OSC-probed palette from upstream) ---
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

fn health_dot(w: &Worker) -> &'static str {
    if w.ping_pending { "\x1b[33m●\x1b[0m" } else if w.active_clients > 0 { "\x1b[32m●\x1b[0m" } else { "\x1b[2m◌\x1b[0m" }
}

struct KeyInfo {
    heading: String, // grouping heading: project path, "sandboxed", etc.
    channel: Option<String>,
}

/// Parse a pool key (see conductor::make_worker_key and the sandbox-mode
/// variants in handle_client) back into display fields. Keys are NUL-joined;
/// see conductor.rs for the exact producer of each shape. The packed thread
/// spec at the end of each key is redundant for display — every worker in a
/// group already reports its own `threads` field on its line.
fn describe_key(key: &str) -> KeyInfo {
    let parts: Vec<&str> = key.split('\0').collect();
    if key.starts_with("__sandbox__") {
        let channel = parts.get(1).filter(|s| !s.is_empty()).map(|s| s.to_string());
        return KeyInfo { heading: "sandboxed (remote)".to_string(), channel };
    }
    if key.starts_with("__lsandbox__") {
        let proj = parts.get(2).filter(|s| !s.is_empty()).map(|s| s.to_string())
            .unwrap_or_else(|| "sandboxed".to_string());
        let channel = parts.get(3).filter(|s| !s.is_empty()).map(|s| s.to_string());
        return KeyInfo { heading: format!("{} (sandboxed)", contract_home(&proj)), channel };
    }
    let project = parts.first().copied().unwrap_or("");
    let channel = parts.get(1).filter(|s| !s.is_empty()).map(|s| s.to_string());
    let heading = if project.is_empty() { "(default)".to_string() } else { contract_home(project) };
    KeyInfo { heading, channel }
}

fn contract_home(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            if let Some(rest) = path.strip_prefix(&home) {
                if rest.is_empty() || rest.starts_with('/') {
                    return format!("~{}", rest);
                }
            }
        }
    }
    path.to_string()
}

fn format_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 { return format!("{}s", secs); }
    let mins = secs / 60;
    if mins < 60 { return format!("{}m{}s", mins, secs % 60); }
    let hours = mins / 60;
    if hours < 24 { return format!("{}h{}m", hours, mins % 60); }
    format!("{}d{}h", hours / 24, hours % 24)
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB * KB { return format!("{}KB", (b / KB).round() as u64); }
    if b < KB * KB * KB { return format!("{:.0}MB", b / (KB * KB)); }
    format!("{:.1}GB", b / (KB * KB * KB))
}

fn clients_for(c: &Conductor, worker_id: u32, now_us: i64) -> Vec<(u32, i64)> {
    c.active_clients.iter()
        .filter(|(_, v)| v.worker_id == worker_id)
        .map(|(&pid, v)| (pid, (now_us - v.start_time_us) / 1_000_000))
        .collect()
}

fn render_worker_line(out: &mut String, c: &Conductor, key: &str, w: &Worker, now: i64, prefix: &str) {
    let mut identity = format!("{} #{}", health_dot(w), w.id);
    if let Some(label) = &w.session_label { identity.push_str(&format!(" [{}]", label)); }
    if let Some(ch) = &w.julia_channel { identity.push_str(&format!(" {}", ch)); }
    if w.interactive { identity.push_str(" (interactive)"); }
    if w.threads != crate::args::THREADS_NONE {
        if let Some(t) = crate::args::render_threads(w.threads) {
            identity.push_str(&format!(" (threads={})", t));
        }
    }
    if w.sandboxed { identity.push_str(" (remote)"); }

    let uptime = format_duration(now - w.created_at);
    let mem = format_bytes(w.mem_bytes);
    let mut line = format!("{}{:<34} up {:<7} {:>8}  {:>5.1}%", prefix, identity, uptime, mem, w.cpu_pct);

    if c.pressure.active() && c.cullable(w, now) {
        let activity_hl = c.activity_half_life();
        let activity = {
            // peek() takes &self, so this is a read-only snapshot for display.
            let occ = w.occ_fast;
            occ.peek(now, activity_hl)
        };
        let budget = c.idle_budget(key, w);
        let idle = now - w.last_active;
        let remaining = (budget - idle as f64).max(0.0) as i64;
        line.push_str(&format!("  {}activity {:.0}% · culls in {}{}", DIM, activity * 100.0, format_duration(remaining), RESET));
    } else if w.active_clients == 0 {
        line.push_str(&format!("  {}idle {}{}", DIM, format_duration(now - w.last_active), RESET));
    }
    out.push_str(&line);
    out.push('\n');

    let now_us = now * 1_000_000;
    let clients = clients_for(c, w.id, now_us);
    for (i, (pid, dur)) in clients.iter().enumerate() {
        let branch = if i + 1 == clients.len() { "╰─" } else { "├─" };
        out.push_str(&format!("│  {} pid {} attached {}\n", branch, pid, format_duration(*dur)));
    }
}

pub fn render_text(c: &mut Conductor, now: i64) -> String {
    let mut out = String::new();

    let worker_count: usize = c.workers.values().map(|l| l.len()).sum();
    let reserve_count = if c.reserve.is_some() { 1 } else { 0 };
    let client_count = c.active_clients.len();
    let total_mem: u64 = c.workers.values().flatten().map(|w| w.mem_bytes).sum::<u64>()
        + c.reserve.as_ref().map(|r| r.mem_bytes).unwrap_or(0);

    out.push_str(&format!(
        "Julia Daemon Conductor \u{2014} {} worker(s), {} reserve, {} client(s), {} total\n",
        worker_count, reserve_count, client_count, format_bytes(total_mem)
    ));
    if c.pressure.active() {
        out.push_str(&format!("{}pressure source: {}{}\n", YELLOW, c.pressure.source_name(), RESET));
    }
    out.push('\n');

    let mut keys: Vec<&String> = c.workers.keys().collect();
    keys.sort();
    for key in keys {
        let Some(list) = c.workers.get(key) else { continue };
        if list.is_empty() { continue; }
        let info = describe_key(key);
        let mut heading = info.heading.clone();
        if let Some(ch) = &info.channel { heading.push_str(&format!(" [{}]", ch)); }
        out.push_str(&format!("{}{}{}\n", GREEN, heading, RESET));
        for (i, w) in list.iter().enumerate() {
            let branch = if i + 1 == list.len() { "\u{2570}\u{2500} " } else { "\u{251c}\u{2500} " };
            render_worker_line(&mut out, c, key, w, now, branch);
        }
        out.push('\n');
    }

    if let Some(r) = &c.reserve {
        out.push_str(&format!("{}reserve{}\n", GREEN, RESET));
        render_worker_line(&mut out, c, "", r, now, "\u{2570}\u{2500} ");
        out.push('\n');
    }

    out.push_str(&format!("worker_args: {}\n", c.config.worker_args));
    out
}

pub fn render_json(c: &mut Conductor, now: i64) -> String {
    let mut workers_json = Vec::new();
    for (key, list) in c.workers.iter() {
        for w in list {
            let now_us = now * 1_000_000;
            let clients: Vec<String> = clients_for(c, w.id, now_us).iter()
                .map(|(pid, dur)| format!(
                    r#"{{"pid":{},"attached_seconds":{}}}"#, pid, dur
                ))
                .collect();
            workers_json.push(format!(
                r#"{{"id":{},"pool_key":{},"session_label":{},"julia_channel":{},"threads":[{},{}],"interactive":{},"sandboxed":{},"created_at":{},"last_active":{},"active_clients":{},"mem_bytes":{},"cpu_pct":{:.2},"clients":[{}]}}"#,
                w.id, json_str(key), json_opt_str(w.session_label.as_deref()),
                json_opt_str(w.julia_channel.as_deref()), w.threads[0], w.threads[1],
                w.interactive, w.sandboxed, w.created_at, w.last_active, w.active_clients,
                w.mem_bytes, w.cpu_pct, clients.join(",")
            ));
        }
    }
    let reserve_json = match &c.reserve {
        Some(r) => format!(
            r#"{{"id":{},"julia_channel":{},"threads":[{},{}],"mem_bytes":{}}}"#,
            r.id, json_opt_str(r.julia_channel.as_deref()), r.threads[0], r.threads[1], r.mem_bytes
        ),
        None => "null".to_string(),
    };

    format!(
        r#"{{"workers":[{}],"reserve":{},"totals":{{"clients":{}}},"min_ttl":{},"max_ttl":{},"label_ttl":{},"worker_args":{},"pressure":{{"source":{},"active":{}}}}}"#,
        workers_json.join(","), reserve_json, c.active_clients.len(),
        c.config.min_ttl, c.config.max_ttl, c.config.label_ttl,
        json_str(&c.config.worker_args), json_str(c.pressure.source_name()), c.pressure.active(),
    )
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_opt_str(s: Option<&str>) -> String {
    match s {
        Some(s) => json_str(s),
        None => "null".to_string(),
    }
}
