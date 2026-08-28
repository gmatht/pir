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
