// Suppress console window on Windows in release builds
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::{egui, Frame};
use egui::{Color32, Margin, RichText, ScrollArea, Stroke, Ui, Vec2};
use egui_plot::{Legend, Line, MarkerShape, Plot, Points};
use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use surge_ping::{Client, Config, IcmpPacket, PingIdentifier, PingSequence, ICMP};
use tokio::runtime::Runtime;
use tokio::time::sleep;

// ─── Constants ───────────────────────────────────────────────────────────────

const MAX_LOGS: usize = 5_000;  // max log entries in memory
const PING_TIMEOUT_MS: u64 = 3_000;
const REPAINT_INTERVAL_MS: u64 = 250;

// ─── Data Types ──────────────────────────────────────────────────────────────

fn now_ts() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}

fn now_file_ts() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

fn now_human() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[derive(Clone, Debug, PartialEq)]
enum Status {
    Idle,
    Resolving,
    Online,
    Timeout,
    Error(String),
}

impl Status {
    fn label(&self) -> &str {
        match self {
            Status::Idle => "idle",
            Status::Resolving => "resolving…",
            Status::Online => "online",
            Status::Timeout => "timeout",
            Status::Error(_) => "error",
        }
    }
    fn color(&self, loss: f64) -> Color32 {
        match self {
            Status::Online => {
                if loss == 0.0 { C::OK }
                else if loss < 20.0 { C::WARN }
                else { C::ERR }
            }
            Status::Timeout => C::ERR,
            Status::Error(_) => C::ERR,
            Status::Resolving => C::INFO,
            Status::Idle => C::DIM,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum LogLevel {
    Info,
    Ok,
    Warn,
    Err,
}

impl LogLevel {
    fn label(self) -> &'static str {
        match self {
            LogLevel::Info => "INFO",
            LogLevel::Ok   => "OK  ",
            LogLevel::Warn => "WARN",
            LogLevel::Err  => "ERR ",
        }
    }
    fn color(self) -> Color32 {
        match self {
            LogLevel::Info => C::INFO,
            LogLevel::Ok   => C::OK,
            LogLevel::Warn => C::WARN,
            LogLevel::Err  => C::ERR,
        }
    }
}

#[derive(Clone, Debug)]
struct LogEntry {
    time:  String,
    host:  String,
    msg:   String,
    level: LogLevel,
}

/// Per-host data: config + live stats + ring-buffer history
#[derive(Clone)]
struct Host {
    // identity
    id:       usize,
    name:     String,
    ip:       Option<IpAddr>,
    status:   Status,

    // config (live-editable)
    interval_ms:  u64,
    paused:       bool,
    removed:      bool, // signals worker to exit
    visible:      bool, // chart visibility (data still collected when false)
    time_origin:  f64,  // session_t at last reset — history is stored relative to this
    color_idx:    usize, // palette slot — reassigned compactly after any removal

    // cumulative stats
    sent:         u64,
    received:     u64,
    min_rtt:      f64, // f64::MAX until first reply
    max_rtt:      f64,
    sum_rtt:      f64,
    last_rtt:     f64, // -1.0 = no data / timeout
    prev_rtt:     f64, // used for jitter calc
    jitter_sum:   f64,
    jitter_count: u64,

    // growing series: (session_elapsed_seconds, rtt_ms)
    // rtt_ms == -1.0 marks a timeout / dropped packet
    history: Vec<(f64, f64)>,
}

impl Host {
    fn new(id: usize, name: String, interval_ms: u64, color_idx: usize) -> Self {
        Self {
            id,
            name,
            ip: None,
            status: Status::Idle,
            interval_ms,
            paused:      false,
            removed:     false,
            visible:     true,
            time_origin: 0.0,
            color_idx,
            sent: 0,
            received: 0,
            min_rtt: f64::MAX,
            max_rtt: 0.0,
            sum_rtt: 0.0,
            last_rtt: -1.0,
            prev_rtt: -1.0,
            jitter_sum: 0.0,
            jitter_count: 0,
            history: Vec::new(),
        }
    }

    fn avg_rtt(&self) -> Option<f64> {
        if self.received == 0 {
            None
        } else {
            Some(self.sum_rtt / self.received as f64)
        }
    }

    fn avg_jitter(&self) -> Option<f64> {
        if self.jitter_count == 0 {
            None
        } else {
            Some(self.jitter_sum / self.jitter_count as f64)
        }
    }

    fn loss_pct(&self) -> f64 {
        if self.sent == 0 {
            0.0
        } else {
            (self.sent - self.received) as f64 / self.sent as f64 * 100.0
        }
    }

    fn record(&mut self, session_t: f64, rtt: Option<f64>) {
        let elapsed = session_t - self.time_origin;
        self.sent += 1;

        if let Some(ms) = rtt {
            self.received += 1;
            if ms < self.min_rtt { self.min_rtt = ms; }
            if ms > self.max_rtt { self.max_rtt = ms; }
            self.sum_rtt += ms;
            if self.prev_rtt >= 0.0 {
                self.jitter_sum += (ms - self.prev_rtt).abs();
                self.jitter_count += 1;
            }
            self.prev_rtt = ms;
            self.last_rtt = ms;
            self.history.push((elapsed, ms));
        } else {
            self.last_rtt = -1.0;
            self.history.push((elapsed, -1.0));
        }
    }

    fn reset(&mut self) {
        self.sent = 0;
        self.received = 0;
        self.min_rtt = f64::MAX;
        self.max_rtt = 0.0;
        self.sum_rtt = 0.0;
        self.last_rtt = -1.0;
        self.prev_rtt = -1.0;
        self.jitter_sum = 0.0;
        self.jitter_count = 0;
        self.history.clear();
    }

    /// Reset stats and re-anchor t=0 to `session_t` (the current session clock).
    fn reset_at(&mut self, session_t: f64) {
        self.reset();
        self.time_origin = session_t;
    }

    fn status_color(&self) -> Color32 {
        self.status.color(self.loss_pct())
    }

    /// Sorted RTT samples (only successful pings)
    fn rtt_samples_sorted(&self) -> Vec<f64> {
        let mut v: Vec<f64> = self.history.iter()
            .filter(|(_, r)| *r >= 0.0)
            .map(|(_, r)| *r)
            .collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v
    }

    /// Percentile by linear interpolation (0.0–1.0)
    fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
        if sorted.is_empty() { return None; }
        if sorted.len() == 1 { return Some(sorted[0]); }
        let idx = p * (sorted.len() - 1) as f64;
        let lo = idx.floor() as usize;
        let hi = (lo + 1).min(sorted.len() - 1);
        Some(sorted[lo] + (idx - lo as f64) * (sorted[hi] - sorted[lo]))
    }

    /// Returns (label, color) for the SLA badge
    fn sla_result(&self, rtt_threshold: f64, loss_threshold: f64) -> (&'static str, Color32) {
        if self.received == 0 {
            return ("  —  ", C::DIM);
        }
        let rtt_ok  = self.avg_rtt().map_or(false, |r| r <= rtt_threshold);
        let loss_ok = self.loss_pct() <= loss_threshold;
        match (rtt_ok, loss_ok) {
            (true,  true)  => (" PASS ", C::OK),
            (false, false) => (" FAIL ", C::ERR),
            _              => (" WARN ", C::WARN),
        }
    }
}

/// Per-host chart colour indexed from a simple, distinct palette.
/// colour_idx is assigned compactly so removing hosts frees slots.
fn host_color(color_idx: usize) -> Color32 {
    const PALETTE: [Color32; 10] = [
        Color32::from_rgb(220,  50,  50), // red
        Color32::from_rgb( 30, 130, 210), // blue
        Color32::from_rgb( 40, 170,  80), // green
        Color32::from_rgb(210, 140,   0), // amber
        Color32::from_rgb(150,  60, 200), // purple
        Color32::from_rgb(  0, 180, 180), // cyan
        Color32::from_rgb(230, 100,   0), // orange
        Color32::from_rgb(180,  30, 120), // magenta
        Color32::from_rgb( 80, 160,  80), // olive green
        Color32::from_rgb(100, 100, 210), // slate blue
    ];
    PALETTE[color_idx % PALETTE.len()]
}

// ─── Palette ─────────────────────────────────────────────────────────────────

struct C;
impl C {
    const BG0:    Color32 = Color32::from_rgb(240, 240, 240); // toolbar / status bar
    const BG1:    Color32 = Color32::from_rgb(243, 243, 243); // central panel
    const BG2:    Color32 = Color32::from_rgb(255, 255, 255); // card background
    const BORDER: Color32 = Color32::from_rgb(200, 200, 200);
    const TEXT:   Color32 = Color32::from_rgb( 32,  32,  32);
    const DIM:    Color32 = Color32::from_rgb(110, 110, 110);
    const INFO:   Color32 = Color32::from_rgb(  0, 102, 204); // Windows blue
    const OK:     Color32 = Color32::from_rgb( 16, 124,  16); // Windows green
    const WARN:   Color32 = Color32::from_rgb(202,  80,  16); // Windows orange
    const ERR:    Color32 = Color32::from_rgb(196,  43,  28); // Windows red
    const ACCENT: Color32 = Color32::from_rgb(  0, 102, 204); // Windows accent blue
}

// ─── Shared State ────────────────────────────────────────────────────────────

type Shared = Arc<Mutex<Vec<Host>>>;
type Logs   = Arc<Mutex<VecDeque<LogEntry>>>;

fn push_log(logs: &mut VecDeque<LogEntry>, host: &str, msg: String, level: LogLevel) {
    if logs.len() >= MAX_LOGS { logs.pop_front(); }
    logs.push_back(LogEntry { time: now_ts(), host: host.to_string(), msg, level });
}

// ─── Ping Worker ─────────────────────────────────────────────────────────────

async fn ping_worker(id: usize, host_name: String, shared: Shared, logs: Logs, session_start: Instant) {
    // ── DNS resolution ──────────────────────────────────────────────────────
    {
        let mut s = shared.lock().unwrap();
        if let Some(h) = s.iter_mut().find(|h| h.id == id) {
            h.status = Status::Resolving;
        }
    }

    let ip = match tokio::net::lookup_host(format!("{}:0", host_name)).await {
        Ok(mut addrs) => match addrs.next() {
            Some(a) => a.ip(),
            None => {
                let msg = "DNS: no addresses returned".to_string();
                mark_error(&shared, &logs, id, &host_name, &msg);
                return;
            }
        },
        Err(e) => {
            mark_error(&shared, &logs, id, &host_name, &format!("DNS error: {e}"));
            return;
        }
    };

    push_log(
        &mut logs.lock().unwrap(),
        &host_name,
        format!("Resolved → {ip}"),
        LogLevel::Info,
    );
    {
        let mut s = shared.lock().unwrap();
        if let Some(h) = s.iter_mut().find(|h| h.id == id) { h.ip = Some(ip); }
    }

    // ── ICMP client ─────────────────────────────────────────────────────────
    let config = match ip {
        IpAddr::V4(_) => Config::default(),
        IpAddr::V6(_) => Config::builder().kind(ICMP::V6).build(),
    };

    let client = match Client::new(&config) {
        Ok(c) => c,
        Err(e) => {
            mark_error(
                &shared,
                &logs,
                id,
                &host_name,
                &format!("Socket error: {e}  →  Run as Administrator"),
            );
            return;
        }
    };

    let mut pinger = client.pinger(ip, PingIdentifier(id as u16)).await;
    pinger.timeout(Duration::from_millis(PING_TIMEOUT_MS));
    let mut seq: u16 = 0;

    // ── Ping loop ────────────────────────────────────────────────────────────
    loop {
        // Read flags without holding the lock during the network call
        let (paused, interval_ms, removed) = {
            let s = shared.lock().unwrap();
            match s.iter().find(|h| h.id == id) {
                Some(h) => (h.paused, h.interval_ms, h.removed),
                None    => return,
            }
        };
        if removed { return; }
        if paused  { sleep(Duration::from_millis(100)).await; continue; }

        let t_ping   = Instant::now();
        let result   = pinger.ping(PingSequence(seq), &[0u8; 16]).await;
        let ping_dur = t_ping.elapsed();
        let session_t = session_start.elapsed().as_secs_f64();

        {
            let mut s  = shared.lock().unwrap();
            let mut lg = logs.lock().unwrap();
            let h = match s.iter_mut().find(|h| h.id == id) {
                Some(h) => h,
                None    => return,
            };

            match result {
                Ok((IcmpPacket::V4(_), dur)) | Ok((IcmpPacket::V6(_), dur)) => {
                    let rtt = dur.as_secs_f64() * 1000.0;
                    h.record(session_t, Some(rtt));
                    h.status = Status::Online;
                    push_log(
                        &mut lg,
                        &host_name,
                        format!("Reply from {ip}  seq={seq}  rtt={rtt:.2} ms"),
                        LogLevel::Ok,
                    );
                }
                Err(surge_ping::SurgeError::Timeout { .. }) => {
                    h.record(session_t, None);
                    h.status = Status::Timeout;
                    push_log(&mut lg, &host_name, format!("Request timeout  seq={seq}"), LogLevel::Warn);
                }
                Err(e) => {
                    h.record(session_t, None);
                    h.status = Status::Error(e.to_string());
                    push_log(&mut lg, &host_name, format!("Error: {e}"), LogLevel::Err);
                }
            }
        }

        seq = seq.wrapping_add(1);
        sleep(Duration::from_millis(interval_ms).saturating_sub(ping_dur)).await;
    }
}

fn mark_error(shared: &Shared, logs: &Logs, id: usize, host: &str, msg: &str) {
    let mut s = shared.lock().unwrap();
    if let Some(h) = s.iter_mut().find(|h| h.id == id) {
        h.status = Status::Error(msg.to_string());
    }
    push_log(&mut logs.lock().unwrap(), host, msg.to_string(), LogLevel::Err);
}

// ─── Application ─────────────────────────────────────────────────────────────

struct MultiPingApp {
    shared:         Shared,
    logs:           Logs,
    rt:             Runtime,
    next_id:        usize,
    session_start:  Instant,

    // toolbar UI state
    host_input:     String,
    interval_ms:    u64,
    sweep_input:    String,

    // SLA thresholds
    sla_rtt_ms:     f64,
    sla_loss_pct:   f64,

    // Report metadata
    network_location: String,
    report_comment:   String,

    // log panel UI state
    log_filter:     String,
    log_autoscroll: bool,
    show_log:       bool,

    status_bar: String,
}

impl MultiPingApp {
    fn new(cc: &eframe::CreationContext) -> Self {
        apply_theme(&cc.egui_ctx);
        Self {
            shared:         Arc::new(Mutex::new(Vec::new())),
            logs:           Arc::new(Mutex::new(VecDeque::new())),
            rt:             Runtime::new().expect("tokio runtime"),
            next_id:        1,
            session_start:  Instant::now(),
            host_input:     String::new(),
            interval_ms:    1_000,
            sweep_input:    String::new(),
            sla_rtt_ms:        50.0,
            sla_loss_pct:      1.0,
            network_location:  String::new(),
            report_comment:    String::new(),
            log_filter:     String::new(),
            log_autoscroll: true,
            show_log:       true,
            status_bar:     "Ready — enter a hostname or IP address above to start monitoring.".into(),
        }
    }

    fn add_host_named(&mut self, name: String) {
        if name.is_empty() { return; }
        {
            let s = self.shared.lock().unwrap();
            if s.iter().any(|h| h.name == name && !h.removed) {
                self.status_bar = format!("'{name}' is already being monitored.");
                return;
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        let color_idx = {
            let s = self.shared.lock().unwrap();
            // find lowest palette index not already in use by an active host
            let used: Vec<usize> = s.iter().filter(|h| !h.removed).map(|h| h.color_idx).collect();
            (0..).find(|i| !used.contains(i)).unwrap_or(0)
        };
        self.shared.lock().unwrap().push(Host::new(id, name.clone(), self.interval_ms, color_idx));
        push_log(&mut self.logs.lock().unwrap(), &name,
            format!("Host added  interval={}ms", self.interval_ms), LogLevel::Info);
        let shared        = Arc::clone(&self.shared);
        let logs          = Arc::clone(&self.logs);
        let session_start = self.session_start;
        self.rt.spawn(async move { ping_worker(id, name, shared, logs, session_start).await; });
        self.status_bar = format!("Added host #{id}.");
    }

    fn add_host(&mut self) {
        let name = self.host_input.trim().to_string();
        self.host_input.clear();
        self.add_host_named(name);
    }

    fn sweep(&mut self) {
        let targets = parse_targets(&self.sweep_input.clone());
        if targets.is_empty() {
            self.status_bar = "No valid targets parsed from sweep input.".into();
            return;
        }
        let n = targets.len();
        for t in targets { self.add_host_named(t); }
        self.status_bar = format!("Sweep: queued {n} targets.");
    }

    fn export_log(&mut self) {
        let logs = self.logs.lock().unwrap();
        let mut out = format!(
            "pinger — Activity Log\nExported: {}\n{}\n\n",
            now_human(),
            "─".repeat(80)
        );
        for e in logs.iter() {
            out.push_str(&format!(
                "[{}]  [{}]  {:25}  {}\n",
                e.time, e.level.label(), e.host, e.msg
            ));
        }
        drop(logs);

        let fname = format!("pinger_log_{}.txt", now_file_ts());
        match std::fs::write(&fname, &out) {
            Ok(_)  => self.status_bar = format!("✓ Log saved → {fname}"),
            Err(e) => self.status_bar = format!("✕ Export failed: {e}"),
        }
    }

    fn export_stats(&mut self) {
        let hosts = self.shared.lock().unwrap();
        let mut out = format!(
            "pinger — Statistics Export\nExported: {}\n\n\
             Host,IP,Status,Sent,Received,Loss %,Min ms,Avg ms,Max ms,Jitter ms\n",
            now_human()
        );
        for h in hosts.iter().filter(|h| !h.removed) {
            let ip  = h.ip.map(|i| i.to_string()).unwrap_or_default();
            let st  = h.status.label();
            let min = if h.min_rtt == f64::MAX { "—".into() } else { format!("{:.2}", h.min_rtt) };
            let avg = h.avg_rtt().map(|v| format!("{:.2}", v)).unwrap_or_else(|| "—".into());
            let max = if h.received == 0 { "—".into() } else { format!("{:.2}", h.max_rtt) };
            let jit = h.avg_jitter().map(|v| format!("{:.2}", v)).unwrap_or_else(|| "—".into());
            out.push_str(&format!(
                "{},{},{},{},{},{:.1},{},{},{},{}\n",
                h.name, ip, st, h.sent, h.received, h.loss_pct(),
                min, avg, max, jit
            ));
        }
        drop(hosts);

        let fname = format!("pinger_stats_{}.csv", now_file_ts());
        match std::fs::write(&fname, &out) {
            Ok(_)  => self.status_bar = format!("✓ Stats saved → {fname}"),
            Err(e) => self.status_bar = format!("✕ Export failed: {e}"),
        }
    }

    fn export_report(&mut self) {
        let hosts: Vec<Host> = {
            let s = self.shared.lock().unwrap();
            s.iter().filter(|h| !h.removed).cloned().collect()
        };
        let duration_s = self.session_start.elapsed().as_secs_f64();
        let svg = generate_report_svg(&hosts);

        // ── SLA table rows ──────────────────────────────────────────────────
        let mut sla_rows = String::new();
        for h in &hosts {
            let (lbl, _) = h.sla_result(self.sla_rtt_ms, self.sla_loss_pct);
            let cls = match lbl.trim() { "PASS" => "pass", "FAIL" => "fail", _ => "warn" };
            let avg = h.avg_rtt().map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into());
            let ip  = h.ip.map(|i| i.to_string()).unwrap_or_default();
            sla_rows.push_str(&format!(
                "<tr><td>{}</td><td>{ip}</td><td>{avg}</td><td>{:.1}%</td>\
                 <td>{:.0} ms</td><td>{:.1}%</td><td class=\"{cls}\">{}</td></tr>\n",
                xml_esc(&h.name), h.loss_pct(), self.sla_rtt_ms, self.sla_loss_pct, lbl.trim()
            ));
        }

        // ── Stats table rows ────────────────────────────────────────────────
        let mut stats_rows = String::new();
        for h in &hosts {
            let ip  = h.ip.map(|i| i.to_string()).unwrap_or_default();
            let min = if h.min_rtt == f64::MAX { "—".into() } else { format!("{:.2}", h.min_rtt) };
            let avg = h.avg_rtt().map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into());
            let max = if h.received == 0 { "—".into() } else { format!("{:.2}", h.max_rtt) };
            let jit = h.avg_jitter().map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into());
            stats_rows.push_str(&format!(
                "<tr><td>{}</td><td>{ip}</td><td>{}</td><td>{}</td><td>{:.1}%</td>\
                 <td>{min}</td><td>{avg}</td><td>{max}</td><td>{jit}</td></tr>\n",
                xml_esc(&h.name), h.sent, h.received, h.loss_pct()
            ));
        }

        let cs_svg  = generate_candlestick_svg(&hosts);
        let loc_str = if self.network_location.is_empty() { "—".into() }
                      else { xml_esc(&self.network_location) };
        let cmt_str = if self.report_comment.is_empty() { "—".into() }
                      else { xml_esc(&self.report_comment).replace('\n', "<br>") };

        let html = format!(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>Network Latency Report</title>
<style>
* {{ box-sizing:border-box; margin:0; padding:0; }}
body {{
  font-family: 'Segoe UI', Arial, sans-serif;
  font-size: 13px;
  color: #1a1a1a;
  background: #f4f4f4;
}}
/* ── Cover banner ── */
.cover {{
  background: #1a1a1a;
  color: #f0f0f0;
  padding: 32px 48px 24px;
  border-bottom: 4px solid #555;
}}
.cover h1 {{
  font-size: 22px;
  font-weight: 600;
  letter-spacing: 0.04em;
  text-transform: uppercase;
  margin-bottom: 4px;
}}
.cover .sub {{
  font-size: 12px;
  color: #aaa;
  letter-spacing: 0.06em;
  text-transform: uppercase;
}}
/* ── Metadata block ── */
.meta-grid {{
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
  gap: 0;
  background: #fff;
  border-bottom: 1px solid #d0d0d0;
}}
.meta-cell {{
  padding: 12px 20px;
  border-right: 1px solid #e8e8e8;
}}
.meta-cell:last-child {{ border-right: none; }}
.meta-label {{
  font-size: 10px;
  text-transform: uppercase;
  letter-spacing: 0.08em;
  color: #888;
  margin-bottom: 3px;
}}
.meta-value {{ font-size: 13px; color: #1a1a1a; font-weight: 500; }}
/* ── Comment block ── */
.comment-block {{
  background: #fff;
  border-left: 3px solid #888;
  margin: 0;
  padding: 12px 20px;
  font-size: 12px;
  color: #444;
  border-bottom: 1px solid #d0d0d0;
}}
/* ── Content area ── */
.content {{ padding: 28px 40px; }}
h2 {{
  font-size: 13px;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.07em;
  color: #1a1a1a;
  border-bottom: 2px solid #1a1a1a;
  padding-bottom: 5px;
  margin: 28px 0 12px;
}}
h2:first-child {{ margin-top: 0; }}
/* ── Tables ── */
table {{
  border-collapse: collapse;
  width: 100%;
  margin-bottom: 8px;
  font-size: 12px;
  background: #fff;
  border: 1px solid #d0d0d0;
}}
th {{
  background: #2c2c2c;
  color: #f0f0f0;
  padding: 7px 14px;
  text-align: left;
  font-weight: 500;
  font-size: 11px;
  letter-spacing: 0.04em;
  text-transform: uppercase;
}}
td {{ padding: 6px 14px; border-bottom: 1px solid #ebebeb; }}
tr:last-child td {{ border-bottom: none; }}
tr:nth-child(even) td {{ background: #fafafa; }}
.pass {{ color: #1a6e1a; font-weight: 600; }}
.fail {{ color: #8b1a1a; font-weight: 600; }}
.warn {{ color: #7a4000; font-weight: 600; }}
/* ── Chart caption ── */
.caption {{
  font-size: 11px;
  color: #666;
  margin: 6px 0 14px;
}}
</style>
</head>
<body>

<div class="cover">
  <div class="sub">pinger</div>
  <h1>Network Latency Report</h1>
</div>

<div class="meta-grid">
  <div class="meta-cell">
    <div class="meta-label">Generated</div>
    <div class="meta-value">{dt}</div>
  </div>
  <div class="meta-cell">
    <div class="meta-label">Network Location</div>
    <div class="meta-value">{loc}</div>
  </div>
  <div class="meta-cell">
    <div class="meta-label">Session Duration</div>
    <div class="meta-value">{dur:.0} s &nbsp;/&nbsp; {dur_m:.1} min</div>
  </div>
  <div class="meta-cell">
    <div class="meta-label">Hosts Monitored</div>
    <div class="meta-value">{n_hosts}</div>
  </div>
  <div class="meta-cell">
    <div class="meta-label">SLA Thresholds</div>
    <div class="meta-value">RTT &lt; {sla_rtt:.0} ms &nbsp;|&nbsp; Loss &lt; {sla_loss:.2}%</div>
  </div>
</div>

<div class="comment-block"><strong>Notes:</strong>&nbsp; {cmt}</div>

<div class="content">

<h2>SLA Results</h2>
<table>
  <tr><th>Host</th><th>IP</th><th>Avg RTT</th><th>Loss</th>
      <th>RTT Threshold</th><th>Loss Threshold</th><th>Result</th></tr>
  {sla_rows}
</table>

<h2>Detailed Statistics</h2>
<table>
  <tr><th>Host</th><th>IP</th><th>Sent</th><th>Recv</th><th>Loss %</th>
      <th>Min (ms)</th><th>Avg (ms)</th><th>Max (ms)</th><th>Jitter (ms)</th></tr>
  {stats_rows}
</table>

<h2>RTT Over Time</h2>
<div class="caption">X-axis: session time (s) &nbsp;|&nbsp; Y-axis: round-trip time (ms)</div>
{svg}

<h2>Latency Distribution</h2>
<div class="caption">
  Whiskers&nbsp;=&nbsp;Min / Max &nbsp;|&nbsp;
  Box&nbsp;=&nbsp;P25–P75 (IQR) &nbsp;|&nbsp;
  Vertical line&nbsp;=&nbsp;Median &nbsp;|&nbsp;
  Circle&nbsp;=&nbsp;Mean
</div>
{cs_svg}

</div>
</body>
</html>"#,
            dt       = now_human(),
            dur      = duration_s,
            dur_m    = duration_s / 60.0,
            n_hosts  = hosts.len(),
            sla_rtt  = self.sla_rtt_ms,
            sla_loss = self.sla_loss_pct,
            loc      = loc_str,
            cmt      = cmt_str,
            cs_svg   = cs_svg,
        );

        let fname = format!("pinger_report_{}.html", now_file_ts());
        match std::fs::write(&fname, &html) {
            Ok(_)  => {
                self.status_bar = format!("✓ Report saved → {fname}");
                // Try to open in default browser
                let _ = std::process::Command::new("cmd")
                    .args(["/c", "start", &fname])
                    .spawn();
            }
            Err(e) => self.status_bar = format!("✕ Report failed: {e}"),
        }
    }
}

// ─── Theme ────────────────────────────────────────────────────────────────────

fn apply_theme(ctx: &egui::Context) {
    let mut vis = egui::Visuals::light();
    vis.panel_fill              = C::BG1;
    vis.window_fill             = C::BG2;
    vis.faint_bg_color          = C::BG1;
    vis.extreme_bg_color        = Color32::from_rgb(255, 255, 255);
    vis.window_stroke           = Stroke::new(1.0, C::BORDER);
    vis.widgets.noninteractive.bg_fill   = C::BG1;
    vis.widgets.noninteractive.fg_stroke = Stroke::new(1.0, C::TEXT);
    vis.widgets.inactive.bg_fill         = Color32::from_rgb(225, 225, 225);
    vis.widgets.inactive.fg_stroke       = Stroke::new(1.0, C::TEXT);
    vis.widgets.hovered.bg_fill          = Color32::from_rgb(209, 209, 209);
    vis.widgets.hovered.fg_stroke        = Stroke::new(1.5, C::TEXT);
    vis.widgets.active.bg_fill           = Color32::from_rgb(190, 190, 190);
    vis.widgets.active.fg_stroke         = Stroke::new(1.5, Color32::from_rgb(0, 0, 0));
    vis.selection.bg_fill                = Color32::from_rgba_premultiplied(0, 102, 204, 50);
    vis.hyperlink_color                  = C::ACCENT;
    ctx.set_visuals(vis);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing     = Vec2::new(8.0, 5.0);
    style.spacing.button_padding   = Vec2::new(8.0, 4.0);
    style.spacing.window_margin    = Margin::same(12.0);
    style.text_styles.insert(
        egui::TextStyle::Body,
        egui::FontId::new(13.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        egui::FontId::new(12.0, egui::FontFamily::Monospace),
    );
    ctx.set_style(style);
}

// ─── eframe::App impl ────────────────────────────────────────────────────────

impl eframe::App for MultiPingApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        // Redraw continuously so live data is visible
        ctx.request_repaint_after(Duration::from_millis(REPAINT_INTERVAL_MS));

        // GC: remove hosts flagged as removed, then compact colour indices
        {
            let mut s = self.shared.lock().unwrap();
            let before = s.len();
            s.retain(|h| !h.removed);
            if s.len() != before {
                // reassign colour_idx in stable order so remaining hosts keep
                // their relative ordering and free up lower palette slots
                for (i, h) in s.iter_mut().enumerate() {
                    h.color_idx = i;
                }
            }
        }

        self.ui_toolbar(ctx);
        self.ui_status_bar(ctx);
        self.ui_log_panel(ctx);
        self.ui_latency_panel(ctx);
        self.ui_combined_chart(ctx);
    }
}

// ─── Taskbar Icon ─────────────────────────────────────────────────────────────
// 32×32 RGBA icon — blue tile with a white "P", generated at runtime.

fn make_icon() -> egui::IconData {
    const S: usize = 32;
    let mut px = vec![0u8; S * S * 4];

    let bg   = [0u8,  102, 204, 255]; // #0066CC
    let fg   = [255u8, 255, 255, 255]; // white
    let none = [0u8, 0, 0, 0];

    for y in 0..S {
        for x in 0..S {
            // Rounded-rect mask for background (corner radius ~5)
            let cx = x as i32;
            let cy = y as i32;
            let r  = 5i32;
            let in_tile =
                cx >= r && cx < (S as i32 - r) ||
                cy >= r && cy < (S as i32 - r) ||
                {
                    // corner circles
                    let dx = (cx.min(r - 1) - (r - 1)).max(cx - (S as i32 - r)).max(0);
                    let dy = (cy.min(r - 1) - (r - 1)).max(cy - (S as i32 - r)).max(0);
                    dx * dx + dy * dy <= r * r
                };

            let col = if !in_tile {
                none
            } else {
                // "P" glyph — stem x: 7-11, full height 6-26
                // bowl: top 6-16, x: 11-23
                let stem   = cx >= 7  && cx <= 11 && cy >= 6  && cy <= 26;
                let top    = cx >= 11 && cx <= 23 && cy >= 6  && cy <= 10;
                let mid    = cx >= 11 && cx <= 23 && cy >= 16 && cy <= 20;
                let right  = cx >= 20 && cx <= 24 && cy >= 6  && cy <= 20;
                if stem || top || mid || right { fg } else { bg }
            };

            let i = (y * S + x) * 4;
            px[i]     = col[0];
            px[i + 1] = col[1];
            px[i + 2] = col[2];
            px[i + 3] = col[3];
        }
    }

    egui::IconData { rgba: px, width: S as u32, height: S as u32 }
}

// ─── Panel rendering ─────────────────────────────────────────────────────────

impl MultiPingApp {
    fn ui_toolbar(&mut self, ctx: &egui::Context) {
        // ── Menu bar ─────────────────────────────────────────────────────────
        egui::TopBottomPanel::top("menubar")
            .frame(egui::Frame::none()
                .fill(C::BG0)
                .inner_margin(Margin { left: 6.0, right: 6.0, top: 2.0, bottom: 2.0 }))
            .show(ctx, |ui| {
                egui::menu::bar(ui, |ui| {
                    // ── File ─────────────────────────────────────────────────
                    ui.menu_button("File", |ui| {
                        if ui.button("📋  Export Activity Log…").clicked() {
                            self.export_log(); ui.close_menu();
                        }
                        if ui.button("📊  Export Statistics CSV…").clicked() {
                            self.export_stats(); ui.close_menu();
                        }
                        if ui.button("📄  Generate HTML Report…").clicked() {
                            self.export_report(); ui.close_menu();
                        }
                    });

                    // ── Hosts ────────────────────────────────────────────────
                    ui.menu_button("Hosts", |ui| {
                        let any_active = self.shared.lock().unwrap()
                            .iter().any(|h| !h.paused && !h.removed);
                        if any_active {
                            if ui.button("⏸  Pause All").clicked() {
                                for h in self.shared.lock().unwrap().iter_mut() { h.paused = true; }
                                ui.close_menu();
                            }
                        } else if ui.button("▶  Resume All").clicked() {
                            for h in self.shared.lock().unwrap().iter_mut() { h.paused = false; }
                            ui.close_menu();
                        }
                        if ui.button("↺  Reset All  (t=0)").clicked() {
                            let now_t = self.session_start.elapsed().as_secs_f64();
                            for h in self.shared.lock().unwrap().iter_mut() { h.reset_at(now_t); }
                            self.status_bar = "All statistics reset. t=0 restarted for all hosts.".into();
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui.button(RichText::new("✕  Remove Offline").color(C::WARN)).clicked() {
                            let mut n = 0usize;
                            for h in self.shared.lock().unwrap().iter_mut() {
                                if !h.removed && !matches!(h.status, Status::Online) {
                                    h.removed = true; n += 1;
                                }
                            }
                            self.status_bar = format!("Removed {n} offline host(s).");
                            ui.close_menu();
                        }
                        if ui.button(RichText::new("✕  Remove All").color(C::ERR)).clicked() {
                            for h in self.shared.lock().unwrap().iter_mut() { h.removed = true; }
                            self.status_bar = "All hosts removed.".into();
                            ui.close_menu();
                        }
                    });

                    // ── Tools ────────────────────────────────────────────────
                    ui.menu_button("Tools", |ui| {
                        ui.label(RichText::new("SLA thresholds").size(11.0).color(C::DIM));
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("RTT <").size(11.0));
                            ui.add(egui::DragValue::new(&mut self.sla_rtt_ms)
                                .speed(1.0).clamp_range(1.0f64..=10_000.0).suffix(" ms"));
                        });
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Loss <").size(11.0));
                            ui.add(egui::DragValue::new(&mut self.sla_loss_pct)
                                .speed(0.1).clamp_range(0.0f64..=100.0).suffix(" %"));
                        });
                        ui.separator();
                        ui.label(RichText::new("Report metadata").size(11.0).color(C::DIM));
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Location:").size(11.0));
                            ui.add(egui::TextEdit::singleline(&mut self.network_location)
                                .desired_width(180.0)
                                .hint_text("e.g. Site A — MCC Cabinet 3"));
                        });
                        ui.label(RichText::new("Comment:").size(11.0));
                        ui.add(egui::TextEdit::multiline(&mut self.report_comment)
                            .desired_width(220.0)
                            .desired_rows(3)
                            .hint_text("Commissioning notes, engineer name…"));
                        ui.separator();
                        ui.label(RichText::new("Subnet sweep").size(11.0).color(C::DIM));
                        ui.add(egui::TextEdit::singleline(&mut self.sweep_input)
                            .desired_width(200.0)
                            .hint_text("192.168.1.0/30  or  x.x.x.1-50"));
                        if ui.button("▶  Add All Targets").clicked() {
                            self.sweep(); ui.close_menu();
                        }
                    });

                    // ── Live summary (right-aligned) ─────────────────────────
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (total, online, timeout, err) = {
                            let s = self.shared.lock().unwrap();
                            let a: Vec<_> = s.iter().filter(|h| !h.removed).collect();
                            (a.len(),
                             a.iter().filter(|h| h.status == Status::Online).count(),
                             a.iter().filter(|h| h.status == Status::Timeout).count(),
                             a.iter().filter(|h| matches!(h.status, Status::Error(_))).count())
                        };
                        if total > 0 {
                            if err > 0 {
                                ui.label(RichText::new(format!("✕ {err} err")).size(12.0).color(C::ERR));
                            }
                            if timeout > 0 {
                                ui.label(RichText::new(format!("⚠ {timeout} timeout")).size(12.0).color(C::WARN));
                            }
                            ui.label(RichText::new(format!("● {online}/{total} online"))
                                .size(12.0)
                                .color(if online == total { C::OK } else { C::WARN }));
                        }
                    });
                });
            });

        // ── Compact toolbar ───────────────────────────────────────────────────
        let frame = egui::Frame::none()
            .fill(C::BG0)
            .stroke(Stroke::new(0.0, C::BORDER))
            .inner_margin(Margin { left: 10.0, right: 10.0, top: 6.0, bottom: 6.0 });

        egui::TopBottomPanel::top("toolbar").frame(frame).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("pinger").size(15.0).color(C::TEXT).strong());
                ui.add(egui::Separator::default().spacing(10.0));

                // Host input
                ui.label(RichText::new("Host:").size(12.0).color(C::DIM));
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.host_input)
                        .desired_width(200.0)
                        .hint_text("hostname or IP address"),
                );
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                ui.label(RichText::new("ms:").size(12.0).color(C::DIM));
                ui.add(egui::DragValue::new(&mut self.interval_ms)
                    .speed(50).clamp_range(200u64..=60_000u64));

                if ui.button(RichText::new("  ＋ Add  ").color(C::ACCENT)).clicked() || enter {
                    self.add_host();
                }

                ui.add(egui::Separator::default().spacing(10.0));

                let any_active = self.shared.lock().unwrap()
                    .iter().any(|h| !h.paused && !h.removed);
                if any_active {
                    if ui.button("⏸").on_hover_text("Pause All").clicked() {
                        for h in self.shared.lock().unwrap().iter_mut() { h.paused = true; }
                    }
                } else {
                    if ui.button("▶").on_hover_text("Resume All").clicked() {
                        for h in self.shared.lock().unwrap().iter_mut() { h.paused = false; }
                    }
                }
                if ui.button("↺").on_hover_text("Reset All — restart t=0").clicked() {
                    let now_t = self.session_start.elapsed().as_secs_f64();
                    for h in self.shared.lock().unwrap().iter_mut() { h.reset_at(now_t); }
                    self.status_bar = "All statistics reset. t=0 restarted for all hosts.".into();
                }
            });
        });
    }

    fn ui_status_bar(&mut self, ctx: &egui::Context) {
        let frame = egui::Frame::none()
            .fill(C::BG0)
            .stroke(Stroke::new(1.0, C::BORDER))
            .inner_margin(Margin { left: 14.0, right: 14.0, top: 3.0, bottom: 3.0 });

        egui::TopBottomPanel::bottom("statusbar").frame(frame).show(ctx, |ui| {
            ui.label(
                RichText::new(&self.status_bar)
                    .size(11.0)
                    .color(C::DIM)
                    .monospace(),
            );
        });
    }

    fn ui_log_panel(&mut self, ctx: &egui::Context) {
        let frame = egui::Frame::none()
            .fill(C::BG0)
            .inner_margin(Margin::same(8.0));

        if self.show_log {
            egui::TopBottomPanel::bottom("log_panel")
                .frame(frame)
                .resizable(true)
                .min_height(100.0)
                .max_height(280.0)
                .show(ctx, |ui| {
                    // Header bar
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("📋  Activity Log")
                                .size(12.0)
                                .color(C::TEXT)
                                .strong(),
                        );
                        ui.add(egui::Separator::default().spacing(8.0));

                        ui.label(RichText::new("Filter:").size(11.0).color(C::DIM));
                        ui.add(
                            egui::TextEdit::singleline(&mut self.log_filter)
                                .desired_width(140.0)
                                .hint_text("host or message…"),
                        );
                        if ui.small_button("✕").on_hover_text("Clear filter").clicked() {
                            self.log_filter.clear();
                        }
                        ui.add(egui::Separator::default().spacing(8.0));
                        ui.checkbox(&mut self.log_autoscroll, RichText::new("Auto-scroll").size(11.0));
                        if ui.small_button("Clear").clicked() {
                            self.logs.lock().unwrap().clear();
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("▼ Hide").clicked() {
                                self.show_log = false;
                            }
                            let count = self.logs.lock().unwrap().len();
                            ui.label(
                                RichText::new(format!("{count} entries"))
                                    .size(11.0)
                                    .color(C::DIM),
                            );
                        });
                    });
                    ui.separator();

                    // Log entries
                    let entries: Vec<LogEntry> = {
                        let lock = self.logs.lock().unwrap();
                        let f = self.log_filter.to_lowercase();
                        lock.iter()
                            .filter(|e| {
                                f.is_empty()
                                    || e.host.to_lowercase().contains(&f)
                                    || e.msg.to_lowercase().contains(&f)
                            })
                            .cloned()
                            .collect()
                    };

                    ScrollArea::vertical()
                        .id_source("log_scroll")
                        .stick_to_bottom(self.log_autoscroll)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for e in &entries {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(&e.time)
                                            .size(11.0)
                                            .color(C::DIM)
                                            .monospace(),
                                    );
                                    ui.label(
                                        RichText::new(e.level.label())
                                            .size(11.0)
                                            .color(e.level.color())
                                            .monospace(),
                                    );
                                    ui.label(
                                        RichText::new(format!("{:22}", e.host))
                                            .size(11.0)
                                            .color(C::INFO)
                                            .monospace(),
                                    );
                                    ui.label(
                                        RichText::new(&e.msg)
                                            .size(11.0)
                                            .color(C::TEXT)
                                            .monospace(),
                                    );
                                });
                            }
                        });
                });
        } else {
            // Collapsed bar
            egui::TopBottomPanel::bottom("log_panel_hidden")
                .frame(frame)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        if ui.small_button("▲ Show Log").clicked() {
                            self.show_log = true;
                        }
                        let count = self.logs.lock().unwrap().len();
                        ui.label(
                            RichText::new(format!("{count} log entries"))
                                .size(11.0)
                                .color(C::DIM),
                        );
                    });
                });
        }
    }

    fn ui_combined_chart(&mut self, ctx: &egui::Context) {
        let frame = egui::Frame::none()
            .fill(C::BG1)
            .inner_margin(Margin::same(10.0));

        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let hosts: Vec<Host> = {
                let s = self.shared.lock().unwrap();
                s.iter().filter(|h| !h.removed).cloned().collect()
            };

            if hosts.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new(
                            "No hosts yet.\n\nEnter a hostname or IP address in the toolbar above and press Enter or click  ＋ Add Host.",
                        )
                        .size(14.0)
                        .color(C::DIM),
                    );
                });
                return;
            }

            // Live status strip above chart
            ui.horizontal(|ui| {
                for h in &hosts {
                    let col = host_color(h.color_idx);
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
                    ui.painter().circle_filled(dot.center(), 4.5, col);
                    let rtt = if h.last_rtt < 0.0 {
                        "—".to_string()
                    } else {
                        format!("{:.1} ms", h.last_rtt)
                    };
                    ui.label(
                        RichText::new(format!("{}  {}", h.name, rtt))
                            .size(11.0)
                            .color(C::TEXT),
                    );
                    ui.add_space(10.0);
                }
            });
            ui.add_space(4.0);

            // Compute axis ranges across all hosts
            let all_rtts = hosts.iter()
                .flat_map(|h| h.history.iter().filter(|(_, r)| *r >= 0.0).map(|(_, r)| *r));
            let y_max = all_rtts.fold(10.0_f64, f64::max) * 1.20;

            let x_max = hosts.iter()
                .filter_map(|h| h.history.last().map(|(t, _)| *t))
                .fold(10.0_f64, f64::max);

            let available = ui.available_height();

            Plot::new("combined_chart")
                .height(available)
                .allow_zoom(true)
                .allow_drag(true)
                .allow_scroll(true)
                .show_axes([true, true])
                .show_grid([true, true])
                .x_axis_label("Time (s)")
                .y_axis_label("RTT (ms)")
                .legend(Legend::default().position(egui_plot::Corner::LeftTop))
                .set_margin_fraction(Vec2::new(0.02, 0.08))
                .include_x(0.0)
                .include_x(x_max)
                .include_y(0.0)
                .include_y(y_max)
                .show(ui, |plot_ui| {
                    for h in &hosts {
                        if !h.visible { continue; }
                        let col = host_color(h.color_idx);

                        let hits: Vec<[f64; 2]> = h.history.iter()
                            .filter(|(_, r)| *r >= 0.0)
                            .map(|(t, r)| [*t, *r])
                            .collect();

                        let misses: Vec<[f64; 2]> = h.history.iter()
                            .filter(|(_, r)| *r < 0.0)
                            .map(|(t, _)| [*t, 0.0])
                            .collect();

                        if !hits.is_empty() {
                            plot_ui.line(
                                Line::new(hits)
                                    .color(col)
                                    .width(1.2)
                                    .name(&h.name),
                            );
                        }
                        if !misses.is_empty() {
                            // Same colour as the line, no separate legend entry
                            plot_ui.points(
                                Points::new(misses)
                                    .color(col)
                                    .radius(4.5)
                                    .shape(MarkerShape::Cross)
                                    .name(&h.name),   // same name → merges into same legend item
                            );
                        }
                    }
                });
        });
    }

    fn ui_latency_panel(&mut self, ctx: &egui::Context) {
        let frame = egui::Frame::none()
            .fill(C::BG0)
            .stroke(Stroke::new(1.0, C::BORDER))
            .inner_margin(Margin { left: 10.0, right: 10.0, top: 8.0, bottom: 8.0 });

        egui::TopBottomPanel::bottom("latency_panel")
            .frame(frame)
            .resizable(true)
            .min_height(60.0)
            .default_height(200.0)
            .show(ctx, |ui| {
                let mut to_remove:        Vec<usize>        = Vec::new();
                let mut to_toggle_pause:  Vec<usize>        = Vec::new();
                let mut to_toggle_vis:    Vec<usize>        = Vec::new();
                let mut to_reset:         Vec<usize>        = Vec::new();
                let mut interval_changes: Vec<(usize, u64)> = Vec::new();

                // Clone + sort by avg RTT (fastest first; no-data hosts go to end)
                let mut hosts: Vec<Host> = {
                    let s = self.shared.lock().unwrap();
                    s.iter().filter(|h| !h.removed).cloned().collect()
                };
                hosts.sort_by(|a, b| match (a.avg_rtt(), b.avg_rtt()) {
                    (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
                    (Some(_), None)    => std::cmp::Ordering::Less,
                    (None, Some(_))    => std::cmp::Ordering::Greater,
                    (None, None)       => std::cmp::Ordering::Equal,
                });

                // Shared bar scale: max RTT across all hosts (min 50 ms)
                let scale_max = hosts.iter()
                    .filter(|h| h.received > 0)
                    .map(|h| h.max_rtt)
                    .fold(50.0_f64, f64::max);

                // Header
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Latency  (sorted fastest → slowest)")
                            .size(12.0).color(C::TEXT).strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("scale  0 – {scale_max:.0} ms"))
                                .size(10.0).color(C::DIM).monospace(),
                        );
                    });
                });
                ui.separator();

                if hosts.is_empty() {
                    ui.label(
                        RichText::new("No hosts added yet.").size(11.0).color(C::DIM).italics(),
                    );
                    return;
                }

                let sla_rtt  = self.sla_rtt_ms;
                let sla_loss = self.sla_loss_pct;

                ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for h in &hosts {
                        draw_latency_row(
                            ui, h, scale_max, sla_rtt, sla_loss,
                            &mut to_remove,
                            &mut to_toggle_pause,
                            &mut to_toggle_vis,
                            &mut to_reset,
                            &mut interval_changes,
                        );
                    }
                });

                // Apply mutations
                {
                    let mut s = self.shared.lock().unwrap();
                    for id in &to_remove       { if let Some(h) = s.iter_mut().find(|h| h.id == *id) { h.removed = true; } }
                    for id in &to_toggle_pause { if let Some(h) = s.iter_mut().find(|h| h.id == *id) { h.paused  = !h.paused; } }
                    for id in &to_toggle_vis   { if let Some(h) = s.iter_mut().find(|h| h.id == *id) { h.visible = !h.visible; } }
                    let now_t = self.session_start.elapsed().as_secs_f64();
                    for id in &to_reset { if let Some(h) = s.iter_mut().find(|h| h.id == *id) { h.reset_at(now_t); } }
                    for (id, ms) in &interval_changes { if let Some(h) = s.iter_mut().find(|h| h.id == *id) { h.interval_ms = *ms; } }
                }
            });
    }
}

// ─── Latency Panel Rows ───────────────────────────────────────────────────────

fn draw_latency_row(
    ui:               &mut Ui,
    host:             &Host,
    scale_max:        f64,
    sla_rtt_ms:       f64,
    sla_loss_pct:     f64,
    to_remove:        &mut Vec<usize>,
    to_toggle_pause:  &mut Vec<usize>,
    to_toggle_vis:    &mut Vec<usize>,
    to_reset:         &mut Vec<usize>,
    interval_changes: &mut Vec<(usize, u64)>,
) {
    let chart_col  = host_color(host.color_idx);
    let status_col = host.status_color();
    let loss       = host.loss_pct();

    ui.horizontal(|ui| {
        // ── Status dot ──────────────────────────────────────────────────────
        let (dot, _) = ui.allocate_exact_size(Vec2::splat(14.0), egui::Sense::hover());
        ui.painter().circle_filled(dot.center(), 5.0, status_col);

        // ── Hostname + IP ────────────────────────────────────────────────────
        let name_ip = if let Some(ip) = host.ip {
            format!("{}  ({})", host.name, ip)
        } else {
            host.name.clone()
        };
        let name_color = if host.paused { C::DIM } else { C::TEXT };
        ui.add_sized(
            Vec2::new(220.0, 18.0),
            egui::Label::new(RichText::new(&name_ip).size(12.0).color(name_color)),
        );

        // ── Candlestick bar ──────────────────────────────────────────────────
        draw_candlestick_bar(ui, host, scale_max, chart_col);

        ui.add_space(8.0);

        // ── Stats text ────────────────────────────────────────────────────────
        let min_s = if host.min_rtt == f64::MAX { " —  ".into() } else { format!("{:6.1}", host.min_rtt) };
        let avg_s = host.avg_rtt().map(|v| format!("{:6.1}", v)).unwrap_or_else(|| " —  ".into());
        let max_s = if host.received == 0 { " —  ".into() } else { format!("{:6.1}", host.max_rtt) };
        let jit_s = host.avg_jitter().map(|v| format!("{:5.1}", v)).unwrap_or_else(|| " —  ".into());
        let loss_col = if loss == 0.0 { C::OK } else if loss < 10.0 { C::WARN } else { C::ERR };

        ui.label(RichText::new(format!("min {min_s}")).size(11.0).color(C::OK).monospace());
        ui.label(RichText::new(format!("avg {avg_s}")).size(11.0).color(C::INFO).monospace());
        ui.label(RichText::new(format!("max {max_s}")).size(11.0).color(C::WARN).monospace());
        ui.label(RichText::new(format!("±{jit_s}")).size(11.0).color(C::DIM).monospace());
        ui.label(RichText::new(format!("loss {:5.1}%", loss)).size(11.0).color(loss_col).monospace());

        // ── SLA badge ────────────────────────────────────────────────────────
        let (sla_label, sla_col) = host.sla_result(sla_rtt_ms, sla_loss_pct);
        ui.label(
            RichText::new(sla_label).size(11.0).color(sla_col).strong().monospace(),
        );

        // ── Controls (right-aligned) ──────────────────────────────────────────
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button(RichText::new("✕").color(C::ERR)).on_hover_text("Remove").clicked() {
                to_remove.push(host.id);
            }
            let pl_label = if host.paused { "▶" } else { "⏸" };
            let pl_tip   = if host.paused { "Resume" } else { "Pause" };
            if ui.small_button(pl_label).on_hover_text(pl_tip).clicked() {
                to_toggle_pause.push(host.id);
            }
            if ui.small_button("↺").on_hover_text("Reset stats").clicked() {
                to_reset.push(host.id);
            }
            // Visibility checkbox — col swatch + checkbox
            let mut vis = host.visible;
            if ui.checkbox(&mut vis, "").on_hover_text("Show/hide on chart").changed() {
                to_toggle_vis.push(host.id);
            }
            // Colour swatch so you can identify the chart line
            let (sw, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
            ui.painter().rect_filled(sw, egui::Rounding::same(2.0), chart_col);
            let mut iv = host.interval_ms;
            if ui.add(
                egui::DragValue::new(&mut iv)
                    .speed(50)
                    .clamp_range(200u64..=60_000u64)
                    .suffix(" ms"),
            ).changed() {
                interval_changes.push((host.id, iv));
            }
            ui.label(RichText::new("int:").size(10.0).color(C::DIM));
            if host.paused {
                ui.label(RichText::new("PAUSED").size(10.0).color(C::WARN));
            }
        });
    });

    ui.add_space(2.0);
}

fn draw_candlestick_bar(ui: &mut Ui, host: &Host, scale_max: f64, color: Color32) {
    let bar_w = 300.0_f32;
    let bar_h = 24.0_f32;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(bar_w, bar_h), egui::Sense::hover());
    if !ui.is_rect_visible(rect) { return; }

    let p  = ui.painter();
    let cy = rect.center().y;

    // Background track
    p.rect_filled(rect, egui::Rounding::same(3.0), Color32::from_rgb(235, 235, 238));
    p.rect_stroke(rect, egui::Rounding::same(3.0), Stroke::new(1.0, C::BORDER));

    if host.received == 0 { return; }

    let to_x = |v: f64| -> f32 {
        (rect.left() + (v / scale_max.max(1.0) * rect.width() as f64) as f32)
            .clamp(rect.left(), rect.right())
    };

    let sorted = host.rtt_samples_sorted();
    let min_x  = to_x(host.min_rtt);
    let max_x  = to_x(host.max_rtt).max(min_x + 2.0);
    let p25_x  = Host::percentile(&sorted, 0.25).map(to_x).unwrap_or(min_x);
    let p75_x  = Host::percentile(&sorted, 0.75).map(to_x).unwrap_or(max_x).max(p25_x + 2.0);
    let med_x  = Host::percentile(&sorted, 0.50).map(to_x).unwrap_or(p25_x);
    let mean_x = host.avg_rtt().map(to_x).unwrap_or(med_x);

    let wh = 8.0_f32; // whisker half-height
    let bh = 14.0_f32; // box half-height

    let col_solid = color;
    let col_fill  = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 80);

    // Min whisker line + cap
    p.line_segment([egui::pos2(min_x, cy), egui::pos2(p25_x, cy)],
        Stroke::new(1.5, col_solid));
    p.line_segment([egui::pos2(min_x, cy - wh), egui::pos2(min_x, cy + wh)],
        Stroke::new(1.5, col_solid));

    // Max whisker line + cap
    p.line_segment([egui::pos2(p75_x, cy), egui::pos2(max_x, cy)],
        Stroke::new(1.5, col_solid));
    p.line_segment([egui::pos2(max_x, cy - wh), egui::pos2(max_x, cy + wh)],
        Stroke::new(1.5, col_solid));

    // IQR box body (P25–P75)
    let box_rect = egui::Rect::from_x_y_ranges(p25_x..=p75_x, (cy - bh)..=(cy + bh));
    p.rect_filled(box_rect, egui::Rounding::same(2.0), col_fill);
    p.rect_stroke(box_rect, egui::Rounding::same(2.0), Stroke::new(1.5, col_solid));

    // Median line (inside box)
    p.line_segment([egui::pos2(med_x, cy - bh), egui::pos2(med_x, cy + bh)],
        Stroke::new(2.0, col_solid));

    // Mean circle
    p.circle_stroke(egui::pos2(mean_x, cy), 3.5, Stroke::new(1.5, col_solid));

    // Scale midpoint tick
    let mid_x = rect.left() + rect.width() / 2.0;
    p.line_segment(
        [egui::pos2(mid_x, rect.bottom() - 2.0), egui::pos2(mid_x, rect.bottom())],
        Stroke::new(1.0, C::DIM),
    );
}

// ─── Sweep target parser ─────────────────────────────────────────────────────

/// Parses target strings into a list of hostnames/IPs.
/// Supports:
///   192.168.1.1-50      → last-octet range
///   192.168.1.0/24      → CIDR /24 (.1–.254)
///   a,b,c               → comma-separated list
///   single host/IP      → as-is
fn parse_targets(input: &str) -> Vec<String> {
    let input = input.trim();
    if input.is_empty() { return vec![]; }

    if input.contains(',') {
        return input.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    if let Some(slash) = input.find('/') {
        if let Ok(prefix) = input[slash + 1..].parse::<u8>() {
            let base = &input[..slash];
            let parts: Vec<&str> = base.split('.').collect();
            if parts.len() == 4 {
                // Parse base IP octets
                let oct: Option<Vec<u32>> = parts.iter()
                    .map(|s| s.parse::<u32>().ok())
                    .collect();
                if let Some(oct) = oct {
                    let base_ip: u32 = (oct[0] << 24) | (oct[1] << 16) | (oct[2] << 8) | oct[3];
                    if prefix >= 1 && prefix <= 30 {
                        let mask    = !0u32 << (32 - prefix);
                        let network = base_ip & mask;
                        let bcast   = network | !mask;
                        // yield all host addresses (exclude network and broadcast)
                        let mut out = Vec::new();
                        let mut addr = network + 1;
                        while addr < bcast {
                            out.push(format!("{}.{}.{}.{}",
                                (addr >> 24) & 0xFF,
                                (addr >> 16) & 0xFF,
                                (addr >>  8) & 0xFF,
                                 addr        & 0xFF));
                            addr += 1;
                        }
                        return out;
                    }
                }
            }
        }
        return vec![];
    }

    if let Some(last_dot) = input.rfind('.') {
        if let Some(dash) = input[last_dot..].find('-') {
            let dash_abs   = last_dot + dash;
            let net_prefix = &input[..last_dot];
            let start_s    = &input[last_dot + 1..dash_abs];
            let end_s      = &input[dash_abs + 1..];
            if let (Ok(start), Ok(end)) = (start_s.parse::<u32>(), end_s.parse::<u32>()) {
                if start <= end && end <= 254 {
                    return (start..=end).map(|i| format!("{net_prefix}.{i}")).collect();
                }
            }
        }
    }

    vec![input.to_string()]
}

// ─── HTML Report helpers ──────────────────────────────────────────────────────

fn xml_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn generate_report_svg(hosts: &[Host]) -> String {
    const W: f64 = 1200.0;
    const H: f64 = 420.0;
    const ML: f64 = 58.0;
    const MR: f64 = 20.0;
    const MT: f64 = 30.0;
    const MB: f64 = 44.0;
    let pw = W - ML - MR;
    let ph = H - MT - MB;

    let x_max = hosts.iter()
        .filter_map(|h| h.history.last().map(|(t, _)| *t))
        .fold(10.0_f64, f64::max);
    let y_max = hosts.iter()
        .flat_map(|h| h.history.iter().filter(|(_, r)| *r >= 0.0).map(|(_, r)| *r))
        .fold(10.0_f64, f64::max) * 1.15;

    let sx = |t: f64| -> f64 { ML + (t / x_max) * pw };
    let sy = |r: f64| -> f64 { MT + ph - (r / y_max) * ph };

    let colors = ["#0072BD","#D95319","#208644","#7E2F8E",
                  "#A2142F","#4DBEEE","#77AC30","#00A6A6"];

    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {W} {H}\" \
         style=\"width:100%;max-width:{W}px;background:#fff;\
         border:1px solid #ccc;border-radius:4px;display:block\">\n"
    );

    // Grid + y-axis labels
    for i in 0..=5u32 {
        let v  = y_max * i as f64 / 5.0;
        let gy = sy(v);
        s.push_str(&format!(
            "<line x1=\"{ML}\" y1=\"{gy:.1}\" x2=\"{:.1}\" y2=\"{gy:.1}\" \
             stroke=\"#e8e8e8\" stroke-width=\"1\"/>\n\
             <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" \
             font-size=\"10\" fill=\"#888\">{:.0}</text>\n",
            ML + pw, ML - 4.0, gy + 4.0, v
        ));
    }
    // x-axis labels
    for i in 0..=6u32 {
        let t  = x_max * i as f64 / 6.0;
        let gx = sx(t);
        s.push_str(&format!(
            "<line x1=\"{gx:.1}\" y1=\"{MT}\" x2=\"{gx:.1}\" y2=\"{:.1}\" \
             stroke=\"#e8e8e8\" stroke-width=\"1\"/>\n\
             <text x=\"{gx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" \
             font-size=\"10\" fill=\"#888\">{:.0}</text>\n",
            MT + ph, MT + ph + 14.0, t
        ));
    }

    // Axis border + labels
    s.push_str(&format!(
        "<rect x=\"{ML}\" y=\"{MT}\" width=\"{pw}\" height=\"{ph}\" \
         fill=\"none\" stroke=\"#aaa\" stroke-width=\"1\"/>\n\
         <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" \
         font-size=\"11\" fill=\"#444\">Time (s)</text>\n\
         <text x=\"-{:.1}\" y=\"14\" text-anchor=\"middle\" \
         font-size=\"11\" fill=\"#444\" transform=\"rotate(-90)\">RTT (ms)</text>\n",
        ML + pw / 2.0, H - 6.0,
        MT + ph / 2.0
    ));

    // Host lines
    for (idx, h) in hosts.iter().enumerate() {
        let color = colors[idx % colors.len()];
        let pts: Vec<String> = h.history.iter()
            .filter(|(_, r)| *r >= 0.0)
            .map(|(t, r)| format!("{:.1},{:.1}", sx(*t), sy(*r)))
            .collect();
        if !pts.is_empty() {
            s.push_str(&format!(
                "<polyline points=\"{}\" fill=\"none\" stroke=\"{}\" \
                 stroke-width=\"1.5\" stroke-linejoin=\"round\"/>\n",
                pts.join(" "), color
            ));
        }
        for (t, _) in h.history.iter().filter(|(_, r)| *r < 0.0) {
            s.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" \
                 font-size=\"9\" fill=\"#C42B1C\">✕</text>\n",
                sx(*t), sy(0.0) - 2.0
            ));
        }
    }

    // Legend
    for (idx, h) in hosts.iter().enumerate() {
        let color = colors[idx % colors.len()];
        let lx = ML + 8.0 + idx as f64 * 150.0;
        let ly = MT + 12.0;
        s.push_str(&format!(
            "<line x1=\"{lx:.1}\" y1=\"{ly:.1}\" x2=\"{:.1}\" y2=\"{ly:.1}\" \
             stroke=\"{color}\" stroke-width=\"2\"/>\n\
             <text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" fill=\"#333\">{}</text>\n",
            lx + 18.0, lx + 22.0, ly + 4.0, xml_esc(&h.name)
        ));
    }

    s.push_str("</svg>");
    s
}

fn generate_candlestick_svg(hosts: &[Host]) -> String {
    if hosts.is_empty() { return String::new(); }

    let colors = ["#0072BD","#D95319","#208644","#7E2F8E",
                  "#A2142F","#4DBEEE","#77AC30","#00A6A6"];

    // layout
    let row_h  = 50.0_f64;
    let ml     = 160.0_f64;  // left margin for labels
    let mr     = 30.0_f64;
    let mt     = 30.0_f64;   // top margin (axis label row)
    let mb     = 30.0_f64;
    let w      = 900.0_f64;
    let h      = mt + row_h * hosts.len() as f64 + mb;
    let pw     = w - ml - mr;

    // scale: global max across all hosts
    let scale_max = hosts.iter()
        .filter(|h| h.received > 0)
        .map(|h| h.max_rtt)
        .fold(1.0_f64, f64::max);

    let to_x = |v: f64| -> f64 { ml + (v / scale_max) * pw };

    let mut s = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w:.0} {h:.0}\" \
         style=\"width:100%;max-width:{w:.0}px;background:#fff;\
         border:1px solid #ccc;border-radius:4px;display:block\">\n"
    );

    // x-axis grid + labels
    for i in 0..=5u32 {
        let v  = scale_max * i as f64 / 5.0;
        let gx = to_x(v);
        s.push_str(&format!(
            "<line x1=\"{gx:.1}\" y1=\"{mt}\" x2=\"{gx:.1}\" y2=\"{:.1}\" \
             stroke=\"#eee\" stroke-width=\"1\"/>\n\
             <text x=\"{gx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" \
             font-size=\"10\" fill=\"#888\">{v:.0}</text>\n",
            mt + row_h * hosts.len() as f64, mt - 4.0
        ));
    }
    // x-axis border
    s.push_str(&format!(
        "<line x1=\"{ml}\" y1=\"{mt}\" x2=\"{:.1}\" y2=\"{mt}\" \
         stroke=\"#ccc\" stroke-width=\"1\"/>\n\
         <text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" \
         font-size=\"11\" fill=\"#555\">RTT (ms)</text>\n",
        ml + pw,
        ml + pw / 2.0,
        h - 6.0
    ));

    // One row per host
    for (idx, host) in hosts.iter().enumerate() {
        let color = colors[idx % colors.len()];
        let cy    = mt + row_h * idx as f64 + row_h / 2.0;
        let wh    = 10.0_f64; // whisker half-height
        let bh    = 14.0_f64; // box half-height

        // Row background (alternating)
        if idx % 2 == 1 {
            s.push_str(&format!(
                "<rect x=\"{ml}\" y=\"{:.1}\" width=\"{pw}\" height=\"{row_h}\" fill=\"#f9f9f9\"/>\n",
                mt + row_h * idx as f64
            ));
        }

        // Label (left of bar)
        let label = if let Some(ip) = host.ip {
            format!("{} ({})", xml_esc(&host.name), ip)
        } else {
            xml_esc(&host.name)
        };
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" font-size=\"11\" fill=\"#333\">{label}</text>\n",
            ml - 6.0, cy + 4.0
        ));

        if host.received == 0 {
            s.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" fill=\"#aaa\">no data</text>\n",
                ml + 4.0, cy + 4.0
            ));
            continue;
        }

        let sorted = host.rtt_samples_sorted();
        let min_x  = to_x(host.min_rtt);
        let max_x  = to_x(host.max_rtt).max(min_x + 2.0);
        let p25_x  = Host::percentile(&sorted, 0.25).map(&to_x).unwrap_or(min_x);
        let p75_x  = Host::percentile(&sorted, 0.75).map(&to_x).unwrap_or(max_x).max(p25_x + 2.0);
        let med_x  = Host::percentile(&sorted, 0.50).map(&to_x).unwrap_or(p25_x);
        let mean_x = host.avg_rtt().map(&to_x).unwrap_or(med_x);

        // Whisker tails
        s.push_str(&format!(
            "<line x1=\"{min_x:.1}\" y1=\"{cy:.1}\" x2=\"{p25_x:.1}\" y2=\"{cy:.1}\" \
             stroke=\"{color}\" stroke-width=\"1.5\"/>\n\
             <line x1=\"{min_x:.1}\" y1=\"{:.1}\" x2=\"{min_x:.1}\" y2=\"{:.1}\" \
             stroke=\"{color}\" stroke-width=\"1.5\"/>\n\
             <line x1=\"{p75_x:.1}\" y1=\"{cy:.1}\" x2=\"{max_x:.1}\" y2=\"{cy:.1}\" \
             stroke=\"{color}\" stroke-width=\"1.5\"/>\n\
             <line x1=\"{max_x:.1}\" y1=\"{:.1}\" x2=\"{max_x:.1}\" y2=\"{:.1}\" \
             stroke=\"{color}\" stroke-width=\"1.5\"/>\n",
            cy - wh, cy + wh, cy - wh, cy + wh
        ));
        // IQR box
        s.push_str(&format!(
            "<rect x=\"{p25_x:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" \
             fill=\"{color}\" fill-opacity=\"0.25\" stroke=\"{color}\" stroke-width=\"1.5\"/>\n",
            cy - bh, (p75_x - p25_x).max(2.0), bh * 2.0
        ));
        // Median
        s.push_str(&format!(
            "<line x1=\"{med_x:.1}\" y1=\"{:.1}\" x2=\"{med_x:.1}\" y2=\"{:.1}\" \
             stroke=\"{color}\" stroke-width=\"2.5\"/>\n",
            cy - bh, cy + bh
        ));
        // Mean circle (hollow)
        s.push_str(&format!(
            "<circle cx=\"{mean_x:.1}\" cy=\"{cy:.1}\" r=\"4\" \
             fill=\"none\" stroke=\"{color}\" stroke-width=\"2\"/>\n"
        ));

        // Value labels at right
        let avg_str = host.avg_rtt().map(|v| format!("{v:.1}")).unwrap_or_default();
        let min_str = if host.min_rtt < f64::MAX { format!("{:.1}", host.min_rtt) } else { "—".into() };
        let max_str = if host.received > 0 { format!("{:.1}", host.max_rtt) } else { "—".into() };
        s.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"10\" fill=\"#555\">\
             min {min_str}  avg {avg_str}  max {max_str} ms</text>\n",
            ml + pw + 4.0, cy + 4.0
        ));
    }

    s.push_str("</svg>");
    s
}

// ─── Entry Point ─────────────────────────────────────────────────────────────

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("pinger — Network Monitor")
            .with_inner_size([1_320.0, 840.0])
            .with_min_inner_size([900.0, 580.0])
            .with_icon(make_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "pinger",
        options,
        Box::new(|cc| Box::new(MultiPingApp::new(cc))),
    )
}
