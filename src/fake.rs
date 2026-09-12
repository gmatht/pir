//! Offline fake model for tests + UI puppetry (no network, no API keys).
//!
//! Enabled by setting `PIR_FAKE_MODEL=1`: [`crate::config::load_providers`]
//! appends a `fake` provider with one model (`fake/slow`), selectable with
//! `pir -m fake/slow`. Prompts drive scripted turns through `FAKE:`
//! directive lines (each on its own line; the `FAKE:` prefix is exact,
//! verbs match case-insensitively):
//!
//! ```text
//! FAKE: markup            — stream a markdown sample in paced chunks
//! FAKE: echo <text>       — stream <text> back slowly, then end
//! FAKE: sleep <secs>      — emit a real `bash` tool call `sleep N` (1..=120)
//! FAKE: type <ch> <cps> <secs> — stream <ch> at <cps> chars/sec for <secs>s
//! FAKE: think <text>      — stream <text> as reasoning chunks (on_think)
//! ```
//!
//! Directives run once per turn: tool directives emit real tool calls (which
//! the agent executes through the normal preflights), and the follow-up model
//! round — once tool results are in history — concludes the turn. A prompt
//! with no directives gets a canned echo reply. Cancellation is honored
//! between chunks, so ESC/ctrl-c kill a paced turn promptly like a real one.

use crate::types::{Block, Message, Role, Usage};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// One scripted action parsed from a `FAKE:` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive {
    /// Stream a markdown sample in paced chunks.
    Markup,
    /// Stream free text back slowly.
    Echo(String),
    /// Real `bash` tool call `sleep <secs>` (clamped 1..=120).
    Sleep(u64),
    /// Stream `ch` at `cps` chars/sec for `secs` seconds (clamped).
    Type { ch: char, cps: u64, secs: u64 },
    /// Stream reasoning text through the thinking channel (paced chunks).
    Think(String),
}

/// Parse `FAKE:` directive lines from prompt text. Verbs match
/// case-insensitively; malformed lines are skipped (a fake must never crash
/// a session); unknown verbs are skipped.
pub fn parse_directives(text: &str) -> Vec<Directive> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let Some(rest) = raw.trim().strip_prefix("FAKE:") else {
            continue;
        };
        let rest = rest.trim();
        let (verb, args) = match rest.split_once(char::is_whitespace) {
            Some((v, a)) => (v, a.trim()),
            None => (rest, ""),
        };
        match verb.to_ascii_lowercase().as_str() {
            "markup" => out.push(Directive::Markup),
            "think" if !args.is_empty() => out.push(Directive::Think(args.to_string())),
            "echo" if !args.is_empty() => out.push(Directive::Echo(args.to_string())),
            "sleep" => {
                if let Ok(secs) = args.parse::<u64>() {
                    out.push(Directive::Sleep(secs.clamp(1, 120)));
                }
            }
            "type" => {
                let mut parts = args.split_whitespace();
                let ch = parts.next().and_then(|s| s.chars().next());
                let cps = parts.next().and_then(|s| s.parse::<u64>().ok());
                let secs = parts.next().and_then(|s| s.parse::<u64>().ok());
                if let (Some(ch), Some(cps), Some(secs)) = (ch, cps, secs) {
                    out.push(Directive::Type {
                        ch,
                        cps: cps.clamp(1, 1000),
                        secs: secs.clamp(1, 120),
                    });
                }
            }
            _ => {}
        }
    }
    out
}

const MARKUP_DEMO: &str = "# Fake markup\n\nHello **bold** and *italic*.\n\n- one\n- two\n\n```sh\necho hi\n```\n";

/// Emit chunks through `on_text`, sleeping `gap` between them. Returns
/// `Err("request cancelled")` early when `cancel` flips — mirroring how a
/// real provider surfaces ESC/ctrl-c mid-stream.
fn emit_paced(
    chunks: &[String],
    gap: Duration,
    on_text: &mut dyn FnMut(&str),
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    for c in chunks {
        if cancel.load(Ordering::SeqCst) {
            return Err("request cancelled".to_string());
        }
        on_text(c);
        std::thread::sleep(gap);
    }
    Ok(())
}

/// Scripted stand-in for `Client::chat`: no network. `history` must contain
/// the latest user prompt; tool results already in history conclude the turn.
/// Reasoning goes through `on_think` (paced like a real reasoning stream).
pub fn fake_chat(
    history: &[Message],
    on_text: &mut dyn FnMut(&str),
    on_think: &mut dyn FnMut(&str),
    cancel: &Arc<AtomicBool>,
) -> Result<(Message, Usage), String> {
    let usage = Usage { input: 0, output: 0 };
    // Follow-up round (tool results NEWER than the latest user text):
    // conclude, never re-emit tools. Results from EARLIER turns (with a newer
    // user prompt after them) belong to history, not to this round.
    let mut last_text_idx = None;
    let mut last_results_idx = None;
    for (i, m) in history.iter().enumerate() {
        if m.role == Role::User
            && m.blocks.iter().any(|b| matches!(b, Block::Text(_)))
        {
            last_text_idx = Some(i);
        }
        if m.blocks.iter().any(|b| matches!(b, Block::ToolResult { .. })) {
            last_results_idx = Some(i);
        }
    }
    let followup = matches!((last_text_idx, last_results_idx), (Some(t), Some(r)) if r > t);
    if followup {
        let done = "fake: tools done.".to_string();
        on_text(&done);
        return Ok((
            Message { role: Role::Assistant, blocks: vec![Block::Text(done)] },
            usage,
        ));
    }
    let prompt = history
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| m.text())
        .unwrap_or_default();
    let directives = parse_directives(&prompt);
    if directives.is_empty() {
        // Canned echo: proves what the turn received (puppet asserts on it).
        let reply = format!("fake: you said {prompt:?}");
        let chunks: Vec<String> = reply
            .chars()
            .collect::<Vec<_>>()
            .chunks(8)
            .map(|c| c.iter().collect())
            .collect();
        emit_paced(&chunks, Duration::from_millis(30), on_text, cancel)?;
        return Ok((
            Message { role: Role::Assistant, blocks: vec![Block::Text(reply)] },
            usage,
        ));
    }
    let mut text = String::new();
    let mut think_text = String::new();
    let mut blocks: Vec<Block> = Vec::new();
    let mut tool_n = 0u32;
    for d in &directives {
        if cancel.load(Ordering::SeqCst) {
            return Err("request cancelled".to_string());
        }
        match d {
            Directive::Markup => {
                let chunks: Vec<String> = MARKUP_DEMO
                    .chars()
                    .collect::<Vec<_>>()
                    .chunks(12)
                    .map(|c| c.iter().collect())
                    .collect();
                emit_paced(&chunks, Duration::from_millis(120), on_text, cancel)?;
                text.push_str(MARKUP_DEMO);
            }
            Directive::Echo(e) => {
                let chunks: Vec<String> = e
                    .chars()
                    .collect::<Vec<_>>()
                    .chunks(8)
                    .map(|c| c.iter().collect())
                    .collect();
                emit_paced(&chunks, Duration::from_millis(100), on_text, cancel)?;
                text.push_str(e);
            }
            Directive::Sleep(secs) => {
                let ack = format!("fake: running `sleep {secs}`…\n");
                on_text(&ack);
                text.push_str(&ack);
                blocks.push(Block::ToolUse {
                    id: format!("fake-tool-{tool_n}"),
                    name: "bash".to_string(),
                    input: serde_json::json!({"command": format!("sleep {secs}")}),
                });
                tool_n += 1;
            }
            Directive::Type { ch, cps, secs } => {
                // ~10 ticks/sec; each tick emits its share, paced.
                let (ch, cps, secs) = (ch, *cps, *secs);
                let ticks = secs.saturating_mul(10).max(1);
                let per_tick = (cps.max(1) / 10).max(1) as usize;
                for _ in 0..ticks {
                    if cancel.load(Ordering::SeqCst) {
                        return Err("request cancelled".to_string());
                    }
                    let chunk: String = std::iter::repeat_n(*ch, per_tick).collect();
                    on_text(&chunk);
                    text.push_str(&chunk);
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            Directive::Think(t) => {
                let chunks: Vec<String> = t.split_inclusive(' ').map(str::to_string).collect();
                for chunk in &chunks {
                    if cancel.load(Ordering::SeqCst) {
                        return Err("request cancelled".to_string());
                    }
                    on_think(chunk);
                    think_text.push_str(chunk);
                    std::thread::sleep(Duration::from_millis(150));
                }
            }
        }
    }
    if !think_text.is_empty() {
        blocks.push(Block::Thinking { text: think_text });
    }
    if !text.is_empty() {
        blocks.insert(0, Block::Text(text));
    }
    Ok((Message { role: Role::Assistant, blocks }, usage))
}

#[cfg(test)]
mod fake_tests {
    use super::*;

    #[test]
    fn parses_all_directives() {
        let ds = parse_directives("do stuff\nFAKE: markup\nFAKE: echo hi there\nFAKE: sleep 9\nFAKE: type x 20 5\nFAKE: think pondering this");
        assert_eq!(
            ds,
            vec![
                Directive::Markup,
                Directive::Echo("hi there".to_string()),
                Directive::Sleep(9),
                Directive::Type { ch: 'x', cps: 20, secs: 5 },
                Directive::Think("pondering this".to_string()),
            ]
        );
    }

    #[test]
    fn malformed_lines_skipped() {
        assert!(parse_directives("FAKE: sleep lots").is_empty());
        assert!(parse_directives("FAKE: type x fast long").is_empty());
        assert!(parse_directives("FAKE: teleport").is_empty());
        assert!(parse_directives("fake: sleep 3").is_empty()); // prefix is exact
        assert!(parse_directives("just chatting").is_empty());
        // clamps, not rejects
        assert_eq!(parse_directives("FAKE: sleep 9999"), vec![Directive::Sleep(120)]);
        assert_eq!(
            parse_directives("FAKE: type ab 0 0"),
            vec![Directive::Type { ch: 'a', cps: 1, secs: 1 }]
        );
    }

    #[test]
    fn sleep_round_emits_bash_tool_use() {
        let cancel = Arc::new(AtomicBool::new(false));
        let history = vec![Message::user("FAKE: sleep 7")];
        let mut seen = String::new();
        let (msg, _) = fake_chat(&history, &mut |t| seen.push_str(t), &mut |_| {}, &cancel).unwrap();
        let uses = msg.tool_uses();
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].1, "bash");
        assert_eq!(uses[0].2["command"], serde_json::json!("sleep 7"));
        assert!(seen.contains("sleep 7"));
    }

    #[test]
    fn followup_round_concludes_without_tools() {
        let cancel = Arc::new(AtomicBool::new(false));
        let history = vec![
            Message::user("FAKE: sleep 7"),
            Message {
                role: Role::Assistant,
                blocks: vec![Block::ToolUse {
                    id: "fake-tool-0".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "sleep 7"}),
                }],
            },
            Message {
                role: Role::User,
                blocks: vec![Block::ToolResult {
                    tool_use_id: "fake-tool-0".into(),
                    content: "".into(),
                    is_error: false,
                }],
            },
        ];
        let (msg, _) = fake_chat(&history, &mut |_| {}, &mut |_| {}, &cancel).unwrap();
        assert!(msg.tool_uses().is_empty());
        assert!(msg.text().contains("tools done"));
    }

    #[test]
    fn old_results_do_not_swallow_new_prompt() {
        // Results from an EARLIER turn with a newer user prompt after them:
        // the new prompt's directives run (here: none, so the echo reply).
        let cancel = Arc::new(AtomicBool::new(false));
        let history = vec![
            Message::user("FAKE: sleep 7"),
            Message {
                role: Role::User,
                blocks: vec![Block::ToolResult {
                    tool_use_id: "fake-tool-0".into(),
                    content: "".into(),
                    is_error: false,
                }],
            },
            Message::user("hel"),
        ];
        let (msg, _) = fake_chat(&history, &mut |_| {}, &mut |_| {}, &cancel).unwrap();
        assert!(msg.text().contains("\"hel\""));
    }

    #[test]
    fn cancel_aborts_paced_emit() {
        let cancel = Arc::new(AtomicBool::new(true));
        let history = vec![Message::user("FAKE: echo hello")];
        let r = fake_chat(&history, &mut |_| {}, &mut |_| {}, &cancel);
        assert_eq!(r.unwrap_err(), "request cancelled");
    }

    #[test]
    fn default_reply_echoes_prompt() {
        let cancel = Arc::new(AtomicBool::new(false));
        let history = vec![Message::user("hel")];
        let (msg, _) = fake_chat(&history, &mut |_| {}, &mut |_| {}, &cancel).unwrap();
        assert!(msg.text().contains("\"hel\""));
    }

    #[test]
    fn think_streams_through_thinking_channel() {
        let cancel = Arc::new(AtomicBool::new(false));
        let history = vec![Message::user("FAKE: think considering options carefully")];
        let mut thought = String::new();
        let mut said = String::new();
        let (msg, _) = fake_chat(
            &history,
            &mut |t| said.push_str(t),
            &mut |t| thought.push_str(t),
            &cancel,
        )
        .unwrap();
        assert!(thought.contains("considering"), "thinking must arrive via on_think");
        assert!(said.is_empty(), "thinking must not leak into the text channel");
        assert!(msg.tool_uses().is_empty());
    }
}
