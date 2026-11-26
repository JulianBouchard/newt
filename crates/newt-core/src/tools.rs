use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::message::ToolCall;
use crate::prompt::ToolSpec;
use crate::skills::Skill;
use crate::workspace::{Page, PageKey};

mod builtin;
mod builtin_set;
mod end_turn;
#[cfg(test)]
mod fuzz;
mod workspace;

pub(crate) use builtin::remember;
pub use builtin::{END_TURN, REMEMBER, REMEMBER_LIMIT, builtin_tools};
pub use builtin_set::{BUILTIN_TOOL_NAMES, BuiltinTools};
pub use end_turn::{ENDS_TURN_NOTE, EndTurn};
pub(crate) use workspace::{is_workspace_tool, run_workspace_tool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    Ephemeral,
    Durable,
    Outward,
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub spec: ToolSpec,
    pub retention: Retention,
    pub invalidates: Vec<PageKey>,
    /// A successful call ends the turn once every call of its batch succeeds.
    pub ends_turn: bool,
    /// A call repeated with the same idempotency key takes effect once, so a
    /// `durable` call interrupted by a crash is run again instead of reported.
    pub idempotent: bool,
}

impl ToolDef {
    pub fn new(spec: ToolSpec, retention: Retention) -> Self {
        Self {
            spec,
            retention,
            invalidates: Vec::new(),
            ends_turn: false,
            idempotent: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invalidate: Vec<PageKey>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<Page>,
    /// Overrides the tool's [`ToolDef::ends_turn`] for this one call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_turn: Option<bool>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Self::default()
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PageRead {
    pub path: String,
    pub version: String,
}

/// Where a running call reports progress, in the words of the tool's
/// server. Newt passes each report to the live feed as `tool_progress`.
#[derive(Debug, Clone)]
pub struct Progress(pub tokio::sync::mpsc::UnboundedSender<String>);

impl Progress {
    pub fn report(&self, message: impl Into<String>) {
        let _ = self.0.send(message.into());
    }
}

impl PartialEq for Progress {
    fn eq(&self, other: &Self) -> bool {
        self.0.same_channel(&other.0)
    }
}

impl Eq for Progress {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CallContext {
    pub idempotency_key: String,
    pub skills_loaded: Vec<String>,
    pub pages_read: Vec<PageRead>,
    pub traceparent: Option<String>,
    #[serde(skip)]
    pub progress: Option<Progress>,
}

pub trait ToolHost: Send + Sync {
    fn call(
        &self,
        call: &ToolCall,
        context: &CallContext,
    ) -> impl Future<Output = ToolOutput> + Send;

    /// Fetch a skill this host serves ([`Skill::served_by`] is set), or one of
    /// its files when `file` is given. The default serves none.
    fn load_skill(
        &self,
        skill: &Skill,
        file: Option<&str>,
    ) -> impl Future<Output = ToolOutput> + Send {
        let _ = file;
        std::future::ready(ToolOutput::error(format!(
            "The skill `{}` cannot be loaded here.",
            skill.name
        )))
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoTools;

impl ToolHost for NoTools {
    fn call(
        &self,
        call: &ToolCall,
        _context: &CallContext,
    ) -> impl Future<Output = ToolOutput> + Send {
        std::future::ready(ToolOutput::error(format!("unknown tool `{}`", call.name)))
    }
}

fn parse<'a, T: Deserialize<'a>>(call: &'a ToolCall) -> Result<T, String> {
    T::deserialize(&call.arguments)
        .map_err(|error| format!("invalid arguments for {}: {error}", call.name))
}
