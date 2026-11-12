use std::time::Duration;

use crate::sample::Spread;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Workspace,
    Skill,
    End,
    Read,
    Write,
    Send,
}

#[derive(Debug, Clone, Copy)]
pub struct ToolProfile {
    pub name: &'static str,
    pub share: f64,
    pub latency: Spread,
    pub error_rate: f64,
    pub kind: Kind,
}

const fn tool(
    name: &'static str,
    share: f64,
    p50: f64,
    p90: f64,
    error_pct: f64,
    kind: Kind,
) -> ToolProfile {
    ToolProfile {
        name,
        share,
        latency: Spread::new(p50, p90),
        error_rate: error_pct / 100.0,
        kind,
    }
}

pub const ASSISTANT_TOOLS: &[ToolProfile] = &[
    tool("read_file", 53.37, 0.0, 0.0, 0.5, Kind::Workspace),
    tool("load_skill", 18.46, 0.0, 0.0, 0.0, Kind::Skill),
    tool("getOrderDetails", 6.45, 216.0, 749.0, 0.3, Kind::Read),
    tool("sendMessage", 5.92, 446.0, 1376.0, 23.5, Kind::Send),
    tool("orders", 2.44, 710.0, 1340.0, 0.7, Kind::Read),
    tool("followups", 2.26, 167.0, 533.0, 4.3, Kind::Write),
    tool("updateProfile", 1.99, 238.0, 928.0, 0.0, Kind::Write),
    tool("grep", 1.74, 0.0, 0.0, 0.1, Kind::Workspace),
    tool("noReply", 1.66, 0.0, 0.0, 0.6, Kind::End),
    tool("checkEligibility", 1.30, 3389.0, 13242.0, 0.3, Kind::Write),
    tool("recordStatus", 1.27, 288.0, 771.0, 1.4, Kind::Write),
    tool("memory", 0.62, 140.0, 440.0, 7.9, Kind::Write),
    tool("searchCatalog", 0.60, 405.0, 1088.0, 0.9, Kind::Read),
    tool("reportToTeam", 0.43, 134.0, 418.0, 0.1, Kind::Write),
    tool("recordDecision", 0.24, 405.0, 1437.0, 2.3, Kind::Write),
    tool(
        "reportBug",
        0.23,
        118.0,
        405.0,
        0.1,
        Kind::Write,
    ),
    tool("glob", 0.20, 0.0, 0.0, 0.3, Kind::Workspace),
    tool("booking", 0.14, 858.0, 2290.0, 10.1, Kind::Write),
    tool("logCallback", 0.12, 217.0, 645.0, 0.0, Kind::Write),
    tool(
        "manageChannels",
        0.12,
        213.0,
        700.0,
        25.0,
        Kind::Write,
    ),
    tool("submitOrder", 0.09, 344.0, 3883.0, 31.3, Kind::Write),
    tool("appointments", 0.07, 160.0, 589.0, 14.5, Kind::Write),
    tool("addComment", 0.07, 143.0, 425.0, 0.0, Kind::Write),
    tool(
        "recordAnswers",
        0.04,
        222.0,
        822.0,
        2.2,
        Kind::Write,
    ),
];

pub fn find(name: &str) -> Option<&'static ToolProfile> {
    ASSISTANT_TOOLS.iter().find(|tool| tool.name == name)
}

#[derive(Debug, Clone, Copy)]
pub struct ModelTiming {
    pub headers_ms: Spread,
    pub thinking_ms: Spread,
    pub text_tokens: Spread,
    pub tokens_per_second: f64,
    pub tool_calls_per_step: Spread,
    pub tool_calls_per_turn: Spread,
    pub prefill_ms_per_1k_uncached: f64,
    pub cache_readable_after: Duration,
}

pub const TERRA: ModelTiming = ModelTiming {
    headers_ms: Spread::new(800.0, 1800.0),
    thinking_ms: Spread::new(450.0, 1500.0),
    text_tokens: Spread::new(60.0, 250.0),
    tokens_per_second: 80.0,
    tool_calls_per_step: Spread::new(3.0, 6.0),
    tool_calls_per_turn: Spread::new(16.0, 26.0),
    prefill_ms_per_1k_uncached: 40.0,
    cache_readable_after: Duration::from_secs(2),
};
