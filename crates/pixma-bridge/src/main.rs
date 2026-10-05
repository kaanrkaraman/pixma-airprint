mod airprint;
mod escl;
mod translate;

use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::Router;
use tokio::sync::Mutex;

use escl::AppState;
use pixma_protocol::discover;

const ESCL_PORT: u16 = 8470;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // PIXMA_PRINTER (IP or hostname, e.g. the printer's Bonjour name) skips mDNS discovery,
    // which can miss the printer on a slow network.
    // Resolved once at startup: restart the service if the printer gets a new address.
    let (model, printer_ip) = match std::env::var("PIXMA_PRINTER") {
        Ok(host) => {
            let addr = tokio::net::lookup_host((host.as_str(), 631))
                .await?
                .find(|a| a.is_ipv4())
                .ok_or_else(|| anyhow::anyhow!("cannot resolve {host}"))?;
            ("Canon G3010 series".to_owned(), addr.ip())
        }
        Err(_) => {
            eprintln!("pixma-bridge: discovering Canon printer...");
            let printers = discover::find_printers(Duration::from_secs(10)).await?;
            let printer = printers
                .into_iter()
                .find(|p| p.scan_capable)
                .ok_or_else(|| anyhow::anyhow!("no scan-capable Canon printer found"))?;
            (printer.model, printer.ip)
        }
    };

    eprintln!("pixma-bridge: using {model} at {printer_ip}");

    let (scan_done_tx, scan_done_rx) = tokio::sync::watch::channel(());

    let state = AppState {
        uuid: airprint::UUID.to_owned(),
        printer_ip,
        scanning: Arc::new(AtomicBool::new(false)),
        active_job_id: Arc::new(Mutex::new(None)),
        jobs: Arc::new(Mutex::new(HashMap::new())),
        scan_done: scan_done_rx,
        scan_done_tx: Arc::new(scan_done_tx),
    };

    // Own name: the printer already advertises its model name on _ipp._tcp.
    let name = std::env::var("PIXMA_NAME").unwrap_or_else(|_| format!("{model} AirPrint"));

    // Advertise eSCL service (more reliable than the mdns-sd crate)
    let _mdns_process = advertise(
        &name,
        "_uscan._tcp",
        None,
        &[
            "txtvers=1".to_owned(),
            "vers=2.0".to_owned(),
            format!("ty={model}"),
            "rs=eSCL".to_owned(),
            "pdl=image/jpeg".to_owned(),
            "cs=color,grayscale".to_owned(),
            "is=platen".to_owned(),
            format!("uuid={}", state.uuid),
        ],
    )?;

    eprintln!("pixma-bridge: advertising eSCL scanner on port {ESCL_PORT}");

    // AirPrint: _universal subtype + URF key are what iOS/macOS look for.
    let _airprint_process = advertise(
        &name,
        "_ipp._tcp",
        Some("_universal"),
        &[
            "txtvers=1".to_owned(),
            "qtotal=1".to_owned(),
            "rp=ipp/print".to_owned(),
            format!("ty={model}"),
            format!("product=({model})"),
            "note=".to_owned(),
            "priority=0".to_owned(),
            "pdl=image/urf,image/pwg-raster".to_owned(),
            format!("URF={}", airprint::URF),
            "Color=T".to_owned(),
            "Duplex=F".to_owned(),
            "Copies=T".to_owned(),
            "Scan=T".to_owned(),
            "kind=document,envelope,photo".to_owned(),
            format!("UUID={}", airprint::UUID),
            format!("adminurl=http://{printer_ip}/"),
        ],
    )?;

    eprintln!("pixma-bridge: advertising AirPrint printer on port {ESCL_PORT}");

    // Start HTTP server
    let app = Router::new()
        .route("/eSCL/ScannerCapabilities", get(escl::get_capabilities))
        .route("/eSCL/ScannerStatus", get(escl::get_status))
        .route("/eSCL/ScanJobs", post(escl::create_scan_job))
        .route(
            "/eSCL/ScanJobs/{job_id}/NextDocument",
            get(escl::get_next_document),
        )
        .route("/eSCL/ScanJobs/{job_id}", delete(escl::delete_scan_job))
        .route(
            "/ipp/print",
            post(airprint::handle).layer(DefaultBodyLimit::max(512 * 1024 * 1024)),
        )
        .with_state(state);

    // Catch-all fallback to log unhandled requests
    let app = app.fallback(|req: axum::extract::Request| async move {
        eprintln!("[escl] UNHANDLED: {} {}", req.method(), req.uri());
        StatusCode::NOT_FOUND
    });

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{ESCL_PORT}")).await?;
    eprintln!("pixma-bridge: eSCL server listening on port {ESCL_PORT}");

    axum::serve(listener, app).await?;
    Ok(())
}

/// Publish a Bonjour service through the OS's own publisher (macOS `dns-sd`, Linux `avahi-publish`).
/// The advert lives as long as the returned child process.
fn advertise(
    name: &str,
    service: &str,
    subtype: Option<&str>,
    txt: &[String],
) -> std::io::Result<std::process::Child> {
    let port = ESCL_PORT.to_string();
    let mut cmd;
    if cfg!(target_os = "macos") {
        cmd = Command::new("dns-sd");
        let ty = match subtype {
            Some(sub) => format!("{service},{sub}"),
            None => service.to_owned(),
        };
        cmd.args(["-R", name, &ty, "local.", &port]);
    } else {
        cmd = Command::new("avahi-publish");
        cmd.arg("-s");
        if let Some(sub) = subtype {
            cmd.arg(format!("--subtype={sub}._sub.{service}"));
        }
        cmd.args([name, service, &port]);
    }
    cmd.args(txt)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
}
