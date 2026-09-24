//! Standalone web tools (Feature 10): `web_search` and `web_fetch`.
//!
//! PIR already has Ollama-Cloud-backed web tools in the `ollama-cloud`
//! extension, but those require an Ollama Cloud key and only work when that
//! provider is configured. These are the *provider-independent* pair: a plain
//! HTTP client (the runtime-dlopen'd host libcurl, so no static TLS dependency)
//! plus a URL guard and an HTML→text reducer.
//!
//! # Why an allow-listed URL guard
//!
//! A fetch tool is an SSRF primitive by construction: the model chooses the URL,
//! and the process may be running with more network reach than the model should
//! have (a cloud metadata endpoint, an internal admin panel, a dev server on
//! localhost). So `web_fetch` only accepts `http`/`https`, rejects hosts that
//! resolve to loopback / private / link-local / unique-local addresses, and
//! rejects the well-known cloud metadata addresses by name. `PIR_WEB_ALLOW=`
//! can lift the restriction for a specific host when an operator really does
//! want the agent to read something on the LAN.
//!
//! # Search backend
//!
//! There is no keyless, ToS-clean, universally-available search API, so the
//! backend is pluggable: `PIR_WEB_SEARCH_URL` takes a template containing
//! `{query}` (e.g. a self-hosted SearXNG at
//! `http://searx.local/search?q={query}&format=json`). With nothing configured
//! we fall back to the public DuckDuckGo HTML endpoint, parse the result links
//! out of the markup with a small regex, and say plainly when that yields
//! nothing — a tool that silently returns zero results is worse than one that
//! explains itself.

use std::net::{IpAddr, ToSocketAddrs};
use std::sync::OnceLock;

/// Maximum bytes we will buffer from one fetch. A tool result that is 50 MB of
/// HTML helps nobody (and would blow the context window), so the body is capped
/// before any conversion happens.
const MAX_BODY: usize = 2 * 1024 * 1024;

/// Default search endpoint (the no-JS HTML page; no key required).
const DEFAULT_SEARCH_URL: &str = "https://html.duckduckgo.com/html/?q={query}";

/// A shared, lazily-loaded libcurl handle. `Curl::load()` dlopens the host's
/// libcurl, which is not free; one shared handle serves every call.
fn curl() -> Result<&'static lsb_curl::Curl, String> {
    static CURL: OnceLock<Result<lsb_curl::Curl, String>> = OnceLock::new();
    CURL.get_or_init(|| {
        lsb_curl::Curl::load().map_err(|e| {
            format!("no usable libcurl on this host ({e}) — web tools need libcurl.so.4")
        })
    })
    .as_ref()
    .map_err(|e| e.clone())
}

/// The outcome of a fetch, before any HTML reduction.
pub struct Fetched {
    pub status: u16,
    pub body: String,
    pub url: String,
    pub truncated: bool,
}

/// Fetch `url` as text, with the SSRF guard applied. `max_bytes` caps the body
/// (0 = [`MAX_BODY`]).
pub fn fetch(url: &str, max_bytes: usize) -> Result<Fetched, String> {
    guard_url(url)?;
    let curl = curl()?;
    let opts = lsb_curl::RequestOptions {
        user_agent: "pir-agent/0.1 (+terminal coding agent)".to_string(),
        timeout_ms: 30_000,
        connect_timeout_ms: 10_000,
        follow_redirects: true,
        max_redirs: 5,
        ..lsb_curl::RequestOptions::default()
    };
    let resp = curl
        .request_with_options(lsb_curl::Method::GET, url, None, &[], &opts)
        .map_err(|e| format!("fetch {url}: {e}"))?;
    let cap = if max_bytes == 0 { MAX_BODY } else { max_bytes };
    let truncated = resp.body.len() > cap;
    let body = String::from_utf8_lossy(&resp.body[..resp.body.len().min(cap)]).to_string();
    Ok(Fetched { status: resp.status, body, url: url.to_string(), truncated })
}

/// Reject URLs the agent must not be able to reach.
///
/// Rules, in order: scheme must be http/https; the host must not be a
/// well-known metadata hostname; and every address the host resolves to must be
/// public (no loopback / private / link-local / unique-local / unspecified /
/// multicast). `PIR_WEB_ALLOW=host1,host2` (also `*` for "any host") lifts the
/// address check for the named hosts, which is the escape hatch for a LAN
/// service an operator genuinely wants read.
///
/// A host we cannot resolve is *allowed*: resolution failure is a network error
/// the fetch itself will report, and refusing a name we simply couldn't resolve
/// would break the legitimate case (a DNS hiccup, or a resolver that only
/// answers at request time).
pub fn guard_url(url: &str) -> Result<(), String> {
    let lower = url.trim().to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(format!(
            "refusing to fetch '{url}': only http:// and https:// URLs are allowed"
        ));
    }
    let rest = &lower[if lower.starts_with("https://") { 8 } else { 7 }..];
    let host_port = rest.split(['/', '?', '#'].as_ref()).next().unwrap_or("");
    // Strip userinfo and the port; IPv6 literals are bracketed.
    let host_port = host_port.rsplit('@').next().unwrap_or(host_port);
    let host = if let Some(h) = host_port.strip_prefix('[') {
        h.split(']').next().unwrap_or("").to_string()
    } else {
        host_port.split(':').next().unwrap_or("").to_string()
    };
    if host.is_empty() {
        return Err(format!("refusing to fetch '{url}': no host"));
    }
    // Names that are sensitive on every cloud, checked before DNS so a hostile
    // resolver or an /etc/hosts entry can't make them look public.
    const BLOCKED_HOSTS: &[&str] = &[
        "metadata.google.internal",
        "metadata.goog",
        "instance-data",
        "169.254.169.254",
        // Localhost aliases are refused by name too: they must never be
        // reachable even if the resolver can't answer (in which case the
        // address check below would let them through).
        "localhost",
        "localhost.localdomain",
        "ip6-localhost",
        "ip6-loopback",
        "0.0.0.0",
        "[::]",
        "::",
    ];
    for b in BLOCKED_HOSTS {
        if host == *b {
            return Err(format!(
                "refusing to fetch '{url}': '{host}' is a loopback/metadata host (SSRF guard) — \
                 set PIR_WEB_ALLOW={host} to permit it deliberately"
            ));
        }
    }
    if allow_star() || allow_list().iter().any(|h| host == *h) {
        return Ok(());
    }
    // Resolve and check every address.
    let addrs = match (host.as_str(), port_of(&lower)).to_socket_addrs() {
        Ok(a) => a.collect::<Vec<_>>(),
        Err(_) => return Ok(()), // unresolvable: let the fetch report the error
    };
    for addr in addrs {
        if !is_public(addr.ip()) {
            return Err(format!(
                "refusing to fetch '{url}': '{host}' resolves to {} which is not a public \
                 address (SSRF guard). Set PIR_WEB_ALLOW={host} to permit it deliberately.",
                addr.ip()
            ));
        }
    }
    Ok(())
}

fn port_of(url_lower: &str) -> u16 {
    let rest = &url_lower[if url_lower.starts_with("https://") { 8 } else { 7 }..];
    let host_port = rest.split(['/', '?', '#'].as_ref()).next().unwrap_or("");
    let host_port = host_port.rsplit('@').next().unwrap_or(host_port);
    if let Some(rest) = host_port.rsplit_once(':')
        && let Ok(p) = rest.1.parse::<u16>()
    {
        return p;
    }
    if url_lower.starts_with("https://") { 443 } else { 80 }
}

/// Whether an address is routable on the public internet.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                // CGNAT 100.64/10 and 0.0.0.0/8 are not public either.
                || v4.octets()[0] == 0
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1])))
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // fc00::/7 unique-local
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // fe80::/10 link-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped: recheck the embedded v4.
                || v6.to_ipv4_mapped().map(|v4| !is_public(IpAddr::V4(v4))).unwrap_or(false))
        }
    }
}

fn allow_star() -> bool {
    std::env::var("PIR_WEB_ALLOW").map(|v| v.trim() == "*").unwrap_or(false)
}

fn allow_list() -> Vec<String> {
    std::env::var("PIR_WEB_ALLOW")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty() && s != "*")
                .collect()
        })
        .unwrap_or_default()
}

/// The configured search endpoint template.
pub fn search_endpoint() -> String {
    std::env::var("PIR_WEB_SEARCH_URL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && v.contains("{query}"))
        .unwrap_or_else(|| DEFAULT_SEARCH_URL.to_string())
}

/// Percent-encode a query for a URL (RFC 3986 unreserved set kept literal).
pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A single search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Run a search and return hits. Uses the JSON shape when the endpoint returns
/// JSON (SearXNG with `format=json`), else falls back to scraping anchors out of
/// the HTML (the DuckDuckGo default).
pub fn search(query: &str, max: usize) -> Result<Vec<Hit>, String> {
    if query.trim().is_empty() {
        return Err("web_search: 'query' must not be empty".into());
    }
    let endpoint = search_endpoint();
    let url = endpoint.replace("{query}", &urlencode(query));
    let fetched = fetch(&url, 0)?;
    if fetched.status != 200 {
        return Err(format!(
            "web_search: {} returned HTTP {}",
            fetched.url, fetched.status
        ));
    }
    let body = fetched.body;
    // JSON backend first (a self-hosted SearXNG is the common configured case).
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body)
        && let Some(results) = v.get("results").and_then(|r| r.as_array())
    {
        let hits: Vec<Hit> = results
            .iter()
            .take(max)
            .map(|r| Hit {
                title: r.get("title").and_then(|t| t.as_str()).unwrap_or("").trim().to_string(),
                url: r.get("url").and_then(|t| t.as_str()).unwrap_or("").trim().to_string(),
                snippet: r
                    .get("content")
                    .or_else(|| r.get("snippet"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            })
            .filter(|h| !h.url.is_empty())
            .collect();
        return Ok(hits);
    }
    Ok(scrape_hits(&body, max))
}

/// Pull result anchors out of an HTML results page. Deliberately conservative:
/// it takes `<a href="…">text</a>` where the href is an absolute http(s) URL and
/// the anchor text is non-trivial, then drops duplicates and the search
/// provider's own chrome. Not a general HTML parser — a "good enough to be
/// useful, honest when empty" reader.
pub fn scrape_hits(html: &str, max: usize) -> Vec<Hit> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // `(?is)` = case-insensitive + dot-matches-newline; the `[^>]*` guards keep
    // this from spanning tag boundaries on malformed markup.
    let re = RE.get_or_init(|| {
        regex::Regex::new(r#"(?is)<a\b[^>]*href=["']([^"']+)["'][^>]*>(.*?)</a>"#).unwrap()
    });
    let mut seen: Vec<String> = Vec::new();
    let mut hits: Vec<Hit> = Vec::new();
    for cap in re.captures_iter(html) {
        let href = decode_entities(&cap[1]).trim().to_string();
        let url = normalize_href(&href);
        let Some(url) = url else { continue };
        let title = decode_entities(&strip_tags(&cap[2])).trim().to_string();
        if title.chars().count() < 3 {
            continue;
        }
        // Provider chrome, not results. Checked against the *unwrapped* host, so
        // a search engine's own redirect wrapper (whose outer host is the
        // engine) is judged by the destination.
        let host = host_of(&url);
        if host.contains("duckduckgo.com") || host.contains("google.com") || host.contains("bing.com") {
            continue;
        }
        if seen.contains(&url) {
            continue;
        }
        seen.push(url.clone());
        hits.push(Hit { title, url, snippet: String::new() });
        if hits.len() >= max {
            break;
        }
    }
    hits
}

/// Turn a scraped `href` into an absolute http(s) URL, unwrapping the redirect
/// wrappers search engines use (`//duckduckgo.com/l/?uddg=<encoded>`).
fn normalize_href(href: &str) -> Option<String> {
    let h = href.trim();
    let absolute = if let Some(rest) = h.strip_prefix("//") {
        format!("https:{rest}")
    } else if h.starts_with("http://") || h.starts_with("https://") {
        h.to_string()
    } else {
        return None;
    };
    // Unwrap a redirect wrapper *before* the caller's chrome filter sees the
    // wrapper's own host (otherwise every real result behind it is discarded as
    // "the search engine itself").
    if let Some(idx) = absolute.find("uddg=") {
        let encoded = absolute[idx + 5..].split('&').next().unwrap_or("");
        let decoded = urldecode(encoded);
        if decoded.starts_with("http") {
            return Some(decoded);
        }
    }
    Some(absolute)
}

fn host_of(url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower);
    rest.split(['/', '?', '#'].as_ref())
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_string()
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Marker inserted between an anchor's text and its link index while the
/// document is being flattened. `\u{1}` (start-of-heading) never appears in real
/// page text, and — crucially — is not `<` or `>`, so the tag stripper leaves it
/// alone.
const LINK_SENTINEL: char = '\u{1}';

/// Reduce an HTML document to readable text: drop `<script>`/`<style>` bodies,
/// turn block-level tags into newlines, strip the rest, and decode entities.
/// The result is what a model can actually use, and it is what `web_fetch`
/// returns so the tool is useful on a normal web page (which is 95% markup).
pub fn html_to_text(html: &str) -> String {
    static DROP: OnceLock<regex::Regex> = OnceLock::new();
    static BLOCK: OnceLock<regex::Regex> = OnceLock::new();
    let drop = DROP.get_or_init(|| {
        // `(?is)` so the script body may span lines. No backreference (the
        // `regex` crate doesn't support them), so the closing tag is matched
        // per-alternative.
        regex::Regex::new(
            r"(?is)<script\b[^>]*>.*?</script\s*>|<style\b[^>]*>.*?</style\s*>|<noscript\b[^>]*>.*?</noscript\s*>|<svg\b[^>]*>.*?</svg\s*>|<head\b[^>]*>.*?</head\s*>",
        )
        .unwrap()
    });
    let block = BLOCK.get_or_init(|| {
        regex::Regex::new(
            r"(?is)</?(p|div|br|li|tr|h[1-6]|section|article|header|footer|table|ul|ol|blockquote|pre)\b[^>]*>",
        )
        .unwrap()
    });
    // Link targets must be captured before the tags are stripped (the `<a>`
    // element itself is what carries the href). We first mark each anchor's
    // href with a sentinel that no tag stripper can eat, strip everything, then
    // expand the sentinel into `text <url>`.
    static LINK: OnceLock<regex::Regex> = OnceLock::new();
    let link = LINK.get_or_init(|| {
        regex::Regex::new(r#"(?is)<a\b[^>]*href=["']([^"']+)["'][^>]*>(.*?)</a>"#).unwrap()
    });
    let mut links: Vec<String> = Vec::new();
    let mut s = drop.replace_all(html, " ").to_string();
    s = link
        .replace_all(&s, |c: &regex::Captures| {
            let text = strip_tags(&c[2]);
            let text = decode_entities(&text);
            let t = text.trim().to_string();
            if t.is_empty() {
                return String::new();
            }
            let href = decode_entities(&c[1]);
            links.push(href);
            let sentinel = LINK_SENTINEL;
            format!("{t}{sentinel}{}", links.len() - 1)
        })
        .to_string();
    s = block.replace_all(&s, "\n").to_string();
    // Strip tags AFTER marking, so nothing inside a marker is mistaken for one.
    s = strip_tags(&s);
    s = s
        .split(LINK_SENTINEL)
        .enumerate()
        .map(|(i, part)| {
            if i == 0 {
                return part.to_string();
            }
            // `part` starts with the link index in decimal.
            let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
            let rest = part[digits.len()..].to_string();
            match digits.parse::<usize>().ok().and_then(|idx| links.get(idx)) {
                Some(href) if href.starts_with("http") => format!(" <{href}>{rest}"),
                _ => rest,
            }
        })
        .collect::<String>();
    s = decode_entities(&s);
    // Collapse runs of blank space / blank lines.
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let t = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(t.trim_end());
        out.push('\n');
    }
    out.trim().to_string()
}

/// Remove every remaining tag (an incomplete pass is fine: the text is a
/// summary, not a faithful render).
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Decode the handful of HTML entities that actually matter for readability.
fn decode_entities(s: &str) -> String {
    let mut out = s.to_string();
    for (from, to) in [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        ("&nbsp;", " "),
        ("&mdash;", "—"),
        ("&ndash;", "–"),
        ("&hellip;", "…"),
        ("&rsquo;", "’"),
        ("&lsquo;", "‘"),
        ("&ldquo;", "“"),
        ("&rdquo;", "”"),
    ] {
        out = out.replace(from, to);
    }
    // Numeric entities (&#123; / &#x1F600;).
    static NUM: OnceLock<regex::Regex> = OnceLock::new();
    let re = NUM.get_or_init(|| regex::Regex::new(r"&#(x?[0-9a-fA-F]+);").unwrap());
    re.replace_all(&out, |c: &regex::Captures| {
        let raw = &c[1];
        let code = if let Some(hex) = raw.strip_prefix('x').or_else(|| raw.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            raw.parse::<u32>().ok()
        };
        match code.and_then(char::from_u32) {
            Some(ch) => ch.to_string(),
            None => c[0].to_string(),
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_rejects_non_http_schemes() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(guard_url("ftp://example.com/x").is_err());
        assert!(guard_url("file:///etc/passwd").is_err());
        assert!(guard_url("gopher://example.com").is_err());
        assert!(guard_url("javascript:alert(1)").is_err());
        assert!(guard_url("/relative/path").is_err());
    }

    #[test]
    fn guard_rejects_loopback_private_and_metadata() {
        // Hold the shared env lock: other tests set `PIR_WEB_ALLOW`, which is
        // exactly the knob that lifts these refusals.
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Literal addresses are rejected by the address check.
        for bad in [
            "http://127.0.0.1:8080/admin",
            "http://localhost/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.5/",
            "http://192.168.1.1/",
            "http://172.16.9.9/",
            "http://100.64.0.1/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fd00::1]/",
            "http://0.0.0.0/",
        ] {
            let err = guard_url(bad).unwrap_err();
            assert!(
                err.contains("refusing to fetch"),
                "{bad} must be refused, got: {err}"
            );
        }
        // Metadata hostnames are refused by name, before any DNS.
        let err = guard_url("http://metadata.google.internal/computeMetadata/v1/").unwrap_err();
        assert!(err.contains("SSRF guard"), "got: {err}");
        assert!(err.contains("PIR_WEB_ALLOW"), "must say how to opt in: {err}");
    }

    #[test]
    fn guard_allows_public_urls_and_lets_an_operator_pin_hosts() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A literal public IP resolves with no DNS at all: deterministic.
        assert!(guard_url("http://93.184.216.34/").is_ok(), "public IP must pass");
        assert!(guard_url("https://93.184.216.34/a?b=c#d").is_ok());
        // Unresolvable names pass (the fetch reports the network error).
        assert!(guard_url("https://this-host-does-not-exist.invalid/").is_ok());
    }

    #[test]
    fn allow_list_and_star_lift_the_address_check() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("PIR_WEB_ALLOW");
        // SAFETY: test-local env mutation under the shared env lock.
        unsafe { std::env::set_var("PIR_WEB_ALLOW", "127.0.0.1,dev.local"); }
        assert!(guard_url("http://127.0.0.1:3000/").is_ok(), "pinned host must pass");
        assert!(guard_url("http://10.0.0.5/").is_err(), "unpinned host still refused");
        unsafe { std::env::set_var("PIR_WEB_ALLOW", "*"); }
        assert!(guard_url("http://10.0.0.5/").is_ok());
        assert!(guard_url("ftp://x/").is_err(), "the scheme check is never lifted");
        match old {
            Some(v) => unsafe { std::env::set_var("PIR_WEB_ALLOW", v) },
            None => unsafe { std::env::remove_var("PIR_WEB_ALLOW") },
        }
    }

    #[test]
    fn urlencode_escapes_and_keeps_the_unreserved_set() {
        assert_eq!(urlencode("rust async"), "rust+async");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencode("AZaz09-_.~"), "AZaz09-_.~");
        assert_eq!(urlencode("héllo"), "h%C3%A9llo");
    }

    #[test]
    fn search_endpoint_defaults_and_validates_the_template() {
        let _env = crate::config::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("PIR_WEB_SEARCH_URL");
        assert_eq!(search_endpoint(), DEFAULT_SEARCH_URL);
        // A template without {query} is unusable, so it is ignored.
        unsafe { std::env::set_var("PIR_WEB_SEARCH_URL", "http://searx.local/search?q=nothing"); }
        assert_eq!(search_endpoint(), DEFAULT_SEARCH_URL, "invalid template ignored");
        unsafe { std::env::set_var("PIR_WEB_SEARCH_URL", "http://searx.local/search?q={query}&format=json"); }
        assert!(search_endpoint().contains("{query}"));
        match old {
            Some(v) => unsafe { std::env::set_var("PIR_WEB_SEARCH_URL", v) },
            None => unsafe { std::env::remove_var("PIR_WEB_SEARCH_URL") },
        }
    }

    #[test]
    fn html_to_text_extracts_readable_content() {
        let html = r#"<html><head><title>t</title><style>body{color:red}</style>
<script>var x = 1 < 2;</script></head>
<body><h1>Heading</h1><p>First para with <b>bold</b> text.</p>
<ul><li>one</li><li>two</li></ul>
<p>See <a href="https://example.com/doc">the docs</a> for more.</p>
</body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Heading"), "got: {text}");
        assert!(text.contains("First para with bold text."), "got: {text}");
        assert!(text.contains("one") && text.contains("two"), "got: {text}");
        assert!(text.contains("the docs <https://example.com/doc>"), "links must survive: {text}");
        // Script/style bodies are dropped entirely.
        assert!(!text.contains("color:red"), "style leaked: {text}");
        assert!(!text.contains("var x"), "script leaked: {text}");
        // No *markup* survives; the only `<...>` left is the URL marker the
        // reducer deliberately emits for links.
        assert!(!text.contains("<p>") && !text.contains("</"), "tags leaked: {text}");
        assert!(!text.contains("<b>"), "tags leaked: {text}");
    }

    #[test]
    fn entities_are_decoded_including_numeric() {
        let t = html_to_text("<p>a &amp; b &lt;tag&gt; &#169; &#x1F600; &nbsp;end</p>");
        assert!(t.contains("a & b <tag> © 😀"), "got: {t}");
    }

    #[test]
    fn scrape_hits_takes_absolute_links_and_unwraps_redirects() {
        let html = r#"
<a href="https://example.com/one">First Result</a>
<a href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Ftwo&amp;rut=abc">Second Result</a>
<a href="/relative">Relative</a>
<a href="https://duckduckgo.com/y.js?ad=1">DDG chrome</a>
<a href="https://example.com/one">First Result</a>
"#;
        let hits = scrape_hits(html, 10);
        assert_eq!(hits.len(), 2, "got: {hits:?}");
        assert_eq!(hits[0].title, "First Result");
        assert_eq!(hits[0].url, "https://example.com/one");
        assert_eq!(hits[1].url, "https://example.org/two", "redirect must be unwrapped");
        assert_eq!(hits[1].title, "Second Result");
    }

    #[test]
    fn scrape_hits_respects_max_and_tolerates_junk() {
        let html = "<a href='https://a.example/1'>One</a><a href='https://a.example/2'>Two</a>";
        assert_eq!(scrape_hits(html, 1).len(), 1);
        assert!(scrape_hits("", 5).is_empty());
        assert!(scrape_hits("<a>bare</a>", 5).is_empty());
        assert!(scrape_hits("<a href='https://x.example/'>ab</a>", 5).is_empty(), "title too short");
    }

    /// Live-network test, opt-in via `PIR_WEB_NET_TESTS=1`. Not run by default
    /// (the suite must pass offline), but this is the only thing that proves the
    /// libcurl wiring, the guard, and the HTML reducer work against a real
    /// server.
    #[test]
    fn live_fetch_and_reduce_example_com() {
        if std::env::var_os("PIR_WEB_NET_TESTS").is_none() {
            return;
        }
        let f = fetch("https://example.com/", 0).expect("fetch example.com");
        assert_eq!(f.status, 200, "got HTTP {}", f.status);
        assert!(!f.body.is_empty());
        let text = html_to_text(&f.body);
        assert!(text.contains("Example Domain"), "got: {text}");
        assert!(!text.contains("<html"), "must be reduced: {text}");
        // The guard must refuse a loopback URL through the same code path.
        assert!(fetch("http://127.0.0.1:1/", 0).is_err());
    }

    /// Live search, opt-in like the fetch test. The public HTML endpoint may
    /// rate-limit or change its markup, so this only asserts the *shape*: either
    /// hits come back well-formed, or the search reports honestly that it parsed
    /// nothing (which `do_web_search` turns into an actionable message).
    #[test]
    fn live_search_returns_hits_or_explains_itself() {
        if std::env::var_os("PIR_WEB_NET_TESTS").is_none() {
            return;
        }
        match search("rust programming language", 5) {
            Ok(hits) => {
                for h in &hits {
                    assert!(h.url.starts_with("http"), "bad url: {h:?}");
                    assert!(!h.title.is_empty(), "empty title: {h:?}");
                }
                eprintln!("live search hits: {}", hits.len());
            }
            Err(e) => {
                // A network/HTTP failure must be a legible error, never a panic.
                assert!(e.contains("web_search"), "got: {e}");
            }
        }
    }

    #[test]
    fn url_decode_handles_percent_and_plus() {
        assert_eq!(urldecode("a%20b+c"), "a b c");
        assert_eq!(urldecode("%2F%3F"), "/?");
        // A malformed escape is passed through rather than dropped.
        assert_eq!(urldecode("100%"), "100%");
        assert_eq!(urldecode("%zz"), "%zz");
    }
}


