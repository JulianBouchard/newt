//! Skills served over MCP (the `io.modelcontextprotocol/skills` extension,
//! SEP-2640): listed when a server connects, and fetched, verified and cached
//! only when the model loads one.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use newt_core::skills::Skill;
use newt_core::tools::ToolOutput;
use rmcp::model::{
    PaginatedRequestParams, ReadResourceRequestParams, ResourceContents, Skill as Entry,
    SkillResource, SkillResources,
};
use serde_json::Value;

use super::{Client, McpHost, Remote};

const MAX_FILES: usize = 512;
const MAX_BYTES: u64 = 16 * 1024 * 1024;
const SKILL_FILE: &str = "SKILL.md";

/// A skill one server serves, as its listing described it.
pub(super) struct Served {
    remote: Remote,
    entry: Entry,
    files: Vec<SkillResource>,
    root: String,
}

/// Verified file contents, by server and URI, with the digest they matched.
pub(super) type Verified = Arc<Mutex<HashMap<(String, String), (String, String)>>>;

pub(super) async fn list(
    server: &str,
    client: &Arc<Client>,
) -> Result<Vec<(Skill, Served)>, String> {
    if !client
        .peer_info()
        .is_some_and(|info| info.capabilities.supports_skills())
    {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    let mut cursor = None;
    loop {
        let page = client
            .list_skills(Some(PaginatedRequestParams::default().with_cursor(cursor)))
            .await
            .map_err(|error| format!("MCP server `{server}`: listing skills: {error}"))?;
        entries.extend(page.skills);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    let mut skills = Vec::new();
    let mut names = HashMap::new();
    for entry in entries {
        let (path, files) = match check(&entry) {
            Ok(checked) => checked,
            Err(why) => {
                tracing::warn!(server, uri = %entry.uri, "MCP skill not offered: {why}");
                continue;
            }
        };
        let name = format!("{server}/{path}");
        if let Some(other) = names.insert(name.clone(), entry.uri.clone()) {
            return Err(format!(
                "MCP server `{server}`: skills `{other}` and `{}` would both be named `{name}`",
                entry.uri
            ));
        }
        let skill = Skill {
            name,
            description: entry.frontmatter.description.trim().to_owned(),
            content: String::new(),
            served_by: Some(server.to_owned()),
        };
        let root = entry
            .uri
            .strip_suffix(SKILL_FILE)
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_owned();
        let remote = Remote {
            client: Arc::clone(client),
            server: server.to_owned(),
        };
        skills.push((
            skill,
            Served {
                remote,
                entry,
                files,
                root,
            },
        ));
    }
    Ok(skills)
}

/// The skill's path (its URI without the scheme and `/SKILL.md`) and its
/// files, or why newt will not offer it.
fn check(entry: &Entry) -> Result<(String, Vec<SkillResource>), String> {
    let SkillResources::Files(files) = &entry.resources else {
        return Err(
            "its files are generated (`resources: \"dynamic\"`), so newt cannot verify them"
                .to_owned(),
        );
    };
    let path = entry
        .uri
        .split_once("://")
        .and_then(|(_, rest)| rest.strip_suffix(SKILL_FILE))
        .and_then(|rest| rest.strip_suffix('/'))
        .filter(|path| !path.is_empty())
        .ok_or("its URI does not end in `/SKILL.md`")?;
    if path.rsplit('/').next() != Some(entry.frontmatter.name.as_str()) {
        return Err(format!(
            "its path does not end in its name `{}`",
            entry.frontmatter.name
        ));
    }
    if files.len() > MAX_FILES {
        return Err(format!("it has more than {MAX_FILES} files"));
    }
    if files.iter().map(|file| file.size).sum::<u64>() > MAX_BYTES {
        return Err(format!("its files add up to more than {MAX_BYTES} bytes"));
    }
    if !files.iter().any(|file| file.uri == entry.uri) {
        return Err("its file list leaves out its own SKILL.md".to_owned());
    }
    let root = entry.uri.trim_end_matches(SKILL_FILE);
    if let Some(stray) = files.iter().find(|file| !file.uri.starts_with(root)) {
        return Err(format!("it lists `{}`, outside the skill", stray.uri));
    }
    Ok((path.to_owned(), files.clone()))
}

impl McpHost {
    pub(super) async fn load_served(&self, skill: &Skill, file: Option<&str>) -> ToolOutput {
        let Some(served) = self.skills.get(&skill.name) else {
            return ToolOutput::error(format!("There is no skill named `{}`.", skill.name));
        };
        let uri = match file {
            None => served.entry.uri.clone(),
            Some(file) if is_relative(file) => format!("{}/{file}", served.root),
            Some(file) => {
                return ToolOutput::error(format!(
                    "`{file}` is not a path inside the skill; give it relative to the skill, without `..`."
                ));
            }
        };
        let Some(listed) = served.files.iter().find(|listed| listed.uri == uri) else {
            return ToolOutput::error(format!(
                "The skill `{}` has no file `{}`. Its files: {}.",
                skill.name,
                file.unwrap_or(SKILL_FILE),
                file_list(served).join(", ")
            ));
        };
        let content = match self.verified(served, listed).await {
            Ok(content) => content,
            Err(error) => return error,
        };
        if file.is_some() {
            return ToolOutput::ok(content);
        }
        let others = file_list(served);
        if others.len() <= 1 {
            return ToolOutput::ok(content);
        }
        let mut out = content;
        out.push_str("\n\n<skill_files>\nRead one with load_skill and `file`:\n");
        for other in others.iter().filter(|other| *other != SKILL_FILE) {
            out.push_str("- ");
            out.push_str(other);
            out.push('\n');
        }
        out.push_str("</skill_files>");
        ToolOutput::ok(out)
    }

    async fn verified(
        &self,
        served: &Served,
        listed: &SkillResource,
    ) -> Result<String, ToolOutput> {
        let key = (served.remote.server.clone(), listed.uri.clone());
        if let Some((digest, content)) = cache(&self.verified).get(&key)
            && *digest == listed.digest
        {
            return Ok(content.clone());
        }
        let read = tokio::time::timeout(
            self.limits.quiet,
            served
                .remote
                .client
                .read_resource(ReadResourceRequestParams::new(listed.uri.clone())),
        )
        .await;
        let text = match read {
            Ok(Ok(result)) => result
                .contents
                .into_iter()
                .find_map(|contents| match contents {
                    ResourceContents::TextResourceContents { uri, text, .. }
                        if uri == listed.uri =>
                    {
                        Some(text)
                    }
                    _ => None,
                }),
            Ok(Err(error)) => {
                tracing::warn!(server = %served.remote.server, uri = %listed.uri, %error, "MCP skill file could not be read");
                return Err(ToolOutput::error(format!(
                    "The skill file could not be read: {error}"
                )));
            }
            Err(_) => {
                return Err(ToolOutput::error(format!(
                    "The skill file did not arrive within {} seconds.",
                    self.limits.quiet.as_secs()
                )));
            }
        };
        let Some(text) = text else {
            return Err(ToolOutput::error(
                "The server sent the skill file as something other than text.",
            ));
        };
        let mismatch = listed
            .verify(text.as_bytes())
            .map_err(|error| error.to_string())
            .and_then(|()| {
                if listed.uri == served.entry.uri {
                    same_frontmatter(&text, &served.entry)
                } else {
                    Ok(())
                }
            });
        if let Err(why) = mismatch {
            tracing::warn!(server = %served.remote.server, uri = %listed.uri, "MCP skill file failed verification: {why}");
            self.stale.store(true, Ordering::Relaxed);
            return Err(ToolOutput::error(
                "The skill changed on its server since it was listed, so it was not loaded. Newt lists the skills again before the next turn.",
            ));
        }
        cache(&self.verified).insert(key, (listed.digest.clone(), text.clone()));
        Ok(text)
    }
}

pub(super) fn merge(bootstrap: &[Skill], served: &[Skill]) -> Result<Vec<Skill>, String> {
    let mut skills = bootstrap.to_vec();
    for skill in served {
        if bootstrap.iter().any(|own| own.name == skill.name) {
            return Err(format!(
                "the bootstrap and MCP server `{}` both offer a skill named `{}`",
                skill.served_by.as_deref().unwrap_or_default(),
                skill.name
            ));
        }
        skills.push(skill.clone());
    }
    Ok(skills)
}

fn cache(
    verified: &Verified,
) -> std::sync::MutexGuard<'_, HashMap<(String, String), (String, String)>> {
    match verified.lock() {
        Ok(cache) => cache,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn is_relative(file: &str) -> bool {
    !file.is_empty()
        && !file.starts_with('/')
        && !file.contains('\\')
        && file
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

fn file_list(served: &Served) -> Vec<String> {
    let prefix = format!("{}/", served.root);
    served
        .files
        .iter()
        .filter_map(|file| file.uri.strip_prefix(&prefix).map(str::to_owned))
        .collect()
}

/// The frontmatter of a fetched `SKILL.md` must say exactly what the listing
/// said, so the description the model was shown is the skill it gets.
fn same_frontmatter(text: &str, entry: &Entry) -> Result<(), String> {
    let yaml = text
        .strip_prefix("---")
        .and_then(|rest| rest.split_once("\n---"))
        .map(|(yaml, _)| yaml)
        .ok_or("SKILL.md has no frontmatter")?;
    let parsed: Value =
        serde_norway::from_str(yaml).map_err(|error| format!("frontmatter: {error}"))?;
    let listed = serde_json::to_value(&entry.frontmatter).map_err(|error| error.to_string())?;
    if parsed == listed {
        Ok(())
    } else {
        Err("its frontmatter differs from the listing".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{Skill, is_relative, merge};

    #[test]
    fn a_served_skill_never_shadows_a_bootstrap_skill() {
        let own = Skill::inline("app/writing", "Ours.", "Short sentences.");
        let served = Skill {
            served_by: Some("app".into()),
            ..Skill::inline("app/writing", "Theirs.", "")
        };
        assert_eq!(
            merge(std::slice::from_ref(&own), &[served]),
            Err("the bootstrap and MCP server `app` both offer a skill named `app/writing`".into())
        );
        let other = Skill {
            served_by: Some("app".into()),
            ..Skill::inline("app/refunds", "Refunds.", "")
        };
        assert_eq!(
            merge(std::slice::from_ref(&own), std::slice::from_ref(&other)),
            Ok(vec![own, other])
        );
    }

    #[test]
    fn only_plain_relative_paths_reach_a_skill_file() {
        assert!(is_relative("references/tone.md"));
        assert!(is_relative("SKILL.md"));
        for bad in [
            "",
            "/etc/passwd",
            "../other/SKILL.md",
            "a/../b",
            "./a",
            "a//b",
            "a\\b",
        ] {
            assert!(!is_relative(bad), "{bad}");
        }
    }
}
