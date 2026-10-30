use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// Internal message model — maps cleanly onto both the Anthropic
/// content-block format and OpenAI tool-call format.
#[derive(Debug, Clone)]
pub enum Block {
    Text(String),
    /// Model reasoning / "extended thinking" content. Parsed from the provider's
    /// thinking stream (Anthropic `thinking_delta`, OpenAI `reasoning`). Shown on
    /// the terminal only when show-thinking is enabled, and never re-sent to the
    /// model (the request builder drops it).
    Thinking { text: String },
    ToolUse { id: String, name: String, input: Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<Block>,
}

impl Message {
    pub fn user(text: &str) -> Self {
        Message { role: Role::User, blocks: vec![Block::Text(text.to_string())] }
    }

    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|b| match b { Block::Text(t) => Some(t.as_str()), _ => None })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The joined reasoning/think the message (for UI display), or "".
    pub fn thinking(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|b| match b { Block::Thinking { text } => Some(text.as_str()), _ => None })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn tool_uses(&self) -> Vec<(&str, &str, &Value)> {
        self.blocks
            .iter()
            .filter_map(|b| match b {
                Block::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
                _ => None,
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
            || self.blocks.iter().all(|b| matches!(b, Block::Text(t) if t.trim().is_empty()))
    }
}

/// Chars per token for tool-result payloads (code, logs, shell output).
/// Measured live 2026-10-27 on deepseek-v4.1-flash: 196,800 chars of Go source
/// → 68,431 real tokens (2.87 chars/token) and 270,000 chars of JSON logs →
/// 87,031 tokens (3.10), against 6.16 for prose. `bytes/4` undercounts tool
/// results by 25-35%, so a long coding session can look safely under budget
/// while actually overflowing the window — and the provider rejects the whole
/// request with an opaque HTTP 400 (`invalid parameters`, param null). `/2`
/// keeps a margin over the densest measured payload; prose-like blocks keep
/// `/4` (see `Block::est_tokens`).
pub(crate) const TOOL_RESULT_CHARS_PER_TOKEN: usize = 2;

impl Block {
    /// Conservative token estimate for context budgeting. Prose-like blocks
    /// (text, thinking, short tool-call JSON) use `bytes/4`; tool *results*
    /// are code/logs/shell output and tokenize much denser
    /// (`TOOL_RESULT_CHARS_PER_TOKEN`). Undercounting here is what lets an
    /// over-window request reach the gateway, which rejects it with an opaque
    /// HTTP 400 — so this errs toward overcounting, which only trims or
    /// compacts a little earlier.
    pub(crate) fn est_tokens(&self) -> usize {
        match self {
            Block::Text(t) => t.len() / 4,
            Block::Thinking { text } => text.len() / 4,
            Block::ToolUse { input, .. } => input.to_string().len() / 4 + 16,
            Block::ToolResult { content, .. } => {
                content.len() / TOOL_RESULT_CHARS_PER_TOKEN + 16
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
}

impl Usage {
    /// Estimate the USD cost of this usage at the given per-1k-token prices
    /// `(input $/1k, output $/1k)`. Returns `None` when no price is known for
    /// the model (so callers can display tokens without a fabricated cost).
    pub fn cost(&self, price: Option<(f64, f64)>) -> Option<f64> {
        let (in_p, out_p) = price?;
        let cost = self.input as f64 / 1000.0 * in_p + self.output as f64 / 1000.0 * out_p;
        Some(cost)
    }
}
