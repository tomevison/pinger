# pinger — Windows GUI

Real-time multi-host network latency monitor with a native Windows GUI. Built with Rust + egui.

---

## Features

- **Live RTT line chart** — all hosts on one shared chart, each in a distinct colour
- **Packet loss markers** (✕) on the chart for every dropped/timed-out ping
- **Latency panel** — per-host candlestick bar (Min / P25–P75 IQR / Median / Mean / Max) sorted fastest → slowest
- **Statistics** per host: Min · Avg · Max · Jitter · Loss % · Sent/Received
- **SLA badges** (PASS / WARN / FAIL) per host against configurable RTT and loss thresholds
- **Per-host controls**: Pause / Resume, Reset stats, Remove, show/hide on chart, live-editable interval
- **Hosts menu**: Pause All / Resume All, Reset All, Remove Offline, Remove All
- **Activity Log panel**: timestamped per-ping log with filter, auto-scroll, hide/show, and clear
- **Subnet sweep**: add a CIDR range (`192.168.1.0/24`) or last-octet range (`x.x.x.1-50`) in one click
- **File → Export Activity Log** → `.txt`
- **File → Export Statistics CSV** → `.csv`
- **File → Generate HTML Report** → self-contained `.html` with RTT chart, candlestick distribution, SLA table, and full stats
- **File → Export PDF Report** → converts the HTML report to PDF via Edge/Chrome headless
- Report filename: `<project>-REP-000 YYYYMMDD Network Latency Report`
- Light Windows-style theme; no console window in release builds

---

## UI Layout

```
┌─────────────────────────────────────────────────────────────────────┐
│  File  Hosts  Tools              ● 3/3 online                        │  ← menu bar
├─────────────────────────────────────────────────────────────────────┤
│  pinger │ Host: [___________] ms:[1000] [  ＋ Add  ]  ⏸  ↺          │  ← toolbar
├─────────────────────────────────────────────────────────────────────┤
│  Latency  (sorted fastest → slowest)                  scale 0–50 ms │
│  ● 1.1.1.1 (1.1.1.1)  [═══════|══╪══╪══════════════]               │
│    min  8.1  avg  9.3  max 14.2  ±0.6  loss   0.0%   PASS  ⏸ ↺ □ ✕ │
│  ● 8.8.8.8 (8.8.8.8)  [══════════|═════════════════]               │
│    min 11.2  avg 12.1  max 16.3  ±0.8  loss   0.0%   PASS  ⏸ ↺ □ ✕ │
├─────────────────────────────────────────────────────────────────────┤
│  (RTT over time — combined chart, zoomable/draggable)               │  ← central panel
├─────────────────────────────────────────────────────────────────────┤
│  📋  Activity Log  [Filter: ______] [Auto-scroll ✓] [Clear] [▼Hide] │
│  10:24:01.123  OK    1.1.1.1          Reply from 1.1.1.1  rtt=8.3ms │
│  10:24:01.456  OK    8.8.8.8          Reply from 8.8.8.8  rtt=12.1ms│
│  10:24:02.789  WARN  192.168.1.254    Request timeout  seq=1         │
├─────────────────────────────────────────────────────────────────────┤
│  Ready — enter a hostname or IP address above to start monitoring.  │  ← status bar
└─────────────────────────────────────────────────────────────────────┘
```

---

## Menus

### File
| Item | Output |
|------|--------|
| Export Activity Log… | `<project>-REP-000 YYYYMMDD Network Latency Report.txt` |
| Export Statistics CSV… | `pinger_stats_YYYYMMDD_HHMMSS.csv` |
| Generate HTML Report… | `<project>-REP-000 YYYYMMDD Network Latency Report.html` (opens in browser) |
| Export PDF Report… | `<project>-REP-000 YYYYMMDD Network Latency Report.pdf` (requires Edge or Chrome) |

### Hosts
- **Pause All / Resume All** — toggle all host workers
- **Reset All** — clear stats and restart t=0 for all hosts
- **Remove Offline** — remove any host not currently responding
- **Remove All** — clear all hosts

### Tools
- **SLA thresholds** — set RTT < X ms and Loss < Y% for PASS/WARN/FAIL badges
- **Report metadata** — Project number, Network location, and free-text comment included in the HTML/PDF report
- **Subnet sweep** — enter a CIDR or range and add all targets at once

---

## Requirements

- **Rust 1.74+** — install via [rustup.rs](https://rustup.rs)
- **Windows 10/11** — Linux/macOS work but require root for ICMP
- **Administrator privileges** — raw ICMP sockets need elevated access
- **PDF export**: Microsoft Edge (built-in on Windows 11) or Google Chrome

---

## Build & Run

```powershell
# Open an Administrator terminal, then:
cd pinger

# Debug build (shows console window)
cargo run

# Release build (no console window, optimised)
cargo build --release

# Run as Administrator (required for ICMP)
.\target\release\pinger.exe
```

> **Note:** On Windows, right-click the `.exe` and choose **Run as administrator**, or
> open an elevated PowerShell/Command Prompt and run it from there.

---

## Keyboard / Mouse

| Action | How |
|--------|-----|
| Add host | Type in Host field, press **Enter** or click **＋ Add** |
| Pause a host | Click **⏸** on the host row |
| Resume a host | Click **▶** on the host row |
| Edit interval | Drag the **ms** value on the host row |
| Show/hide on chart | Checkbox on the host row |
| Filter log | Type in the log Filter box |
| Zoom chart | Scroll wheel / pinch |
| Pan chart | Click-drag |
| Export log | File → Export Activity Log |
| Export stats | File → Export Statistics CSV |
| Generate report | File → Generate HTML Report |
| Export PDF | File → Export PDF Report |
| Resize log panel | Drag the divider between log and latency panels |

---

## Colour Guide

### Status / log colours

| Colour | Meaning |
|--------|---------|
| Green `#107c10` | Online · zero packet loss |
| Orange `#ca5010` | Some packet loss (< 20%) · warning |
| Red `#c42b1c` | Timeout · error · packet loss ≥ 20% |
| Blue `#0066cc` | Info · resolving DNS |
| Grey `#6e6e6e` | Idle / dim text |

### Chart line colours (host palette)

Each host is assigned a distinct colour from a 10-slot palette (red, blue, green, amber, purple, cyan, orange, magenta, olive, slate blue) in the order hosts are added.

### SLA badge colours

| Badge | Colour | Condition |
|-------|--------|-----------|
| **PASS** | Green | Avg RTT ≤ threshold **and** loss ≤ threshold |
| **WARN** | Orange | One threshold exceeded |
| **FAIL** | Red | Both thresholds exceeded |

---

## HTML / PDF Report

The report includes:

- **Cover** with project number, generated timestamp, network location, session duration, host count, SLA thresholds, and free-text notes
- **SLA Results table** — per-host PASS / WARN / FAIL verdict
- **Detailed Statistics table** — Sent, Recv, Loss %, Min, Avg, Max, Jitter
- **RTT Over Time chart** — SVG line chart
- **Latency Distribution chart** — SVG candlestick boxes (Min/P25–P75/Median/Mean/Max)

PDF export uses Edge or Chrome in headless mode to print the HTML to PDF.  
File naming: `<project>-REP-000 YYYYMMDD Network Latency Report.(html|pdf)`  
If no project number is set, the prefix defaults to `000`.

---

## Dependencies

| Crate | Purpose |
|-------|---------|
| `eframe` / `egui` | Native GUI framework |
| `egui_plot` | RTT line chart |
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
| PDF export — "neither found" | Install Microsoft Edge or Google Chrome |
| PDF export — exit code error | Ensure the HTML report was saved first; try generating HTML report first |
