//! AirPrint front for the printer: an IPP proxy that adds the AirPrint bits the
//! firmware lacks (URF capabilities) and converts Apple raster (image/urf) jobs
//! to PWG raster, which the printer accepts natively.
//!
//! URF and PWG raster share the same compressed page encoding, so conversion is
//! a header rewrite plus copying page data, expanding URF's one extra opcode.

use std::net::IpAddr;

use anyhow::{anyhow, bail, ensure, Context};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::escl::AppState;

/// Stable identity shared by the AirPrint and eSCL adverts so macOS pairs them as one device.
pub const UUID: &str = "6a3f2c1e-5b7d-4e9a-8c20-00183b935b11";
/// AirPrint capability string (TXT `URF=` and IPP `urf-supported`). The printer is 600 dpi, simplex only.
pub const URF: &str = "V1.4,CP1,DM1,IS1,MT1-2-3,OB10,PQ4-5,RS600,SRGB24,W8";

const PRINTER_PORT: u16 = 631;
const OP_GET_JOB_ATTRIBUTES: u16 = 0x0009;
const OP_GET_JOBS: u16 = 0x000A;
const OP_GET_PRINTER_ATTRIBUTES: u16 = 0x000B;
const GROUP_OPERATION: u8 = 0x01;
const GROUP_PRINTER: u8 = 0x04;
const TAG_END: u8 = 0x03;
const TAG_KEYWORD: u8 = 0x44;
const TAG_URI: u8 = 0x45;
const TAG_MIME: u8 = 0x49;

/// POST /ipp/print
pub async fn handle(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost")
        .to_owned();
    match proxy(state.printer_ip, &host, &body).await {
        Ok(resp) => ([(header::CONTENT_TYPE, "application/ipp")], resp).into_response(),
        Err(e) => {
            eprintln!("[ipp] error: {e:#}");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

// The whole job is buffered in RAM; stream request/response for low-memory hosts.
async fn proxy(printer_ip: IpAddr, host: &str, body: &[u8]) -> anyhow::Result<Vec<u8>> {
    let (mut req, data) = Ipp::parse(body).context("bad IPP request")?;
    let op = u16::from_be_bytes([req.head[2], req.head[3]]);
    req.rewrite_uris(&format!("{printer_ip}:{PRINTER_PORT}"));

    // Validate-Job / Create-Job carry the format but no data; rewrite the format, convert only data.
    let is_urf = req
        .attr(GROUP_OPERATION, "document-format")
        .is_some_and(|a| a.values[0].1 == b"image/urf");
    let data = if is_urf {
        req.set(GROUP_OPERATION, "document-format", TAG_MIME, &["image/pwg-raster"]);
        if data.is_empty() { Vec::new() } else { urf_to_pwg(data)? }
    } else {
        data.to_vec()
    };
    let note = format!("{} bytes{}", data.len(), if is_urf { " urf->pwg" } else { "" });

    let mut out = req.to_bytes();
    out.extend(data);
    let raw = post_to_printer(printer_ip, &out).await?;

    let (mut resp, rest) = Ipp::parse(&raw).context("bad IPP response from printer")?;
    let status = u16::from_be_bytes([resp.head[2], resp.head[3]]);
    let polling = matches!(op, OP_GET_PRINTER_ATTRIBUTES | OP_GET_JOB_ATTRIBUTES | OP_GET_JOBS);
    if !polling || status >= 0x0400 {
        eprintln!("[ipp] op 0x{op:04x} {note} -> status 0x{status:04x} {}", resp.summary());
    }
    resp.rewrite_uris(host);
    if op == OP_GET_PRINTER_ATTRIBUTES {
        advertise_airprint(&mut resp, host);
    }
    let mut bytes = resp.to_bytes();
    bytes.extend_from_slice(rest);
    Ok(bytes)
}

/// Patch the printer's attributes so iOS/macOS accept it as an AirPrint printer.
fn advertise_airprint(r: &mut Ipp, host: &str) {
    if !r.groups.iter().any(|g| g.tag == GROUP_PRINTER) {
        return; // error response
    }
    let uri = format!("ipp://{host}/ipp/print");
    r.set(GROUP_PRINTER, "printer-uri-supported", TAG_URI, &[&uri]);
    r.set(GROUP_PRINTER, "uri-security-supported", TAG_KEYWORD, &["none"]);
    r.set(GROUP_PRINTER, "uri-authentication-supported", TAG_KEYWORD, &["none"]);
    r.set(GROUP_PRINTER, "printer-uuid", TAG_URI, &[&format!("urn:uuid:{UUID}")]);
    r.set(GROUP_PRINTER, "urf-supported", TAG_KEYWORD, &URF.split(',').collect::<Vec<_>>());
    if let Some(a) = r.attr_mut(GROUP_PRINTER, "document-format-supported")
        && !a.values.iter().any(|(_, v)| v == b"image/urf")
    {
        a.values.push((TAG_MIME, b"image/urf".to_vec()));
    }
}

async fn post_to_printer(ip: IpAddr, body: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut s = TcpStream::connect((ip, PRINTER_PORT)).await?;
    let head = format!(
        "POST /ipp/print HTTP/1.1\r\nHost: {ip}:{PRINTER_PORT}\r\nContent-Type: application/ipp\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await?;
    s.write_all(body).await?;

    // The printer ignores `Connection: close`, so stop once the response is complete.
    let mut raw = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = s.read(&mut buf).await?;
        raw.extend_from_slice(&buf[..n]);
        if let Some(body) = complete_body(&raw, n == 0)? {
            return Ok(body);
        }
        ensure!(n > 0, "printer closed the connection mid-response");
    }
}

/// The HTTP response body once fully received, `None` while more bytes are needed.
fn complete_body(mut raw: &[u8], eof: bool) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(mut split) = find(raw, b"\r\n\r\n") else { return Ok(None) };
    // The printer sends an unsolicited "100 Continue" before the real response.
    while raw.starts_with(b"HTTP/1.1 100") || raw.starts_with(b"HTTP/1.0 100") {
        raw = &raw[split + 4..];
        let Some(s) = find(raw, b"\r\n\r\n") else { return Ok(None) };
        split = s;
    }
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    ensure!(head.split(' ').nth(1) == Some("200"), "printer said: {}", head.lines().next().unwrap_or(""));
    let body = &raw[split + 4..];
    if head.contains("transfer-encoding: chunked") {
        return dechunk(body);
    }
    let length = head.lines().find_map(|l| l.strip_prefix("content-length:")?.trim().parse::<usize>().ok());
    Ok(match length {
        Some(len) if body.len() >= len => Some(body[..len].to_vec()),
        None if eof => Some(body.to_vec()),
        _ => None,
    })
}

/// Decoded chunked body, `None` if the final chunk hasn't arrived yet.
fn dechunk(mut b: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    loop {
        let Some(eol) = find(b, b"\r\n") else { return Ok(None) };
        let size_hex = std::str::from_utf8(&b[..eol])?.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)?;
        if size == 0 {
            return Ok(Some(out));
        }
        let Some(chunk) = b.get(eol + 2..eol + 2 + size) else { return Ok(None) };
        out.extend_from_slice(chunk);
        let Some(next) = b.get(eol + 4 + size..) else { return Ok(None) };
        b = next;
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------- IPP message

/// One attribute; extra values (and collection members) keep their raw tags.
struct Attr {
    name: String,
    values: Vec<(u8, Vec<u8>)>,
}

struct Group {
    tag: u8,
    attrs: Vec<Attr>,
}

/// Raw IPP message: version, operation/status and request-id kept as bytes.
struct Ipp {
    head: [u8; 8],
    groups: Vec<Group>,
}

impl Ipp {
    /// Returns the message and the document data that follows it.
    fn parse(buf: &[u8]) -> Option<(Ipp, &[u8])> {
        let head = buf.get(..8)?.try_into().ok()?;
        let mut pos = 8;
        let mut groups: Vec<Group> = Vec::new();
        loop {
            let tag = *buf.get(pos)?;
            pos += 1;
            if tag == TAG_END {
                return Some((Ipp { head, groups }, &buf[pos..]));
            }
            if tag < 0x10 {
                groups.push(Group { tag, attrs: Vec::new() });
                continue;
            }
            let (name, next) = read_field(buf, pos)?;
            let (value, next) = read_field(buf, next)?;
            pos = next;
            let attrs = &mut groups.last_mut()?.attrs;
            match attrs.last_mut() {
                Some(a) if name.is_empty() => a.values.push((tag, value.to_vec())),
                _ => attrs.push(Attr {
                    name: String::from_utf8_lossy(name).into_owned(),
                    values: vec![(tag, value.to_vec())],
                }),
            }
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.head.to_vec();
        for g in &self.groups {
            out.push(g.tag);
            for a in &g.attrs {
                for (i, (tag, value)) in a.values.iter().enumerate() {
                    let name: &[u8] = if i == 0 { a.name.as_bytes() } else { &[] };
                    out.push(*tag);
                    out.extend((name.len() as u16).to_be_bytes());
                    out.extend_from_slice(name);
                    out.extend((value.len() as u16).to_be_bytes());
                    out.extend_from_slice(value);
                }
            }
        }
        out.push(TAG_END);
        out
    }

    /// Log view of a printer response: the job outcome attributes.
    fn summary(&self) -> String {
        const KEYS: &[&str] = &["job-id", "job-state", "job-state-reasons", "status-message"];
        let mut out = String::new();
        for a in self.groups.iter().flat_map(|g| &g.attrs).filter(|a| KEYS.contains(&a.name.as_str())) {
            let vals: Vec<String> = a.values.iter().map(|(tag, v)| match (tag, v.len()) {
                (0x21 | 0x23, 4) => i32::from_be_bytes(v[..4].try_into().unwrap()).to_string(),
                (0x22, 1) => (v[0] != 0).to_string(),
                _ => String::from_utf8_lossy(v).into_owned(),
            }).collect();
            out += &format!("{}={} ", a.name, vals.join(","));
        }
        out
    }

    fn attr(&self, group: u8, name: &str) -> Option<&Attr> {
        self.groups.iter().filter(|g| g.tag == group).flat_map(|g| &g.attrs).find(|a| a.name == name)
    }

    fn attr_mut(&mut self, group: u8, name: &str) -> Option<&mut Attr> {
        self.groups.iter_mut().filter(|g| g.tag == group).flat_map(|g| &mut g.attrs).find(|a| a.name == name)
    }

    /// Replace an attribute's values, adding it to the first matching group if absent.
    fn set(&mut self, group: u8, name: &str, tag: u8, values: &[&str]) {
        let values = values.iter().map(|v| (tag, v.as_bytes().to_vec())).collect();
        if let Some(a) = self.attr_mut(group, name) {
            a.values = values;
        } else if let Some(g) = self.groups.iter_mut().find(|g| g.tag == group) {
            g.attrs.push(Attr { name: name.to_owned(), values });
        }
    }

    /// Point every ipp/ipps URI at `authority` (host:port); other schemes (http icons etc.) untouched.
    fn rewrite_uris(&mut self, authority: &str) {
        for a in self.groups.iter_mut().flat_map(|g| &mut g.attrs) {
            for (tag, v) in &mut a.values {
                let Ok(s) = std::str::from_utf8(v) else { continue };
                let Some(rest) = s.strip_prefix("ipp://").or_else(|| s.strip_prefix("ipps://")) else { continue };
                if *tag == TAG_URI {
                    let path = rest.find('/').map_or("", |i| &rest[i..]);
                    *v = format!("ipp://{authority}{path}").into_bytes();
                }
            }
        }
    }
}

fn read_field(buf: &[u8], pos: usize) -> Option<(&[u8], usize)> {
    let len = u16::from_be_bytes(buf.get(pos..pos + 2)?.try_into().ok()?) as usize;
    Some((buf.get(pos + 2..pos + 2 + len)?, pos + 2 + len))
}

// ------------------------------------------------------------- URF -> PWG

fn be32(b: &[u8], off: usize) -> anyhow::Result<u32> {
    Ok(u32::from_be_bytes(b.get(off..off + 4).ok_or_else(|| anyhow!("truncated URF"))?.try_into()?))
}

/// Convert an Apple raster (UNIRAST) stream to PWG raster.
pub fn urf_to_pwg(urf: &[u8]) -> anyhow::Result<Vec<u8>> {
    ensure!(urf.starts_with(b"UNIRAST\0"), "not an Apple raster stream");
    let pages = be32(urf, 8)?;
    let mut out = b"RaS2".to_vec();
    let mut pos = 12;
    for _ in 0..pages {
        let h = urf.get(pos..pos + 32).ok_or_else(|| anyhow!("truncated URF page header"))?;
        let (bpp, quality) = (u32::from(h[0]), h[3]);
        let (w, ht, dpi) = (be32(h, 12)?, be32(h, 16)?, be32(h, 20)?);
        out.extend_from_slice(&pwg_header(bpp, quality, w, ht, dpi, pages)?);
        pos = copy_page(urf, pos + 32, w, ht, bpp / 8, &mut out)?;
    }
    Ok(out)
}

/// Copy one page of compressed data (line repeat + per-pixel PackBits) to `out` as PWG and
/// return where it ends. URF's 0x80 "fill rest of line with white" has no PWG equivalent (the
/// printer aborts the job on it; iOS uses it, macOS doesn't), so it becomes white repeat runs.
fn copy_page(b: &[u8], mut pos: usize, w: u32, h: u32, bytes_pp: u32, out: &mut Vec<u8>) -> anyhow::Result<usize> {
    let (line, bpp) = ((w * bytes_pp) as usize, bytes_pp as usize);
    let get = |from: usize, len: usize| b.get(from..from + len).ok_or_else(|| anyhow!("truncated URF page data"));
    let mut y = 0;
    while y < h {
        let repeat = get(pos, 1)?[0];
        out.push(repeat);
        y += u32::from(repeat) + 1;
        pos += 1;
        let mut x = 0;
        while x < line {
            let c = get(pos, 1)?[0];
            pos += 1;
            if c == 128 {
                let mut left = (line - x) / bpp;
                while left > 0 {
                    let run = left.min(128);
                    out.push((run - 1) as u8);
                    out.extend(std::iter::repeat_n(0xFF, bpp)); // white in sGray and sRGB
                    left -= run;
                }
                x = line;
                continue;
            }
            let pixels = if c < 128 { usize::from(c) + 1 } else { 257 - usize::from(c) };
            let bytes = if c < 128 { bpp } else { pixels * bpp };
            out.push(c);
            out.extend_from_slice(get(pos, bytes)?);
            pos += bytes;
            x += pixels * bpp;
        }
        ensure!(x == line, "URF run crosses the end of a line");
    }
    Ok(pos)
}

/// PWG 5102.4 page header; fields as macOS rastertopwg writes them (verified byte-for-byte).
fn pwg_header(bpp: u32, quality: u8, w: u32, h: u32, dpi: u32, pages: u32) -> anyhow::Result<[u8; 1796]> {
    let (color_space, colors) = match bpp {
        8 => (18, 1),  // sGray
        24 => (19, 3), // sRGB
        _ => bail!("unsupported URF bit depth {bpp}"),
    };
    let size_mm = ((w * 254 + dpi * 5) / (dpi * 10), (h * 254 + dpi * 5) / (dpi * 10));
    let size_name = match size_mm {
        (210, 297) => "iso_a4_210x297mm",
        (216, 279) => "na_letter_8.5x11in",
        (148, 210) => "iso_a5_148x210mm",
        _ => "",
    };
    let mut hd = [0u8; 1796];
    hd[..9].copy_from_slice(b"PwgRaster");
    hd[128..138].copy_from_slice(b"Stationery"); // fixed media type; read media-col if photo paper matters
    hd[1732..1732 + size_name.len()].copy_from_slice(size_name.as_bytes());
    let mut put = |off: usize, vals: &[u32]| {
        for (i, v) in vals.iter().enumerate() {
            hd[off + 4 * i..off + 4 * i + 4].copy_from_slice(&v.to_be_bytes());
        }
    };
    put(276, &[dpi, dpi]); // HWResolution
    put(340, &[1]); // NumCopies
    put(352, &[w * 72 / dpi, h * 72 / dpi]); // PageSize in points, truncated like macOS
    put(372, &[w, h]);
    put(384, &[8, bpp, w * bpp / 8]); // BitsPerColor, BitsPerPixel, BytesPerLine
    put(400, &[color_space]);
    put(420, &[colors]);
    // TotalPageCount, Cross/FeedTransform, ImageBox, AlternatePrimary
    put(452, &[pages, 1, 1, 0, 0, w, h, 0xFF_FFFF]);
    if (3..=5).contains(&quality) {
        put(484, &[u32::from(quality)]); // PrintQuality
    }
    Ok(hd)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference pair rendered by macOS: cgpdftoraster | rastertourf / rastertopwg (gray, 2 pages).
    #[test]
    fn urf_converts_like_macos_rastertopwg() {
        let urf = include_bytes!("../test_data/gray.urf");
        let reference = include_bytes!("../test_data/gray.pwg");
        let ours = urf_to_pwg(urf).unwrap();
        assert_eq!(ours.len(), reference.len());
        // Fields macOS fills from the PPD (margins, media position/type) are allowed to differ.
        let ppd_fields = [128..192, 284..300, 324..328, 464..480];
        let mut pos = 4;
        for _ in 0..2 {
            for i in 0..1796 {
                if !ppd_fields.iter().any(|r| r.contains(&i)) {
                    assert_eq!(ours[pos + i], reference[pos + i], "header byte {i}");
                }
            }
            let end = copy_page(&ours, pos + 1796, 4960, 7015, 1, &mut Vec::new()).unwrap();
            assert_eq!(ours[pos + 1796..end], reference[pos + 1796..end]);
            pos = end;
        }
        assert_eq!(pos, ours.len());
    }

    /// iOS ends lines with 0x80 (fill with white); the printer only understands repeat runs.
    #[test]
    fn urf_fill_code_becomes_white_runs() {
        // 300-pixel gray line: one black pixel, then fill.
        let mut out = Vec::new();
        let end = copy_page(&[0x00, 0x00, 0x00, 0x80], 0, 300, 1, 1, &mut out).unwrap();
        assert_eq!(end, 4);
        // line repeat, black pixel, then 299 white pixels as 128 + 128 + 43.
        assert_eq!(out, [0x00, 0x00, 0x00, 127, 0xFF, 127, 0xFF, 42, 0xFF]);
    }

    #[test]
    fn ipp_roundtrip_and_patch() {
        let mut msg = vec![2, 0, 0, 0x0B, 0, 0, 0, 1, GROUP_PRINTER];
        for (tag, name, value) in [
            (TAG_URI, "printer-uri-supported", "ipp://192.168.1.13:631/ipp/print"),
            (TAG_URI, "", "ipps://192.168.1.13:443/ipp/print"),
            (TAG_MIME, "document-format-supported", "image/pwg-raster"),
        ] {
            msg.push(tag);
            msg.extend((name.len() as u16).to_be_bytes());
            msg.extend(name.as_bytes());
            msg.extend((value.len() as u16).to_be_bytes());
            msg.extend(value.as_bytes());
        }
        msg.push(TAG_END);
        let (mut ipp, rest) = Ipp::parse(&msg).unwrap();
        assert!(rest.is_empty());
        assert_eq!(ipp.to_bytes(), msg);

        ipp.rewrite_uris("mac.local:8470");
        advertise_airprint(&mut ipp, "mac.local:8470");
        let uris = &ipp.attr(GROUP_PRINTER, "printer-uri-supported").unwrap().values;
        assert_eq!(uris, &[(TAG_URI, b"ipp://mac.local:8470/ipp/print".to_vec())]);
        let formats = &ipp.attr(GROUP_PRINTER, "document-format-supported").unwrap().values;
        assert!(formats.iter().any(|(_, v)| v == b"image/urf"));
        assert!(ipp.attr(GROUP_PRINTER, "urf-supported").is_some());
    }
}
