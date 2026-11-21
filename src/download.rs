//! Download official Windows ISOs straight from Microsoft's servers.
//!
//! The flow: register a download session, ask for the languages of an
//! edition, ask for the download links of a language, then fetch the file.
//! It uses the `curl` program for the network, so Ignite needs no TLS stack.
//!
//! IMPORTANT: Microsoft changes this protocol from time to time. Everything
//! that does not need the network is unit-tested; the calls themselves were
//! written from the public protocol and could not be run against Microsoft's
//! servers while this was developed. Errors are reported in plain words, and
//! the page offers the normal download page as a fallback.

use crate::job::{Msg, Shared};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const ORG_ID: &str = "y6jn8c31";
const PROFILE_ID: &str = "606624d44113";
const INSTANCE_ID: &str = "560dc9f3-1aa5-4a2f-b63c-9e18f8d0e175";
const LOCALE: &str = "en-US";
const API: &str = "https://www.microsoft.com/software-download-connector/api";
const REFERER: &str = "https://www.microsoft.com/software-download/windows11";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36 Edg/130.0.0.0";

pub const DOWNLOAD_PAGE: &str = "https://www.microsoft.com/software-download/windows11";

/// (name shown, product edition ids). Windows 11 needs two ids: one for x64
/// and one for ARM64, each with its own session.
pub const VERSIONS: [(&str, &[u32]); 2] = [
    ("Windows 11", &[3813, 3816]),
    ("Windows 10", &[2618]),
];
pub const ARCHS: [&str; 3] = ["x64", "ARM64", "x86"];

#[derive(Clone, Debug, PartialEq)]
pub struct Lang {
    /// Name Microsoft uses internally ("English (United States)").
    pub name: String,
    pub display: String,
    /// (index of the session it belongs to, SKU id)
    pub skus: Vec<(usize, String)>,
}

impl std::fmt::Display for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display)
    }
}

/// Languages plus the sessions they were fetched with (a download link only
/// works with the session that listed its SKU).
#[derive(Clone, Debug, PartialEq)]
pub struct Fetched {
    pub langs: Vec<Lang>,
    pub sessions: Vec<String>,
    pub jar: PathBuf,
}

// ------------------------------------------------------------ pure helpers

pub fn tags_url(session: &str) -> String {
    format!(
        "https://vlscppe.microsoft.com/tags?org_id={}&session_id={}",
        ORG_ID, session
    )
}

pub fn mdt_url(session: &str) -> String {
    format!(
        "https://ov-df.microsoft.com/mdt.js?instanceId={}&PageId=si&session_id={}",
        INSTANCE_ID, session
    )
}

pub fn ovdf_reply_url(session: &str, w: &str, rticks: &str, now_ms: u128) -> String {
    format!(
        "https://ov-df.microsoft.com/?session_id={}&CustomerId={}&PageId=si&w={}&mdt={}&rticks={}",
        session, INSTANCE_ID, w, now_ms, rticks
    )
}

pub fn skus_url(edition_id: u32, session: &str) -> String {
    format!(
        "{}/getskuinformationbyproductedition?profile={}&productEditionId={}&SKU=undefined&friendlyFileName=undefined&Locale={}&sessionID={}",
        API, PROFILE_ID, edition_id, LOCALE, session
    )
}

pub fn links_url(sku: &str, session: &str) -> String {
    format!(
        "{}/GetProductDownloadLinksBySku?profile={}&productEditionId=undefined&SKU={}&friendlyFileName=undefined&Locale={}&sessionID={}",
        API, PROFILE_ID, sku, LOCALE, session
    )
}

/// `w` and `rticks` from the script served by ov-df.microsoft.com/mdt.js.
pub fn parse_mdt(js: &str) -> Option<(String, String)> {
    let w = {
        let i = js.find("?w=").or_else(|| js.find("&w="))? + 3;
        let v: String = js[i..]
            .chars()
            .take_while(|c| matches!(c, '0'..='9' | 'A'..='F'))
            .collect();
        (!v.is_empty()).then_some(v)?
    };
    let rticks = {
        let i = js.find("rticks=\"")? + "rticks=\"".len();
        let rest = js[i..].trim_start_matches('+');
        let v: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        (!v.is_empty()).then_some(v)?
    };
    Some((w, rticks))
}

fn first_error(v: &Value) -> Option<(i64, String)> {
    let e = v.get("Errors")?.as_array()?.first()?;
    Some((
        e.get("Type").and_then(Value::as_i64).unwrap_or(0),
        e.get("Value")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string(),
    ))
}

/// (language, localized language, sku id) triples from a SKU answer.
pub fn parse_skus(json: &str) -> Result<Vec<(String, String, String)>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("unexpected answer from Microsoft: {}", e))?;
    if let Some((_, msg)) = first_error(&v) {
        return Err(msg);
    }
    let skus = v
        .get("Skus")
        .and_then(Value::as_array)
        .ok_or("Microsoft returned no language list")?;
    let out: Vec<_> = skus
        .iter()
        .filter_map(|s| {
            let id = match s.get("Id")? {
                Value::String(t) => t.clone(),
                Value::Number(n) => n.to_string(),
                _ => return None,
            };
            let lang = s.get("Language")?.as_str()?.to_string();
            let disp = s
                .get("LocalizedLanguage")
                .and_then(Value::as_str)
                .unwrap_or(&lang)
                .to_string();
            Some((lang, disp, id))
        })
        .collect();
    if out.is_empty() {
        Err("Microsoft returned an empty language list".into())
    } else {
        Ok(out)
    }
}

/// Merge the language lists of several sessions (keeps Microsoft's order).
pub fn merge_languages(per_session: Vec<Vec<(String, String, String)>>) -> Vec<Lang> {
    let mut langs: Vec<Lang> = Vec::new();
    for (si, list) in per_session.into_iter().enumerate() {
        for (name, display, id) in list {
            match langs.iter_mut().find(|l| l.name == name) {
                Some(l) => l.skus.push((si, id)),
                None => langs.push(Lang {
                    name,
                    display,
                    skus: vec![(si, id)],
                }),
            }
        }
    }
    langs
}

pub fn arch_name(download_type: i64) -> &'static str {
    match download_type {
        0 => "x86",
        1 => "x64",
        2 => "ARM64",
        _ => "Unknown",
    }
}

/// (architecture, url) pairs from a download-links answer.
pub fn parse_links(json: &str) -> Result<Vec<(&'static str, String)>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("unexpected answer from Microsoft: {}", e))?;
    if let Some((kind, msg)) = first_error(&v) {
        return Err(if kind == 9 {
            "Microsoft refused this request (it blocks some networks and repeated requests). Try again later, or use the browser download page.".to_string()
        } else {
            msg
        });
    }
    let opts = v
        .get("ProductDownloadOptions")
        .and_then(Value::as_array)
        .ok_or("Microsoft returned no download links")?;
    let out: Vec<_> = opts
        .iter()
        .filter_map(|o| {
            Some((
                arch_name(o.get("DownloadType")?.as_i64()?),
                o.get("Uri")?.as_str()?.to_string(),
            ))
        })
        .collect();
    if out.is_empty() {
        Err("Microsoft returned no download links".into())
    } else {
        Ok(out)
    }
}

/// "https://.../Win11_25H2_English_x64.iso?t=..." -> "Win11_25H2_English_x64.iso"
pub fn file_name_from_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let name = path.rsplit('/').next()?;
    (name.to_lowercase().ends_with(".iso") && !name.contains(['/', '\\']) && name != ".iso")
        .then(|| name.to_string())
}

/// Last Content-Length of a (possibly redirected) HEAD request.
pub fn parse_content_length(headers: &str) -> Option<u64> {
    headers
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse().ok())?
        })
        .last()
}

pub fn pick_default_language(langs: &[Lang]) -> Option<usize> {
    langs
        .iter()
        .position(|l| l.name.starts_with("English"))
        .or(if langs.is_empty() { None } else { Some(0) })
}

/// Where downloads go: ~/Downloads, else the home directory, else /tmp.
pub fn download_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("HOME").map(PathBuf::from) {
        let d = h.join("Downloads");
        if d.is_dir() {
            return d;
        }
        if h.is_dir() {
            return h;
        }
    }
    PathBuf::from("/tmp")
}

// ----------------------------------------------------------------- network

pub fn curl_available() -> bool {
    Command::new("curl")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn new_uuid() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!(
                "{:08x}-0000-4000-8000-{:012x}",
                std::process::id(),
                t & 0xffff_ffff_ffff
            )
        })
}

fn http_get(url: &str, referer: Option<&str>, jar: &Path, cancel: &AtomicBool) -> Result<String, String> {
    if cancel.load(Ordering::SeqCst) {
        return Err("cancelled".into());
    }
    let mut c = Command::new("curl");
    c.args(["-sS", "--max-time", "40", "-A", USER_AGENT, "-b"])
        .arg(jar)
        .arg("-c")
        .arg(jar);
    if let Some(r) = referer {
        c.args(["-e", r]);
    }
    c.arg(url);
    let out = c
        .output()
        .map_err(|e| format!("cannot run curl (is it installed?): {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "network error: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Register a session the way Microsoft's page does (three "bot protection"
/// requests), then ask for the SKUs of one edition.
fn session_skus(
    edition: u32,
    jar: &Path,
    cancel: &AtomicBool,
) -> Result<(String, Vec<(String, String, String)>), String> {
    let sid = new_uuid();
    http_get(&tags_url(&sid), None, jar, cancel)?;
    let js = http_get(&mdt_url(&sid), None, jar, cancel)?;
    let (w, rticks) = parse_mdt(&js).ok_or("could not read Microsoft's session data (the protocol may have changed)")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    http_get(&ovdf_reply_url(&sid, &w, &rticks, now), None, jar, cancel)?;

    let mut last = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(2));
        }
        match http_get(&skus_url(edition, &sid), None, jar, cancel).and_then(|b| parse_skus(&b)) {
            Ok(v) => return Ok((sid, v)),
            Err(e) => last = e,
        }
    }
    Err(last)
}

pub fn fetch_languages(version: usize, cancel: &AtomicBool) -> Result<Fetched, String> {
    let (_, ids) = VERSIONS.get(version).ok_or("unknown Windows version")?;
    if !curl_available() {
        return Err("'curl' is not installed. Install it (sudo dnf/apt/pacman/zypper install curl).".into());
    }
    let jar = std::env::temp_dir().join(format!("ignite-dl-{}.cookies", std::process::id()));
    let mut sessions = Vec::new();
    let mut per_session = Vec::new();
    for id in ids.iter() {
        let (sid, skus) = session_skus(*id, &jar, cancel)?;
        sessions.push(sid);
        per_session.push(skus);
    }
    Ok(Fetched {
        langs: merge_languages(per_session),
        sessions,
        jar,
    })
}

/// Ask for the links of one language and return the one for `arch`.
pub fn get_link(f: &Fetched, lang: &Lang, arch: &str, cancel: &AtomicBool) -> Result<String, String> {
    let mut available: Vec<&str> = Vec::new();
    for (si, sku) in &lang.skus {
        let sid = f.sessions.get(*si).ok_or("lost the download session; fetch the languages again")?;
        let body = http_get(&links_url(sku, sid), Some(REFERER), &f.jar, cancel)?;
        for (a, url) in parse_links(&body)? {
            if a.eq_ignore_ascii_case(arch) {
                return Ok(url);
            }
            available.push(a);
        }
    }
    Err(format!(
        "Microsoft has no {} download for this language (available: {}).",
        arch,
        if available.is_empty() { "none".to_string() } else { available.join(", ") }
    ))
}

/// Download `url` into `dir`, reporting progress through `shared`.
/// Runs in its own thread; ends with `Msg::Downloaded` or `Msg::Error`.
pub fn start_download(url: String, dir: PathBuf, shared: Arc<Shared>, cancel: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let r = download(&url, &dir, &shared, &cancel);
        match r {
            Ok(p) => shared.push(Msg::Downloaded(p.to_string_lossy().into_owned())),
            Err(e) => shared.push(Msg::Error(e)),
        }
    });
}

fn download(url: &str, dir: &Path, shared: &Shared, cancel: &AtomicBool) -> Result<PathBuf, String> {
    let name = file_name_from_url(url).ok_or("Microsoft's link does not point to an ISO file")?;
    let dest = dir.join(&name);
    let part = dir.join(format!("{}.part", name));
    shared.push(Msg::Stage(format!("Downloading {}", name)));

    let total = Command::new("curl")
        .args(["-sIL", "--max-time", "40", "-A", USER_AGENT])
        .arg(url)
        .output()
        .ok()
        .and_then(|o| parse_content_length(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or(0);

    let mut child = Command::new("curl")
        .args(["-L", "--fail", "-sS", "-C", "-", "-A", USER_AGENT, "-o"])
        .arg(&part)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run curl (is it installed?): {}", e))?;
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("download cancelled (the partial file is kept and will be resumed next time)".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    break;
                }
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.read_to_string(&mut err);
                }
                return Err(format!("download failed: {}", err.trim()));
            }
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }
        if total > 0 {
            let done = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
            shared.push(Msg::Progress {
                phase: "download".into(),
                done,
                total,
            });
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::rename(&part, &dest).map_err(|e| format!("cannot save {}: {}", dest.display(), e))?;
    Ok(dest)
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_match_the_protocol() {
        let u = skus_url(3813, "S");
        assert!(u.starts_with("https://www.microsoft.com/software-download-connector/api/getskuinformationbyproductedition?profile=606624d44113&productEditionId=3813&SKU=undefined"));
        assert!(u.ends_with("&Locale=en-US&sessionID=S"));
        let u = links_url("17734", "S");
        assert!(u.contains("GetProductDownloadLinksBySku") && u.contains("&SKU=17734&") && u.contains("productEditionId=undefined"));
        assert_eq!(
            tags_url("S"),
            "https://vlscppe.microsoft.com/tags?org_id=y6jn8c31&session_id=S"
        );
        assert!(mdt_url("S").contains("instanceId=560dc9f3-1aa5-4a2f-b63c-9e18f8d0e175&PageId=si&session_id=S"));
        assert_eq!(
            ovdf_reply_url("S", "AB12", "345", 99),
            "https://ov-df.microsoft.com/?session_id=S&CustomerId=560dc9f3-1aa5-4a2f-b63c-9e18f8d0e175&PageId=si&w=AB12&mdt=99&rticks=345"
        );
    }

    #[test]
    fn mdt_parsing() {
        let js = "var x = 'https://ov-df.microsoft.com/?session_id=1&w=7F3A9C&foo=1'; var r = new Date(); rticks=\"+12345678\"; ";
        assert_eq!(parse_mdt(js), Some(("7F3A9C".into(), "12345678".into())));
        assert_eq!(parse_mdt("nothing useful"), None);
        assert_eq!(parse_mdt("?w=ABC"), None);
    }

    #[test]
    fn sku_parsing_and_merging() {
        let json = r#"{"Skus":[
            {"Id":"17734","Language":"English (United States)","LocalizedLanguage":"English (United States)"},
            {"Id":"17740","Language":"Persian","LocalizedLanguage":"فارسی"}]}"#;
        let a = parse_skus(json).unwrap();
        assert_eq!(a.len(), 2);
        let json2 = r#"{"Skus":[{"Id":18000,"Language":"English (United States)","LocalizedLanguage":"English (United States)"}]}"#;
        let b = parse_skus(json2).unwrap();
        assert_eq!(b[0].2, "18000", "numeric ids work too");
        let langs = merge_languages(vec![a, b]);
        assert_eq!(langs.len(), 2);
        assert_eq!(langs[0].skus, vec![(0, "17734".to_string()), (1, "18000".to_string())]);
        assert_eq!(langs[1].skus, vec![(0, "17740".to_string())]);
        assert_eq!(pick_default_language(&langs), Some(0));
        assert_eq!(pick_default_language(&langs[1..]), Some(0));
        assert_eq!(pick_default_language(&[]), None);
    }

    #[test]
    fn sku_errors() {
        assert!(parse_skus("not json").is_err());
        assert!(parse_skus(r#"{"Skus":[]}"#).is_err());
        assert_eq!(
            parse_skus(r#"{"Errors":[{"Type":1,"Value":"Bad request"}]}"#).unwrap_err(),
            "Bad request"
        );
    }

    #[test]
    fn link_parsing() {
        let json = r#"{"ProductDownloadOptions":[
            {"DownloadType":1,"Uri":"https://x.microsoft.com/Win11_x64.iso?t=1"},
            {"DownloadType":2,"Uri":"https://x.microsoft.com/Win11_arm64.iso?t=1"}]}"#;
        let l = parse_links(json).unwrap();
        assert_eq!(l[0].0, "x64");
        assert_eq!(l[1].0, "ARM64");
        assert!(parse_links(r#"{"ProductDownloadOptions":[]}"#).is_err());
        let e = parse_links(r#"{"Errors":[{"Type":9,"Value":"x"}]}"#).unwrap_err();
        assert!(e.contains("refused"));
        assert_eq!(parse_links(r#"{"Errors":[{"Type":2,"Value":"gone"}]}"#).unwrap_err(), "gone");
    }

    #[test]
    fn file_names_and_sizes() {
        assert_eq!(
            file_name_from_url("https://a.b/c/Win11_25H2_English_x64.iso?t=1&P1=2").as_deref(),
            Some("Win11_25H2_English_x64.iso")
        );
        assert_eq!(file_name_from_url("https://a.b/c/readme.txt"), None);
        assert_eq!(file_name_from_url("https://a.b/.iso"), None);
        let h = "HTTP/2 302\r\ncontent-length: 0\r\n\r\nHTTP/2 200\r\nContent-Length: 5000000000\r\n";
        assert_eq!(parse_content_length(h), Some(5_000_000_000));
        assert_eq!(parse_content_length("HTTP/2 200\r\n"), None);
    }

    #[test]
    fn versions_have_editions() {
        assert!(VERSIONS.iter().all(|(_, ids)| !ids.is_empty()));
        assert_eq!(arch_name(1), "x64");
        assert_eq!(arch_name(7), "Unknown");
        assert!(new_uuid().len() >= 32);
    }

    #[test]
    fn cancelled_requests_stop_before_touching_the_network() {
        let cancel = AtomicBool::new(true);
        assert_eq!(
            http_get("https://example.invalid/", None, Path::new("/tmp/x"), &cancel).unwrap_err(),
            "cancelled"
        );
    }
}
