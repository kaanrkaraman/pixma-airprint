# pixma-airprint

**AirPrint and AirScan for Canon PIXMA G-series printers that don't support them.**

The Canon PIXMA G3010 series (sold as G3410, G3416, …) speaks IPP Everywhere but
is not an AirPrint printer: it doesn't advertise or accept Apple raster (URF), so
iPhones and iPads can't see it. It also has no eSCL, so macOS Image Capture can't
scan from it. Canon's macOS drivers stop at older releases.

This bridge runs on any always-on Mac or Linux machine on the same network and
makes the printer show up everywhere as a normal AirPrint printer + AirScan scanner:

- **Print from iPhone, iPad and Mac** — appears in the standard print sheet, with color/grayscale, copies and quality.
- **Scan from Image Capture / Preview** — native macOS scanning, no Canon software.
- **No drivers on clients** — nothing to install on the phone or other Macs.

Based on [pdrgds/pixma-rs](https://github.com/pdrgds/pixma-rs), which did the
hard part: reverse-engineering Canon's CHMP scanning protocol. This project adds the
AirPrint side and Linux support.

## How it works

```
 iPhone / iPad / Mac
   │  AirPrint (IPP + Apple raster)        AirScan (eSCL)
   ▼                                        ▼
 pixma-bridge  :8470  ── advertised over Bonjour as one printer + scanner
   │  IPP + PWG raster, port 631           CHMP over HTTP, port 80
   ▼                                        ▼
 Canon PIXMA (Wi-Fi)
```

- **Printing** — an IPP proxy. Print jobs in Apple raster (URF) are converted
  on the fly to PWG raster, which the printer accepts. Everything else (job
  status, cancel, printer state) passes straight through. Printer attributes are
  patched to add what AirPrint clients require (`urf-supported`, UUID, …).
- **Scanning** — pixma-rs's eSCL-to-CHMP translator.
- **Discovery** — Bonjour adverts via `dns-sd` on macOS and `avahi-publish` on Linux.

## Supported printers

| Printer | Status |
|---------|--------|
| Canon PIXMA G3010 series (G3010, G3410, G3416) | Print + scan tested |
| Canon PIXMA G2010, G4010, G3020, G2020 | Scanning should work (same CHMP protocol); printing untested |

The printer must be on Wi-Fi and reachable from the bridge machine.

## Install

Needs [Rust](https://rustup.rs). The bridge machine must stay on and awake for
the printer to be visible.

Give the printer's IP address or Bonjour hostname so the bridge doesn't have to
rely on discovery. Find it with `dns-sd -B _ipp._tcp` (macOS) or
`avahi-browse -rt _ipp._tcp` (Linux), or on the printer's network settings page.

### macOS

```bash
git clone https://github.com/kaanrkaraman/pixma-airprint.git
cd pixma-airprint
installer/macos-service.sh install <printer-ip-or-hostname>
```

Runs as a per-user LaunchAgent (no sudo), starts at login, restarts on crash.
macOS will ask for **Local Network** access the first time — allow it, or the
bridge can't reach the printer.

- Log: `~/Library/Logs/pixma-airprint.log`
- Remove: `installer/macos-service.sh uninstall`

### Linux (Ubuntu / Debian)

```bash
git clone https://github.com/kaanrkaraman/pixma-airprint.git
cd pixma-airprint
installer/linux-service.sh install <printer-ip-or-hostname>
```

Installs a systemd service (needs sudo) and Avahi if missing. Starts at boot.

- Log: `journalctl -u pixma-airprint -f`
- Remove: `installer/linux-service.sh uninstall`

Only run **one** bridge per network — two would advertise the same printer.

### Custom name

The printer shows up as "Canon G3010 series AirPrint" by default. To change it:

```bash
PIXMA_NAME="Canon G3416" installer/macos-service.sh install <printer>
```

## Use

- **iPhone / iPad** — Share → Print, pick the printer.
- **Mac** — System Settings → Printers & Scanners → Add Printer, pick the one
  marked *AirPrint*. Scanning: Image Capture or Preview → File → Import from Scanner.

### Printing from outside home

Bonjour doesn't cross VPNs. With the bridge on a [Tailscale](https://tailscale.com)
node, add a second queue on a Mac pointing at the bridge directly:

```bash
lpadmin -p Canon_Remote -E -v ipp://<bridge-tailscale-name>:8470/ipp/print -m everywhere
```

## Troubleshooting

- **"No route to host" in the log on macOS** — Local Network access was denied.
  Allow it in System Settings → Privacy & Security → Local Network, then restart the service.
- **Printer not found after it changed IP** — the address is resolved once at
  startup. Use the Bonjour hostname (`XXXXXXXX.local`) or a DHCP reservation,
  and restart the service.
- **Blank pages from iPhone** — fixed: iOS uses a URF "fill rest of line" code
  that the printer rejects in PWG raster; the converter expands it.

## Development

```bash
cargo test -p pixma-bridge        # includes URF→PWG conversion against macOS reference output
cargo run --release -p pixma-bridge
```

The bridge listens on port 8470. `PIXMA_PRINTER` sets the printer address
(otherwise it is discovered), `PIXMA_NAME` the advertised name.

The workspace also contains pixma-rs's CLI (`pixma discover | scan | print`) and
protocol notes in `docs/`.

## Legal

The CHMP protocol work comes from pixma-rs, developed through clean-room reverse
engineering of network traffic from a personally owned device, for
interoperability. No Canon software was decompiled or redistributed. Not
affiliated with Canon or Apple; AirPrint and AirScan are Apple trademarks.

## License

MIT OR Apache-2.0, same as pixma-rs.
