//! Integration tests for lsb-curl against live HTTPS endpoints.
//!
//! These need network access (like the `ci/run_tests.sh` distro matrix).
//! They hit `example.com` (stable, tiny) and `httpbin.org` (echo/stream).
//! Every test is hermetic: no shared state, default options.

use lsb_curl::{Curl, CurlError, Method, RequestOptions};

fn curl() -> Curl {
    Curl::load().expect("libcurl should load")
}

#[test]
fn test_get_example_com() {
    let curl = curl();
    let resp = curl
        .request(Method::GET, "https://example.com", None, &[])
        .expect("GET example.com");
    assert_eq!(resp.status, 200);
    assert!(!resp.body.is_empty());
    assert!(!resp.headers.is_empty());
    // First line is the HTTP status line.
    assert!(
        resp.headers[0].contains("200"),
        "status line: {}",
        resp.headers[0]
    );
    let text = String::from_utf8_lossy(&resp.body);
    assert!(text.contains("Example Domain"), "unexpected body");
}

#[test]
fn test_post_json_echo() {
    let curl = curl();
    let resp = curl
        .request(
            Method::POST,
            "https://httpbin.org/post",
            Some(br#"{"hello":"lsb-curl"}"#.as_ref()),
            &[("content-type", "application/json")],
        )
        .expect("POST httpbin");
    assert_eq!(resp.status, 200);
    let text = String::from_utf8_lossy(&resp.body);
    assert!(text.contains("lsb-curl"), "echo missing payload: {text}");
}

#[test]
fn test_streaming_chunks_arrive() {
    let curl = curl();
    let mut chunks = 0usize;
    let mut bytes = 0usize;
    let (status, headers) = curl
        .request_streaming(
            Method::GET,
            "https://httpbin.org/stream/5",
            None,
            &[],
            &mut |chunk: &[u8]| {
                chunks += 1;
                bytes += chunk.len();
                Ok(true)
            },
        )
        .expect("streaming GET");
    assert_eq!(status, 200);
    assert!(chunks > 0, "no chunks delivered");
    assert!(bytes > 0, "no bytes delivered");
    assert!(!headers.is_empty());
}

#[test]
fn test_streaming_abort() {
    let curl = curl();
    let r = curl.request_streaming(
        Method::GET,
        "https://httpbin.org/stream/20",
        None,
        &[],
        &mut |_chunk: &[u8]| Ok(false),
    );
    assert!(
        matches!(r, Err(CurlError::Aborted)),
        "expected Aborted, got {r:?}"
    );
}

#[test]
fn test_streaming_callback_error_propagates() {
    let curl = curl();
    let r = curl.request_streaming(
        Method::GET,
        "https://httpbin.org/stream/5",
        None,
        &[],
        &mut |_chunk: &[u8]| {
            Err(CurlError::InvalidInput("boom".into()))
        },
    );
    assert!(
        matches!(r, Err(CurlError::InvalidInput(_))),
        "expected InvalidInput, got {r:?}"
    );
}

#[test]
fn test_redirect_followed() {
    // httpbin /redirect/1 issues a single 302; default options follow it.
    let curl = curl();
    let resp = curl
        .request(Method::GET, "https://httpbin.org/redirect/1", None, &[])
        .expect("redirect GET");
    assert_eq!(resp.status, 200);
}

#[test]
fn test_custom_options_no_redirect() {
    let curl = curl();
    let opts = RequestOptions { follow_redirects: false, ..RequestOptions::default() };
    let resp = curl
        .request_with_options(
            Method::GET,
            "https://httpbin.org/redirect/1",
            None,
            &[],
            &opts,
        )
        .expect("no-follow GET");
    assert_eq!(resp.status, 302, "expected bare 302, got {}", resp.status);
}

#[test]
fn test_bad_host_is_transport_error() {
    let curl = curl();
    let opts = RequestOptions {
        connect_timeout_ms: 5_000,
        timeout_ms: 10_000,
        ..RequestOptions::default()
    };
    let r = curl.request_with_options(
        Method::GET,
        "https://nonexistent.invalid/",
        None,
        &[],
        &opts,
    );
    // DNS failure (or a hostile captive proxy 4xx/5xx): either way it is
    // not a success and not an InvalidInput.
    match r {
        Err(CurlError::Curl { .. }) => {}
        Ok(resp) => assert!(
            resp.status >= 400,
            "expected transport error or 4xx/5xx, got {}",
            resp.status
        ),
        Err(e) => panic!("unexpected error kind: {e:?}"),
    }
}
