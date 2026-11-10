// util.rs — Go-compatible primitives: path ops, formatting, time, mime, sniffing,
// base64, crc32 and JSON encoding. Zero external dependencies.

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::OnceLock;

// ─── Go filepath semantics (slash-separated strings) ─────────────────────────

/// Port of Go's path/filepath.Clean.
pub fn goclean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let rooted = path.starts_with('/');
    let n = path.len();
    let b = path.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut r = 0usize;
    let mut dotdot = 0usize;
    if rooted {
        out.push(b'/');
        r = 1;
        dotdot = 1;
    }
    while r < n {
        if b[r] == b'/' {
            r += 1;
        } else if b[r] == b'.' && (r + 1 == n || b[r + 1] == b'/') {
            r += 1;
        } else if b[r] == b'.' && b[r + 1] == b'.' && (r + 2 == n || b[r + 2] == b'/') {
            r += 2;
            if out.len() > dotdot {
                let mut w = out.len() - 1;
                while w > dotdot && out[w] != b'/' {
                    w -= 1;
                }
                out.truncate(w);
            } else if !rooted {
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.push(b'.');
                out.push(b'.');
                dotdot = out.len();
            }
        } else {
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                out.push(b'/');
            }
            while r < n && b[r] != b'/' {
                out.push(b[r]);
                r += 1;
            }
        }
    }
    if out.is_empty() {
        return ".".to_string();
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Port of Go's path/filepath.Join.
pub fn gojoin(a: &str, b: &str) -> String {
    if a.is_empty() && b.is_empty() {
        return String::new();
    }
    let joined = if a.is_empty() {
        b.to_string()
    } else if b.is_empty() {
        a.to_string()
    } else {
        format!("{a}/{b}")
    };
    goclean(&joined)
}

/// Port of Go's path/filepath.Base.
pub fn gobase(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let b = path.as_bytes();
    let mut n = b.len();
    while n > 0 && b[n - 1] == b'/' {
        n -= 1;
    }
    let p = &b[..n];
    match p.iter().rposition(|&c| c == b'/') {
        Some(i) => String::from_utf8_lossy(&p[i + 1..]).into_owned(),
        None => String::from_utf8_lossy(p).into_owned(),
    }
}

// ─── Size formatting (Go fmt %.1f) ───────────────────────────────────────────

pub fn fmt_size(n: u64) -> String {
    const GB: u64 = 1 << 30;
    const MB: u64 = 1 << 20;
    const KB: u64 = 1 << 10;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

// ─── Base64 (standard alphabet) ──────────────────────────────────────────────

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b" \r\n\t".contains(b)).collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut chunk = [0u8; 4];
    let mut n = 0usize;
    let mut pad = 0usize;
    for &c in &bytes {
        if c == b'=' {
            if n == 0 {
                return None;
            }
            pad += 1;
            chunk[n] = 0;
            n += 1;
        } else {
            if pad > 0 {
                return None;
            }
            chunk[n] = val(c)?;
            n += 1;
        }
        if n == 4 {
            out.push(chunk[0] << 2 | chunk[1] >> 4);
            if pad < 2 {
                out.push(chunk[1] << 4 | chunk[2] >> 2);
            }
            if pad < 1 {
                out.push(chunk[2] << 6 | chunk[3]);
            }
            n = 0;
        }
    }
    if n != 0 {
        return None;
    }
    Some(out)
}

// ─── CRC32 (IEEE) ────────────────────────────────────────────────────────────

pub struct Crc32 {
    v: u32,
}

impl Crc32 {
    pub fn new() -> Self {
        Crc32 { v: 0xFFFF_FFFF }
    }
    pub fn update(&mut self, data: &[u8]) {
        let mut v = self.v;
        for &b in data {
            v = crc_table()[((v ^ b as u32) & 0xFF) as usize] ^ (v >> 8);
        }
        self.v = v;
    }
    pub fn finish(&self) -> u32 {
        self.v ^ 0xFFFF_FFFF
    }
}

fn crc_table() -> &'static [u32; 256] {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    })
}

// ─── Time: Unix → civil + local zone offset via TZif ─────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct Tm {
    pub year: i64,
    pub mon: u32,  // 1-12
    pub day: u32,  // 1-31
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
    pub wday: u32, // 0=Sunday
}

/// Howard Hinnant's days-from-civil / civil-from-days.
pub fn civil_from_unix(secs: i64) -> Tm {
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let wday = (days + 4).rem_euclid(7) as u32;
    Tm {
        year,
        mon: m as u32,
        day: d as u32,
        hour: (sod / 3600) as u32,
        min: ((sod % 3600) / 60) as u32,
        sec: (sod % 60) as u32,
        wday,
    }
}

pub fn civil_to_unix(year: i64, mon: u32, day: u32, hh: u32, mm: u32, ss: u32) -> i64 {
    let y = if mon <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if mon > 2 { mon - 3 } else { mon + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    days * 86400 + hh as i64 * 3600 + mm as i64 * 60 + ss as i64
}

/// Local UTC offset (seconds east) for the given epoch seconds.
pub fn local_offset(utc_secs: i64) -> i32 {
    tzinfo().offset_for(utc_secs)
}

/// Convert epoch UTC seconds to broken-down local time.
pub fn local_tm(utc_secs: i64) -> Tm {
    civil_from_unix(utc_secs + local_offset(utc_secs) as i64)
}

pub fn parse_http_date(s: &str) -> Option<i64> {
    let s = s.trim();
    let months = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    // RFC1123: "Mon, 02 Jan 2006 15:04:05 GMT"
    let rest = s.split_once(", ").map(|x| x.1).unwrap_or(s);
    let tok: Vec<&str> = rest.split_whitespace().collect();
    if tok.len() >= 4 {
        if let (Ok(d), Some(mon), Ok(y)) = (tok[0].parse::<u32>(), months.iter().position(|m| tok[1].to_ascii_lowercase().starts_with(m)), tok[2].parse::<i64>()) {
            let hms: Vec<&str> = tok[3].split(':').collect();
            if hms.len() == 3 {
                if let (Ok(h), Ok(mi), Ok(se)) = (
                    hms[0].parse::<u32>(),
                    hms[1].parse::<u32>(),
                    hms[2].parse::<u32>(),
                ) {
                    return Some(civil_to_unix(y, mon as u32 + 1, d, h, mi, se));
                }
            }
        }
    }
    // asctime: "Sun Nov  6 08:49:37 1994"
    if tok.len() >= 5 {
        let mon = months.iter().position(|m| tok[1].to_ascii_lowercase().starts_with(m))? as u32 + 1;
        let d: u32 = tok[2].parse().ok()?;
        let hms: Vec<&str> = tok[3].split(':').collect();
        if hms.len() != 3 {
            return None;
        }
        let y: i64 = tok[4].parse().ok()?;
        return Some(civil_to_unix(
            y,
            mon,
            d,
            hms[0].parse().ok()?,
            hms[1].parse().ok()?,
            hms[2].parse().ok()?,
        ));
    }
    None
}

const WDAY: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MON: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// RFC1123 GMT (HTTP Last-Modified / Date).
pub fn http_date(utc_secs: i64) -> String {
    let t = civil_from_unix(utc_secs);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        WDAY[t.wday as usize],
        t.day,
        MON[(t.mon - 1) as usize],
        t.year,
        t.hour,
        t.min,
        t.sec
    )
}

/// Go "2006-01-02 15:04" in local time.
pub fn fmt_modtime(utc_secs: i64) -> String {
    let t = local_tm(utc_secs);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        t.year, t.mon, t.day, t.hour, t.min
    )
}

/// Go time.RFC3339 in local time ("2006-01-02T15:04:05+08:00").
pub fn fmt_rfc3339(utc_secs: i64) -> String {
    let off = local_offset(utc_secs);
    let t = civil_from_unix(utc_secs + off as i64);
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.abs();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{sign}{:02}:{:02}",
        t.year,
        t.mon,
        t.day,
        t.hour,
        t.min,
        t.sec,
        a / 3600,
        (a % 3600) / 60
    )
}

// ─── TZif parsing for local timezone ─────────────────────────────────────────

struct TzInfo {
    /// transition times (i64) and the type index active from each transition
    transitions: Vec<(i64, usize)>,
    /// per-type UTC offsets
    offsets: Vec<i32>,
    /// offset used before the first transition
    first_offset: i32,
    /// footer TZ std offset (used beyond the last transition)
    footer_offset: Option<i32>,
}

impl TzInfo {
    fn offset_for(&self, t: i64) -> i32 {
        match self.transitions.binary_search_by(|(tt, _)| tt.cmp(&t)) {
            Ok(i) => self.offsets[self.transitions[i].1],
            Err(0) => self.first_offset,
            Err(i) => {
                if i > self.transitions.len() {
                    self.footer_offset.unwrap_or(self.first_offset)
                } else {
                    self.offsets[self.transitions[i - 1].1]
                }
            }
        }
    }
}

static TZINFO: OnceLock<TzInfo> = OnceLock::new();

fn tzinfo() -> &'static TzInfo {
    TZINFO.get_or_init(load_tzinfo)
}

fn utc_tzinfo() -> TzInfo {
    TzInfo {
        transitions: vec![],
        offsets: vec![0],
        first_offset: 0,
        footer_offset: Some(0),
    }
}

fn load_tzinfo() -> TzInfo {
    let path: PathBuf = match std::env::var("TZ") {
        Ok(tz) if !tz.is_empty() && tz.trim() != "UTC" => {
            let tz = tz.trim().trim_start_matches(':');
            let mut p = PathBuf::from("/usr/share/zoneinfo");
            p.push(tz);
            if !p.is_file() {
                return utc_tzinfo();
            }
            p
        }
        _ => PathBuf::from("/etc/localtime"),
    };
    let mut buf = Vec::new();
    match File::open(&path).and_then(|mut f| f.read_to_end(&mut buf)) {
        Ok(_) => parse_tzif(&buf).unwrap_or_else(utc_tzinfo),
        Err(_) => utc_tzinfo(),
    }
}

struct Counts {
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

fn read_counts(b: &[u8], hdr: usize) -> Option<Counts> {
    if hdr + 24 > b.len() {
        return None;
    }
    let g = |i: usize| -> usize {
        // caller checked hdr+24 <= b.len(), so these 4-byte slices are in bounds
        let s = &b[hdr + i * 4..hdr + 4 + i * 4];
        u32::from_be_bytes(s.try_into().unwrap_or([0; 4])) as usize
    };
    Some(Counts {
        isutcnt: g(0),
        isstdcnt: g(1),
        leapcnt: g(2),
        timecnt: g(3),
        typecnt: g(4),
        charcnt: g(5),
    })
}

fn parse_block(b: &[u8], base: usize, tsize: usize) -> Option<(TzInfo, usize)> {
    // base points just past the 44-byte header (magic+version+padding+counts)
    let c = read_counts(b, base - 24)?;
    let mut p = base;
    let mut transitions = Vec::with_capacity(c.timecnt);
    for _ in 0..c.timecnt {
        if p + tsize > b.len() {
            return None;
        }
        let tt = if tsize == 8 {
            i64::from_be_bytes(b[p..p + 8].try_into().ok()?)
        } else {
            i32::from_be_bytes(b[p..p + 4].try_into().ok()?) as i64
        };
        transitions.push((tt, 0usize));
        p += tsize;
    }
    for t in transitions.iter_mut() {
        if p >= b.len() {
            return None;
        }
        t.1 = b[p] as usize;
        p += 1;
    }
    let mut offsets = Vec::with_capacity(c.typecnt);
    for _ in 0..c.typecnt {
        if p + 6 > b.len() {
            return None;
        }
        offsets.push(i32::from_be_bytes(b[p..p + 4].try_into().ok()?));
        p += 6;
    }
    p += c.leapcnt * (tsize + 4) + c.isstdcnt + c.isutcnt + c.charcnt;
    if p > b.len() {
        return None;
    }
    let first_offset = offsets.first().copied().unwrap_or(0);
    Some((
        TzInfo {
            transitions,
            offsets,
            first_offset,
            footer_offset: None,
        },
        p,
    ))
}

fn parse_tzif(b: &[u8]) -> Option<TzInfo> {
    if b.len() < 44 || &b[0..4] != b"TZif" {
        return None;
    }
    let version = b[4];
    let (mut info, pos) = parse_block(b, 44, 4)?;
    if version >= b'2' {
        if pos + 44 > b.len() || &b[pos..pos + 4] != b"TZif" {
            return None;
        }
        let (info2, end) = parse_block(b, pos + 44, 8)?;
        info = info2;
        // footer TZ string
        let footer = std::str::from_utf8(&b[end..]).ok();
        info.footer_offset = footer.and_then(parse_tz_footer_offset);
    }
    Some(info)
}

/// Minimal TZ footer parse: `[STD]offset` e.g. "CST-8", "UTC0", "<+08>-8".
fn parse_tz_footer_offset(s: &str) -> Option<i32> {
    let s = s.trim_end_matches('\n').trim_end_matches('\0');
    if s.is_empty() {
        return None;
    }
    let std_part = s.split(',').next()?;
    let rest = if let Some(r) = std_part.strip_prefix('<') {
        r.split_once('>')?.1
    } else {
        match std_part.find(|c: char| !c.is_ascii_alphabetic()) {
            Some(i) => &std_part[i..],
            None => return None,
        }
    };
    if rest.is_empty() {
        return None;
    }
    let (sign, digits) = match rest.as_bytes()[0] {
        b'-' => (-1i32, &rest[1..]),
        b'+' => (1, &rest[1..]),
        _ => (1, rest),
    };
    let nums: Vec<&str> = digits.split(':').collect();
    let h: i32 = nums.first()?.parse().ok()?;
    let m: i32 = nums.get(1).and_then(|x| x.parse().ok()).unwrap_or(0);
    // POSIX TZ: offset is time added to local time to get UTC → invert sign
    Some(-sign * (h * 3600 + m * 60))
}

// ─── MIME table (Go builtins + system mime.types) ────────────────────────────

pub fn mime_by_extension(ext: &str) -> String {
    static TABLE: OnceLock<Vec<(String, String)>> = OnceLock::new();
    let table = TABLE.get_or_init(load_mime);
    let el = ext.to_ascii_lowercase();
    table
        .iter()
        .find(|(k, _)| *k == el)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn builtin_mime() -> Vec<(&'static str, &'static str)> {
    vec![
        (".avif", "image/avif"),
        (".css", "text/css; charset=utf-8"),
        (".gif", "image/gif"),
        (".htm", "text/html; charset=utf-8"),
        (".html", "text/html; charset=utf-8"),
        (".jpeg", "image/jpeg"),
        (".jpg", "image/jpeg"),
        (".js", "text/javascript; charset=utf-8"),
        (".json", "application/json"),
        (".mjs", "text/javascript; charset=utf-8"),
        (".pdf", "application/pdf"),
        (".png", "image/png"),
        (".svg", "image/svg+xml"),
        (".wasm", "application/wasm"),
        (".webp", "image/webp"),
        (".xml", "text/xml; charset=utf-8"),
    ]
}

fn set_extension_type(v: &mut Vec<(String, String)>, ext: &str, mime: &str) {
    // Go setExtensionType: text/* types without a charset param get
    // "; charset=utf-8" appended.
    let mut ctype = mime.to_string();
    if ctype.starts_with("text/") {
        let has_charset = ctype
            .split(';')
            .skip(1)
            .any(|p| p.trim().to_ascii_lowercase().starts_with("charset="));
        if !has_charset {
            ctype.push_str("; charset=utf-8");
        }
    }
    if let Some(slot) = v.iter_mut().find(|(k, _)| *k == ext) {
        slot.1 = ctype;
    } else {
        v.push((ext.to_string(), ctype));
    }
}

fn load_mime() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = builtin_mime()
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    // Mirror Go initMimeUnix: the FreeDesktop globs2 database takes priority;
    // mime.types files are only a fallback when no globs2 file exists.
    for path in [
        "/usr/local/share/mime/globs2",
        "/usr/share/mime/globs2",
    ] {
        if let Ok(content) = std::fs::read_to_string(path) {
            // lines of format: weight:mimetype:glob[:morefields...]
            for line in content.lines() {
                let fields: Vec<&str> = line.split(':').collect();
                if fields.len() < 3
                    || fields[0].is_empty()
                    || fields[2].len() < 3
                    || fields[0].starts_with('#')
                    || !fields[2].starts_with("*.")
                {
                    continue;
                }
                let ext = &fields[2][1..]; // "*.txt" → ".txt"
                if ext.contains('?') || ext.contains('*') || ext.contains('[') {
                    continue; // real glob, not a bare extension
                }
                if v.iter().any(|(k, _)| *k == ext) {
                    continue; // weight order: first entry wins
                }
                set_extension_type(&mut v, ext, fields[1]);
            }
            return v; // stop checking more files once a database is found
        }
    }
    // Fallback when no system-generated mimetype database exists.
    for path in [
        "/etc/mime.types",
        "/etc/apache2/mime.types",
        "/etc/apache/mime.types",
        "/etc/httpd/conf/mime.types",
    ] {
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                let mut it = line.split_whitespace();
                let Some(mimetype) = it.next() else { continue };
                if mimetype.starts_with('#') {
                    continue;
                }
                for ext in it {
                    if ext.starts_with('#') {
                        break;
                    }
                    set_extension_type(&mut v, &format!(".{}", ext.to_ascii_lowercase()), mimetype);
                }
            }
        }
    }
    v
}

// ─── Content sniffing (port of net/http.DetectContentType) ───────────────────

const SNIFF_LEN: usize = 512;

fn is_ws(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0C | b'\r' | b' ')
}

fn is_tt(b: u8) -> bool {
    b == b' ' || b == b'>'
}

pub fn detect_content_type(data: &[u8]) -> String {
    let data = if data.len() > SNIFF_LEN { &data[..SNIFF_LEN] } else { data };
    let mut first_non_ws = 0;
    while first_non_ws < data.len() && is_ws(data[first_non_ws]) {
        first_non_ws += 1;
    }
    let d = &data[first_non_ws..];

    // htmlSig entries (case-insensitive "<TAG" + tag-terminating byte)
    for sig in [
        "<!DOCTYPE HTML", "<HTML", "<HEAD", "<SCRIPT", "<IFRAME", "<H1", "<DIV", "<FONT",
        "<TABLE", "<A", "<STYLE", "<TITLE", "<B", "<BODY", "<BR", "<P", "<!--",
    ] {
        if d.len() >= sig.len() + 1 {
            let sb = sig.as_bytes();
            let matched = sb
                .iter()
                .zip(d.iter())
                .all(|(&b, &db)| b == (if b.is_ascii_uppercase() { db & 0xDF } else { db }));
            if matched && is_tt(d[sb.len()]) {
                return "text/html; charset=utf-8".into();
            }
        }
    }

    if mask_match(d, b"\xFF\xFF\xFF\xFF\xFF", b"<?xml", true) {
        return "text/xml; charset=utf-8".into();
    }
    if exact(d, b"%PDF-") {
        return "application/pdf".into();
    }
    if exact(d, b"%!PS-Adobe-") {
        return "application/postscript".into();
    }
    if mask_match(d, b"\xFF\xFF\x00\x00", b"\xFE\xFF\x00\x00", false) {
        return "text/plain; charset=utf-16be".into();
    }
    if mask_match(d, b"\xFF\xFF\x00\x00", b"\xFF\xFE\x00\x00", false) {
        return "text/plain; charset=utf-16le".into();
    }
    if mask_match(d, b"\xFF\xFF\xFF\x00", b"\xEF\xBB\xBF\x00", false) {
        return "text/plain; charset=utf-8".into();
    }
    if exact(d, b"\x00\x00\x01\x00") || exact(d, b"\x00\x00\x02\x00") {
        return "image/x-icon".into();
    }
    if exact(d, b"BM") {
        return "image/bmp".into();
    }
    if exact(d, b"GIF87a") || exact(d, b"GIF89a") {
        return "image/gif".into();
    }
    if mask_match(
        d,
        b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF\xFF\xFF",
        b"RIFF\x00\x00\x00\x00WEBPVP",
        false,
    ) {
        return "image/webp".into();
    }
    if exact(d, b"\x89PNG\x0D\x0A\x1A\x0A") {
        return "image/png".into();
    }
    if exact(d, b"\xFF\xD8\xFF") {
        return "image/jpeg".into();
    }
    if mask_match(
        d,
        b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
        b"FORM\x00\x00\x00\x00AIFF",
        false,
    ) {
        return "audio/aiff".into();
    }
    if mask_match(d, b"\xFF\xFF\xFF", b"ID3", false) {
        return "audio/mpeg".into();
    }
    if mask_match(d, b"\xFF\xFF\xFF\xFF\xFF", b"OggS\x00", false) {
        return "application/ogg".into();
    }
    if mask_match(d, b"\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF", b"MThd\x00\x00\x00\x06", false) {
        return "audio/midi".into();
    }
    if mask_match(
        d,
        b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
        b"RIFF\x00\x00\x00\x00AVI ",
        false,
    ) {
        return "video/avi".into();
    }
    if mask_match(
        d,
        b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
        b"RIFF\x00\x00\x00\x00WAVE",
        false,
    ) {
        return "audio/wave".into();
    }
    if mp4_sig(data) {
        return "video/mp4".into();
    }
    if exact(d, b"\x1A\x45\xDF\xA3") {
        return "video/webm".into();
    }
    if d.len() >= 36 && d[..34].iter().all(|&b| b == 0) && d[34] == b'L' && d[35] == b'P' {
        return "application/vnd.ms-fontobject".into();
    }
    if exact(d, b"\x00\x01\x00\x00") {
        return "font/ttf".into();
    }
    if exact(d, b"OTTO") {
        return "font/otf".into();
    }
    if exact(d, b"ttcf") {
        return "font/collection".into();
    }
    if exact(d, b"wOFF") {
        return "font/woff".into();
    }
    if exact(d, b"wOF2") {
        return "font/woff2".into();
    }
    if exact(d, b"\x1F\x8B\x08") {
        return "application/x-gzip".into();
    }
    if exact(d, b"PK\x03\x04") {
        return "application/zip".into();
    }
    if exact(d, b"Rar!\x1A\x07\x00") || exact(d, b"Rar!\x1A\x07\x01\x00") {
        return "application/x-rar-compressed".into();
    }
    // textSig (last)
    for &b in d {
        if b <= 0x08 || b == 0x0B || (0x0E..=0x1A).contains(&b) || (0x1C..=0x1F).contains(&b) {
            return "application/octet-stream".into();
        }
    }
    "text/plain; charset=utf-8".into()
}

fn exact(d: &[u8], pat: &[u8]) -> bool {
    d.starts_with(pat)
}

/// Go maskedSig: data[i] & mask[i] == pat[i] for all i.
fn mask_match(d: &[u8], mask: &[u8], pat: &[u8], skip_ws: bool) -> bool {
    let mut dd = d;
    if skip_ws {
        let mut i = 0;
        while i < dd.len() && is_ws(dd[i]) {
            i += 1;
        }
        dd = &dd[i..];
    }
    dd.len() >= mask.len()
        && mask
            .iter()
            .zip(dd.iter())
            .zip(pat.iter())
            .all(|((&m, &b), &p)| b & m == p)
}

/// mp4Sig ignores leading whitespace (matches Go, which receives raw data).
fn mp4_sig(data: &[u8]) -> bool {
    let d = if data.len() > SNIFF_LEN { &data[..SNIFF_LEN] } else { data };
    if d.len() < 12 {
        return false;
    }
    let box_size = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) as usize;
    if d.len() < box_size || box_size % 4 != 0 {
        return false;
    }
    if &d[4..8] != b"ftyp" {
        return false;
    }
    let mut st = 8;
    while st < box_size {
        if st != 12 && st + 3 <= d.len() && &d[st..st + 3] == b"mp4" {
            return true;
        }
        st += 4;
    }
    false
}

// ─── Go-style OS error strings ───────────────────────────────────────────────

pub fn go_errno(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "no such file or directory".into(),
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        std::io::ErrorKind::AlreadyExists => "file exists".into(),
        std::io::ErrorKind::InvalidInput => "invalid argument".into(),
        std::io::ErrorKind::DirectoryNotEmpty => "directory not empty".into(),
        _ => {
            let s = e.to_string();
            match s.find(" (os error") {
                Some(i) => s[..i].to_ascii_lowercase(),
                None => s,
            }
        }
    }
}

/// Format like Go: "open /path: no such file or directory"
pub fn goerr(op: &str, path: &str, e: &std::io::Error) -> String {
    format!("{op} {path}: {}", go_errno(e))
}

// ─── Go-compatible JSON string encoding (HTML-escaped, like json.Marshal) ────

// Mirrors encoding/json's encodeState.string (escapeHTML=true), operating on
// raw bytes: invalid UTF-8 emits the six-character escape text `\ufffd` per
// bad byte (not the literal replacement character), and every other invalid
// byte is handled independently — exactly like utf8.DecodeRuneInString with
// size==1.
pub fn gojson_string(s: &[u8]) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut i = 0usize;
    while i < s.len() {
        let b = s[i];
        if b < 0x80 {
            match b {
                b'\\' => out.push_str("\\\\"),
                b'"' => out.push_str("\\\""),
                0x08 => out.push_str("\\b"),
                0x0C => out.push_str("\\f"),
                b'\n' => out.push_str("\\n"),
                b'\r' => out.push_str("\\r"),
                b'\t' => out.push_str("\\t"),
                b'<' => out.push_str("\\u003c"),
                b'>' => out.push_str("\\u003e"),
                b'&' => out.push_str("\\u0026"),
                0x00..=0x1f => out.push_str(&format!("\\u{:04x}", b)),
                _ => out.push(b as char),
            }
            i += 1;
            continue;
        }
        let (cp, size) = decode_rune(&s[i..]);
        if cp == 0xFFFD && size == 1 {
            out.push_str("\\ufffd");
            i += 1;
            continue;
        }
        if cp == 0x2028 {
            out.push_str("\\u2028");
        } else if cp == 0x2029 {
            out.push_str("\\u2029");
        } else {
            // valid sequence: emit bytes verbatim
            out.push_str(std::str::from_utf8(&s[i..i + size]).unwrap());
        }
        i += size;
    }
    out.push('"');
    out
}

// Mirrors Go utf8.DecodeRuneInString: (codepoint, size); (0xFFFD, 1) on error.
fn decode_rune(s: &[u8]) -> (u32, usize) {
    let b0 = s[0];
    let (cp_min, need, lo1, hi1) = match b0 {
        0xC2..=0xDF => (0x80u32, 1usize, 0x80u8, 0xBFu8),
        0xE0 => (0x800, 2, 0xA0, 0xBF),
        0xE1..=0xEC => (0x800, 2, 0x80, 0xBF),
        0xED => (0x800, 2, 0x80, 0x9F),
        0xEE..=0xEF => (0x800, 2, 0x80, 0xBF),
        0xF0 => (0x10000, 3, 0x90, 0xBF),
        0xF1..=0xF3 => (0x10000, 3, 0x80, 0xBF),
        0xF4 => (0x10000, 3, 0x80, 0x8F),
        _ => return (0xFFFD, 1),
    };
    if s.len() < need + 1 {
        return (0xFFFD, 1);
    }
    if s[1] < lo1 || s[1] > hi1 {
        return (0xFFFD, 1);
    }
    for k in 2..=need {
        if s[k] < 0x80 || s[k] > 0xBF {
            return (0xFFFD, 1);
        }
    }
    let cp = match need {
        1 => ((b0 & 0x1F) as u32) << 6 | (s[1] & 0x3F) as u32,
        2 => ((b0 & 0x0F) as u32) << 12 | ((s[1] & 0x3F) as u32) << 6 | (s[2] & 0x3F) as u32,
        _ => {
            ((b0 & 0x07) as u32) << 18
                | ((s[1] & 0x3F) as u32) << 12
                | ((s[2] & 0x3F) as u32) << 6
                | (s[3] & 0x3F) as u32
        }
    };
    if cp < cp_min {
        return (0xFFFD, 1);
    }
    (cp, need + 1)
}

// ─── Misc helpers ────────────────────────────────────────────────────────────


pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn now_unix_nanos() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}
