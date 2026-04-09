# ◈ MULTIPING — Windows GUI

Real-time multi-host network monitor with a native Windows GUI. Built with Rust + egui.

---

## Features

- **Live RTT line charts** per host — colour-coded green → cyan → amber → red by latency
- **Packet loss** markers (✕) on the chart for every dropped/timed-out ping
- **Statistics panel** per host: Last RTT · Min · Avg · Max · Jitter · Loss % · Sent/Received
- **Per-host controls**: Pause / Resume, Reset stats, Remove, live-editable Interval
- **Global controls**: Pause All / Resume All, Reset All, Remove All
- **Activity Log panel**: timestamped per-ping log with filter, auto-scroll, and clear
- **Export Log** → `.txt` file  |  **Export Stats** → `.csv` file
- Dark NOC-style theme; no console window in release builds

---

## UI Layout

```
┌────────────────────────────────────────────────────────────────────┐
│  ◈ MULTIPING  | Host: [___________] ms:[1000] [+ Add Host]         │
│               | [⏸ Pause All] [↺ Reset All] [✕ Remove All]         │
│               | [📋 Export Log] [📊 Export CSV]    ● 3/3 online     │
├────────────────────────────────────────────────────────────────────┤
│  ● 8.8.8.8 (8.8.8.8)  ● online              [⏸ Pause] [↺] [✕]     │
│  LAST RTT │ MIN  │ AVG  │ MAX  │ JITTER │ LOSS │ SENT  │ RECV      │
│  12.1 ms  │ 10.8 │ 11.9 │ 15.3 │  0.8   │ 0.0% │  100  │  100      │
│  ┌──────────────────────────────────────────────────────────────┐  │
│  │  ▁▂▁▁▂▁▃▂▁▁▂▁▁▂▁▃▂▁▁▂▁▁▂▁▃▂▁▁▂▁▁▂▁▃▂▁▁▂▁▁▂▁▃▂▁▁▂▁▁▂▁▃▂▁▁▂  │  │
│  └──────────────────────────────────────────────────────────────┘  │
├────────────────────────────────────────────────────────────────────┤
│  📋 Activity Log  [Filter: ______] [Auto-scroll ✓] [Clear] [▼Hide] │
│  10:24:01.123  OK    8.8.8.8         Reply 8.8.8.8  seq=1 rtt=12.1ms│
│  10:24:01.456  OK    1.1.1.1         Reply 1.1.1.1  seq=1 rtt=8.3ms │
│  10:24:02.789  WARN  192.168.1.254   Request timeout  seq=1          │
├────────────────────────────────────────────────────────────────────┤
│  Ready — enter a hostname or IP address above to start monitoring.  │
└────────────────────────────────────────────────────────────────────┘
```

---

## Requirements

- **Rust 1.74+** — install via [rustup.rs](https://rustup.rs)
- **Windows** (tested on 10/11) — Linux/macOS also work but require root for ICMP
- **Administrator privileges** — raw ICMP sockets need elevated access

---

## Build & Run

```powershell
# 1. Clone/copy the project, open an Administrator terminal, then:
cd multiping

# 2. Build (debug — shows console window with logs)
cargo run -- 

# 3. Build release (no console window, optimised)
cargo build --release

# 4. Run as Administrator (required for ICMP)
# Right-click → "Run as administrator"
.\target\release\multiping.exe
```

> **Note:** On Windows, right-click the `.exe` and choose **Run as administrator**, or
> open an elevated PowerShell/Command Prompt and run it from there.

---

## Keyboard / Mouse

| Action | How |
|--------|-----|
| Add host | Type in Host field, press **Enter** or click **+ Add Host** |
| Pause a host | Click **⏸ Pause** on the host card |
| Edit interval | Drag the **ms** value on the host card |
| Filter log | Type in the log Filter box |
| Export log | Click **📋 Export Log** in toolbar (saves `.txt` next to the exe) |
| Export stats | Click **📊 Export CSV** in toolbar (saves `.csv` next to the exe) |
| Resize log panel | Drag the divider between log and host panels |

---

## Colour Guide

| Colour | Meaning |
|--------|---------|
| 🟢 Green | RTT < 30 ms · zero packet loss |
| 🔵 Teal | RTT < 80 ms |
| 🟡 Amber | RTT < 200 ms · or some packet loss |
| 🔴 Red | RTT ≥ 200 ms · timeout · error |
| ✕ Red cross on chart | Dropped / timed-out packet |

---

## Dependencies

| Crate | Purpose |
|-------|---------|
| `eframe` / `egui` | Native GUI framework |
| `egui_plot` | RTT line charts |
| `surge-ping` | Async ICMP ping |
| `tokio` | Async runtime |
| `chrono` | Timestamps |

---

## Troubleshooting

| Problem | Fix |
|---------|-----|
| "Socket error: Access denied" | Run as Administrator |
| "DNS error: no addresses" | Check hostname spelling / network |
| No replies but no error | Target may firewall ICMP (e.g. some cloud VMs) |
| Chart not updating | Check interval value; host may be paused |
