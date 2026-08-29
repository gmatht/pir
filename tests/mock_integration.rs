use crate::config::ApiKind;
use crate::provider::Client;
use crate::types::{Block, Message};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct RecordedReq {
    path: String,
    auth: String,
    body: String,
}

#[derive(Clone, Debug)]
enum MockResponse {
    Succeed {
        chunks: Vec<String>,
        prompt_tokens: u64,
        completion_tokens: u64,
    },
    Error { status: u16, body: String },
    HtmlError { status: u16 },
}

struct MockServer {
    addr: String,
    requests: Arc<Mutex<Vec<RecordedReq>>>,
    done: Arc<AtomicBool>,
    response: Arc<Mutex<MockResponse>>,
}

impl MockServer {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        let requests: Arc<Mutex<Vec<RecordedReq>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let response = Arc::new(Mutex::new(MockResponse::Succeed {
            chunks: vec!["done".into()],
            prompt_tokens: 5,
            completion_tokens: 1,
        }));
        let reqs = requests.clone();
        let resp = response.clone();
        let done_flag = done.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                if done_flag.load(Ordering::SeqCst) {
                    break;
                }
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let mut auth = String::new();
                let mut content_length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line == "\r\n" {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(": ") {
                        let v = v.trim();
                        if k.eq_ignore_ascii_case("authorization") {
                            auth = v.to_string();
                        } else if k.eq_ignore_ascii_case("content-length") {
                            content_length = v.parse().unwrap_or(0);
                        }
                    }
                }
                let mut body = vec![0u8; content_length];
                let _ = reader.read_exact(&mut body);
                let body_str = String::from_utf8_lossy(&body).to_string();
                reqs.lock().unwrap().push(RecordedReq {
                    path,
                    auth,
                    body: body_str,
                });

                let reply = resp.lock().unwrap().clone();
                match reply {
                    MockResponse::Succeed {
                        chunks,
                        prompt_tokens,
                        completion_tokens,
                    } => {
                        let mut out = String::new();
                        for c in &chunks {
                            let chunk = serde_json::json!({
                                "choices": [{"delta": {"content": c}}]
                            });
                            out.push_str(&format!("data: {}\n\n", chunk));
                        }
                        let usage = serde_json::json!({
                            "choices": [{"delta": {}}],
                            "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens}
                        });
                        out.push_str(&format!("data: {}\n\n", usage));
                        out.push_str("data: [DONE]\n\n");
                        let http = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                            out.len(),
                            out
                        );
                        let _ = stream.write_all(http.as_bytes());
                    }
                    MockResponse::Error { status, body } => {
                        let reason = match status {
                            404 => "Not Found",
                            500 => "Internal Server Error",
                            _ => "Error",
                        };
                        let http = format!(
                            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            status,
                            reason,
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(http.as_bytes());
                    }
                    MockResponse::HtmlError { status } => {
                        let body = "<!DOCTYPE html><html><body>Not Found</body></html>";
                        let reason = match status {
                            404 => "Not Found",
                            _ => "Error",
                        };
                        let http = format!(
                            "HTTP/1.1 {} {}\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                            status,
                            reason,
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(http.as_bytes());
                    }
                }
                let _ = stream.flush();
            }
        });
        MockServer {
            addr,
            requests,
            done,
            response,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn set_response(&self, r: MockResponse) {
        *self.response.lock().unwrap() = r;
    }

    fn requests(&self) -> Vec<RecordedReq> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(&self.addr);
    }
}

#[test]
fn mock_server_full_request_response_cycle() {
    // The headline test: start a mock HTTP server, point a real `Client` at
    // it, drive a full turn,
    // into text + usage. This is the end-to-end path the real providers
    // exercise (minus TLS).
    let mock = MockServer::new();
    mock.set_response(MockResponse::Succeed {
        chunks: vec!["Hello".into(), " world!".into()],
        prompt_tokens: 10,
        completion_tokens: 2,
    });
    let mut client = Client::new(ApiKind::OpenAi, &mock.url(), "test-key".to_string());
    let cancel = Arc::new(AtomicBool::new(false));
    client.set_cancel(cancel);

    let mut text = String::new();
    let res = client.chat(
        "mock-gpt",
        128,
        "sys",
        &[Message::user("hi")],
        &[],
        &mut |t| text.push_str(t),
        crate::config::ThinkingLevel::Off,
        0,
        &mut |_s| {},
        None,
        None,
        true,
    );
    let (msg, usage) = res.expect("chat should succeed against mock");
    assert_eq!(text, "Hello world!", "streamed chunks should concatenate");
    let msg_text = msg.text();
    assert!(
        msg_text.contains("Hello world!"),
        "final message should carry text: {msg_text}"
    );
    assert!(
        usage.input >= 10,
        "usage.input should reflect prompt_tokens: {usage:?}"
    );
    assert!(
        usage.output >= 1,
        "usage.output should reflect completion_tokens: {usage:?}"
    );

    // The request should carry the Bearer <REDACTED> we configured + a JSON body.
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1, "exactly one request");
    assert!(
        reqs[0].auth.contains("Bearer"),
        "auth header must be Bearer: {}",
        reqs[0].auth
    );
    assert!(
        reqs[0].body.contains("mock-gpt"),
        "body should name the model: {}",
        reqs[0].body
    );
}

#[test]
fn mock_server_streams_tokens_in_order() {
    // Verify streaming chunks arrive in order and aren't reordered/dropped.
    let mock = MockServer::new();
    mock.set_response(MockResponse::Succeed {
        chunks: vec!["one".into(), "two".into(), "three".into()],
        prompt_tokens: 3,
        completion_tokens: 3,
    });
    let mut client = Client::new(ApiKind::OpenAi, &mock.url(), "k".to_string());
    client.setnew(AtomicBool::new(false)));

    let mut text = String::new();
    let res = client.chat(
        "m",
        64,
        "s",
        &[Message::user("q")],
        &[],
        &mut |t| text.push_str(t),
        crate::config::ThinkingLevel::Off,
        0,
        &mut |_| {},
        None,
        None,
        true,
    );
    assert!(res.is_ok(), "streaming should succeed: {:?}", res.err());
    assert_eq!(text, "onetwothree", "chunks must concatenate in order");
}

#[test]
fn mock_server_404_is_fatal_not_retried() {
    // A genuine JSON 404 (model doesn't exist) must end the turn, not be
    // retried forever. Previously this would spin through MAX_RETRIES.
    let mock = MockServer::new();
    mock.set_response(MockResponse::Error {
        status: 404,
        body: r#"{"error":{"message":"model does not exist"}}"#.into(),
    });
    let mut client = Client::new(ApiKind::OpenAi, &mock.url(), "k".to_string());
    client.set_cancel(Arc::new(AtomicBool::new(false)));

    let started = Instant::now();
    let res = client.chat(
        "missing-model",
        64,
        "s",
        &[Message::user("q")],
        &[],
        &mut |_| {},
        crate::config::ThinkingLevel::Off,
        0,
        &mut |_| {},
        None,
        None,
        true,
    );
    let elapsed = started.elapsed();
    assert!(res.is_err(), "404 must error");
    assert!(
        res.unwrap_err().contains("model does not exist"),
        "should surface API message"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "404 must not retry: took {elapsed:?}"
    );
}

#[test]
fn mock_server_500_retries_then_fails() {
    // A transient 500 should be retried (bounded) and eventually fail with
    // the server's message — proving the retry loop + error surfacing.
    let mock = MockServer::new();
    mock.set_response(MockResponse::Error {
        status: 500,
        body: r#"{"error":{"message":"upstream timeout"}}"#.into(),
    });
    let mut client = Client::new(ApiKind::OpenAi, &mock.url(), "k".to_string());
    client.set_cancel(Arc::new(AtomicBool::new(false)));

    let started = Instant::now();
    let res = client.chat(
        "m",
        64,
        "s",
        &[Message::user("q")],
        &[],
        &mut |_| {},
        crate::config::ThinkingLevel::Off,
        0,
        &mut |_| {},
        None,
        None,
        true,
    );
    let elapsed = started.elapsed();
    assert!(res.is_err(), "500 must eventually error");
    assert!(res.unwrap_err().contains("upstream timeout"));
    // Retries use immediate backoff on timeouts but geometric (60s+) on 5xx,
    // so the whole thing should finish within the retry window, not hang.
    assert!(
        elapsed < Duration::from_secs(60 * 5 + 10),
        "retries bounded: {elapsed:?}"
    );
}

#[test]
fn mock_server_html_404_is_misrouted_fatal() {
    // An HTML 404 means the request never reached the model API (misrouted
    // baseUrl). It should be marked misrouted and NOT retried.
    let mock = MockServer::new();
    mock.set_response(MockResponse::HtmlError { status: 404 });
    let mut client = Client::new(ApiKind::OpenAi, &mock.url(), "k".to_string());
    client.set_cancel(Arc::new(AtomicBool::new(false)));

    let res = client.chat(
        "m",
        64,
        "s",
        &[Message::user("q")],
        &[],
        &mut |_| {},
        crate::config::ThinkingLevel::Off,
        0,
        &mut |_| {},
        None,
        None,
        true,
    );
    assert!(res.is_err(), "HTML 404 must error");
    assert!(
        res.unwrap_err().contains("misrouted"),
        "should mark as misrouted: {:?}",
        res.err()
    );
}
