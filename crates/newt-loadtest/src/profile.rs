use std::time::Duration;

use serde::Deserialize;

use crate::sample::Spread;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Workspace,
    Skill,
    End,
    Read,
    Write,
    Send,
}

#[derive(Debug, Clone)]
pub struct ToolProfile {
    pub name: String,
    pub share: f64,
    pub latency: Spread,
    pub error_rate: f64,
    pub kind: Kind,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolEntry {
    name: String,
    share: f64,
    p50_ms: f64,
    p90_ms: f64,
    error_pct: f64,
    kind: Kind,
}

impl From<ToolEntry> for ToolProfile {
    fn from(entry: ToolEntry) -> Self {
        Self {
            name: entry.name,
            share: entry.share,
            latency: Spread::new(entry.p50_ms, entry.p90_ms),
            error_rate: entry.error_pct / 100.0,
            kind: entry.kind,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub tools: Vec<ToolProfile>,
    pub skills: Vec<String>,
    pub pages: Vec<String>,
    pub file_path_prefix: String,
    pub skill_tools: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    tools: Vec<ToolEntry>,
    skills: Vec<String>,
    pages: Vec<String>,
    #[serde(default)]
    file_path_prefix: String,
    #[serde(default = "default_skill_tools")]
    skill_tools: Vec<String>,
}

fn default_skill_tools() -> Vec<String> {
    ["load_skill", "read_file"].map(str::to_owned).to_vec()
}

fn tool(
    name: &str,
    share: f64,
    p50_ms: f64,
    p90_ms: f64,
    error_pct: f64,
    kind: Kind,
) -> ToolProfile {
    ToolEntry {
        name: name.to_owned(),
        share,
        p50_ms,
        p90_ms,
        error_pct,
        kind,
    }
    .into()
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            tools: vec![
                tool("read_file", 53.4, 0.0, 0.0, 0.5, Kind::Workspace),
                tool("load_skill", 18.5, 0.0, 0.0, 0.0, Kind::Skill),
                tool("lookup_order", 6.5, 216.0, 749.0, 0.3, Kind::Read),
                tool("send_message", 5.9, 446.0, 1376.0, 23.5, Kind::Send),
                tool("search_catalog", 3.0, 710.0, 1340.0, 0.7, Kind::Read),
                tool("schedule_follow_up", 2.3, 167.0, 533.0, 4.3, Kind::Write),
                tool("update_customer", 2.0, 238.0, 928.0, 0.0, Kind::Write),
                tool("grep", 1.7, 0.0, 0.0, 0.1, Kind::Workspace),
                tool("no_reply", 1.7, 0.0, 0.0, 0.6, Kind::End),
                tool("check_eligibility", 1.3, 3389.0, 13242.0, 0.3, Kind::Write),
                tool("record_status", 1.3, 288.0, 771.0, 1.4, Kind::Write),
                tool("save_note", 0.6, 140.0, 440.0, 7.9, Kind::Write),
                tool("escalate_to_team", 0.5, 134.0, 418.0, 0.1, Kind::Write),
                tool("glob", 0.2, 0.0, 0.0, 0.3, Kind::Workspace),
                tool("book_call", 0.2, 858.0, 2290.0, 10.1, Kind::Write),
            ],
            skills: ["writing", "onboarding", "billing", "returns", "escalation"]
                .map(str::to_owned)
                .to_vec(),
            pages: [
                "since-last-turn.md",
                "memory.md",
                "calendar.md",
                "orders.md",
                "profile/profile.md",
                "profile/preferences.md",
                "conversation/latest.md",
                "account.md",
            ]
            .map(str::to_owned)
            .to_vec(),
            file_path_prefix: String::new(),
            skill_tools: default_skill_tools(),
        }
    }
}

impl Profile {
    pub fn parse(raw: &str) -> Result<Self, serde_json::Error> {
        let file: ProfileFile = serde_json::from_str(raw)?;
        Ok(Self {
            tools: file.tools.into_iter().map(ToolProfile::from).collect(),
            skills: file.skills,
            pages: file.pages,
            file_path_prefix: file.file_path_prefix,
            skill_tools: file.skill_tools,
        })
    }

    pub fn find(&self, name: &str) -> Option<&ToolProfile> {
        self.tools.iter().find(|tool| tool.name == name)
    }
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
    /// The wait between the last output item and `response.completed`. Against
    /// gpt-6.1-terra in production on 2026-11-16, a response with one small tool
    /// call ended 548 ms (p50) and 753 ms (p90) after the call started streaming;
    /// this is that time less the arguments themselves, an estimate.
    pub completion_tail_ms: Spread,
    /// How soon after a request its prompt can be read from the cache. Against
    /// `OpenAI` on 2026-11-12 the next request hit the cache every time it
    /// started 1.1 s or more after the first.
    pub cache_readable_after: Duration,
}

pub const REFERENCE_TIMING: ModelTiming = ModelTiming {
    headers_ms: Spread::new(800.0, 1800.0),
    thinking_ms: Spread::new(450.0, 1500.0),
    text_tokens: Spread::new(60.0, 250.0),
    tokens_per_second: 80.0,
    tool_calls_per_step: Spread::new(3.0, 6.0),
    tool_calls_per_turn: Spread::new(16.0, 26.0),
    prefill_ms_per_1k_uncached: 40.0,
    completion_tail_ms: Spread::new(300.0, 500.0),
    cache_readable_after: Duration::from_secs(1),
};

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use super::*;

    #[test]
    fn a_profile_file_lists_tools_skills_and_pages() {
        let profile = Profile::parse(
            r#"{
                "tools": [{ "name": "send", "share": 2.0, "p50_ms": 100, "p90_ms": 400, "error_pct": 5, "kind": "send" }],
                "skills": ["writing"],
                "pages": ["memory.md"]
            }"#,
        )
        .unwrap();
        let send = profile.find("send").unwrap();
        assert_eq!(send.kind, Kind::Send);
        assert!((send.error_rate - 0.05).abs() < f64::EPSILON);
        assert_eq!(profile.skills, vec!["writing"]);
        assert_eq!(profile.pages, vec!["memory.md"]);
    }

    #[test]
    fn unknown_fields_and_kinds_are_rejected() {
        assert!(
            Profile::parse(r#"{ "tools": [], "skills": [], "pages": [], "extra": 1 }"#).is_err()
        );
        assert!(
            Profile::parse(
                r#"{ "tools": [{ "name": "x", "share": 1, "p50_ms": 1, "p90_ms": 1, "error_pct": 0, "kind": "teleport" }], "skills": [], "pages": [] }"#
            )
            .is_err()
        );
    }
}
