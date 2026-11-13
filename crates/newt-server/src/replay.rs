//! Replaying a session: its events sent again, turn by turn, to another
//! session (usually of another agent, with a new prompt or model), and what
//! each turn did then and now side by side.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use newt_core::message::{Message, Part, Role};
use serde::Deserialize;
use serde_json::json;

const POLL: Duration = Duration::from_millis(300);
const TURN_TIMEOUT: Duration = Duration::from_mins(5);
const SHOWN_CHARS: usize = 300;

/// One turn of a session's history: the events it read and what it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub id: String,
    pub events: Vec<String>,
    pub did: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Entry {
    turn: String,
    entry: Message,
}

#[derive(Debug, Deserialize)]
struct History {
    entries: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
struct Status {
    lease_owner: Option<String>,
    pending_events: i64,
    history_entries: i64,
}

/// Splits history into turns, in order. A user message's text is an event;
/// an assistant's text and tool calls are what the turn did.
fn turns(entries: Vec<Entry>) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for Entry { turn, entry } in entries {
        if turns.last().is_none_or(|last| last.id != turn) {
            turns.push(Turn {
                id: turn,
                events: Vec::new(),
                did: Vec::new(),
            });
        }
        let Some(current) = turns.last_mut() else {
            continue;
        };
        for part in entry.parts {
            match (&entry.role, part) {
                (Role::User, Part::Text { text }) => current.events.push(text),
                (Role::Assistant, Part::Text { text }) => current.did.push(shorten(&text)),
                (Role::Assistant, Part::ToolCall(call)) => current.did.push(format!(
                    "{}({})",
                    call.name,
                    shorten(&call.arguments.to_string())
                )),
                _ => {}
            }
        }
    }
    turns
}

fn shorten(text: &str) -> String {
    let text = text.trim().replace('\n', " ");
    if text.chars().count() <= SHOWN_CHARS {
        return text;
    }
    let mut short: String = text.chars().take(SHOWN_CHARS).collect();
    short.push('…');
    short
}

/// A session on a newt server: `{agent}/{key}`.
#[derive(Debug, Clone)]
pub struct SessionRef {
    pub agent: String,
    pub key: String,
}

impl std::str::FromStr for SessionRef {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text.split_once('/') {
            Some((agent, key)) if !agent.is_empty() && !key.is_empty() && !key.contains('/') => {
                Ok(Self {
                    agent: agent.to_owned(),
                    key: key.to_owned(),
                })
            }
            _ => Err(format!("`{text}` is not agent/key")),
        }
    }
}

#[derive(Debug)]
pub struct Replay {
    pub base_url: String,
    pub token: String,
    pub from: SessionRef,
    pub into: SessionRef,
    /// Replay from this turn (0-based); the turns before it are forked in.
    pub from_turn: usize,
    pub turns: Option<usize>,
}

impl Replay {
    fn url(&self, session: &SessionRef, path: &str) -> String {
        format!(
            "{}/v1/agents/{}/sessions/{}{path}",
            self.base_url.trim_end_matches('/'),
            session.agent,
            session.key
        )
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        client: &reqwest::Client,
        url: String,
    ) -> anyhow::Result<Option<T>> {
        let response = client.get(&url).bearer_auth(&self.token).send().await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = response.error_for_status().with_context(|| url.clone())?;
        Ok(Some(response.json().await?))
    }

    async fn history(
        &self,
        client: &reqwest::Client,
        session: &SessionRef,
    ) -> anyhow::Result<Vec<Turn>> {
        let history: Option<History> = self.get(client, self.url(session, "/history")).await?;
        Ok(history
            .map(|history| turns(history.entries))
            .unwrap_or_default())
    }

    /// Runs the replay and writes the comparison to `out`.
    pub async fn run(&self, out: &mut impl std::io::Write) -> anyhow::Result<()> {
        let client = reqwest::Client::new();
        let original = self.history(&client, &self.from).await?;
        if original.is_empty() {
            bail!(
                "{}/{} has no history to replay",
                self.from.agent,
                self.from.key
            );
        }
        let status: Option<Status> = self.get(&client, self.url(&self.into, "")).await?;
        if status.is_some() {
            bail!(
                "{}/{} already exists; replay into a new session",
                self.into.agent,
                self.into.key
            );
        }
        if self.from_turn > 0 {
            let through = original
                .get(self.from_turn - 1)
                .with_context(|| format!("the session has only {} turns", original.len()))?;
            client
                .post(self.url(&self.from, "/fork"))
                .bearer_auth(&self.token)
                .json(&json!({ "into": self.into.key, "agent": self.into.agent, "through_turn": through.id }))
                .send()
                .await?
                .error_for_status()
                .context("forking the session")?;
        }
        let end = self.turns.map_or(original.len(), |turns| {
            (self.from_turn + turns).min(original.len())
        });
        for (index, turn) in original.iter().enumerate().take(end).skip(self.from_turn) {
            let known = self.history(&client, &self.into).await?.len();
            let before = self.history_entries(&client).await?;
            for (number, text) in turn.events.iter().enumerate() {
                client
                    .post(self.url(&self.into, "/events"))
                    .bearer_auth(&self.token)
                    .json(&json!({ "id": format!("replay-{index}-{number}"), "text": text, "policy": "queue" }))
                    .send()
                    .await?
                    .error_for_status()
                    .context("sending an event")?;
            }
            let committed = self.wait_for_turn(&client).await? > before;
            let now: Vec<String> = self
                .history(&client, &self.into)
                .await?
                .into_iter()
                .skip(known)
                .flat_map(|replayed| replayed.did)
                .collect();
            write_turn(out, index, turn, &now, committed)?;
        }
        Ok(())
    }

    async fn history_entries(&self, client: &reqwest::Client) -> anyhow::Result<i64> {
        let status: Option<Status> = self.get(client, self.url(&self.into, "")).await?;
        Ok(status.map_or(0, |status| status.history_entries))
    }

    /// Waits until the target session has no pending events and no turn
    /// running, and returns its history length then.
    async fn wait_for_turn(&self, client: &reqwest::Client) -> anyhow::Result<i64> {
        let started = Instant::now();
        loop {
            tokio::time::sleep(POLL).await;
            let status: Option<Status> = self.get(client, self.url(&self.into, "")).await?;
            if let Some(status) = status
                && status.pending_events == 0
                && status.lease_owner.is_none()
            {
                return Ok(status.history_entries);
            }
            if started.elapsed() > TURN_TIMEOUT {
                bail!(
                    "the replayed turn did not finish within {} s",
                    TURN_TIMEOUT.as_secs()
                );
            }
        }
    }
}

fn write_turn(
    out: &mut impl std::io::Write,
    index: usize,
    turn: &Turn,
    now: &[String],
    committed: bool,
) -> std::io::Result<()> {
    writeln!(out, "\nturn {index}")?;
    for event in &turn.events {
        writeln!(out, "  > {}", shorten(event))?;
    }
    writeln!(out, "  then:")?;
    for did in &turn.did {
        writeln!(out, "    {did}")?;
    }
    writeln!(out, "  now:")?;
    if !committed {
        writeln!(
            out,
            "    (the turn committed nothing: it failed, see the server log)"
        )?;
    }
    for did in now {
        writeln!(out, "    {did}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use super::{Entry, Turn, turns};
    use serde_json::json;

    #[test]
    fn history_splits_into_turns_of_events_and_what_was_done() {
        let entries: Vec<Entry> = serde_json::from_value(json!([
            { "turn": "t1", "entry": { "role": "user", "parts": [{ "type": "text", "text": "add milk" }] } },
            { "turn": "t1", "entry": { "role": "assistant", "parts": [
                { "type": "tool_call", "id": "c1", "name": "send_message", "arguments": { "text": "Added." } }
            ] } },
            { "turn": "t1", "entry": { "role": "user", "parts": [
                { "type": "tool_result", "call_id": "c1", "content": "sent", "is_error": false }
            ] } },
            { "turn": "t2", "entry": { "role": "user", "parts": [{ "type": "text", "text": "thanks" }] } },
            { "turn": "t2", "entry": { "role": "assistant", "parts": [{ "type": "text", "text": "Any time." }] } }
        ]))
        .unwrap();
        assert_eq!(
            turns(entries),
            [
                Turn {
                    id: "t1".into(),
                    events: vec!["add milk".into()],
                    did: vec![r#"send_message({"text":"Added."})"#.into()],
                },
                Turn {
                    id: "t2".into(),
                    events: vec!["thanks".into()],
                    did: vec!["Any time.".into()],
                },
            ]
        );
    }
}
