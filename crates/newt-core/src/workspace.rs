use std::collections::BTreeMap;

use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("invalid glob pattern: {0}")]
    Glob(#[from] globset::Error),
    #[error("invalid regular expression: {0}")]
    Regex(#[from] regex::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch<'a> {
    pub path: &'a str,
    pub line: usize,
    pub text: &'a str,
}

#[derive(Debug, Clone, Default)]
pub struct Workspace {
    pages: BTreeMap<String, String>,
}

impl Workspace {
    pub fn new(pages: BTreeMap<String, String>) -> Self {
        Self { pages }
    }

    pub fn read(&self, path: &str) -> Option<&str> {
        self.pages.get(path).map(String::as_str)
    }

    pub fn glob(&self, pattern: &str) -> Result<Vec<&str>, WorkspaceError> {
        let matcher = matcher(pattern)?;
        Ok(self
            .pages
            .keys()
            .filter(|path| matcher.is_match(path.as_str()))
            .map(String::as_str)
            .collect())
    }

    pub fn grep(
        &self,
        pattern: &str,
        within: Option<&str>,
        limit: usize,
    ) -> Result<Vec<GrepMatch<'_>>, WorkspaceError> {
        let regex = Regex::new(pattern)?;
        let within = within.map(matcher).transpose()?;
        Ok(self
            .pages
            .iter()
            .filter(|(path, _)| {
                within
                    .as_ref()
                    .is_none_or(|glob| glob.is_match(path.as_str()))
            })
            .flat_map(|(path, content)| {
                content
                    .lines()
                    .enumerate()
                    .filter(|(_, text)| regex.is_match(text))
                    .map(move |(index, text)| GrepMatch {
                        path,
                        line: index + 1,
                        text,
                    })
            })
            .take(limit)
            .collect())
    }
}

fn matcher(pattern: &str) -> Result<GlobMatcher, globset::Error> {
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()?
        .compile_matcher())
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

    use super::*;

    fn workspace() -> Workspace {
        Workspace::new(BTreeMap::from([
            (
                "profile.md".into(),
                "# Profile\nStaff engineer\nRemote".into(),
            ),
            ("roles/acme.md".into(), "# Acme\nRemote first".into()),
            ("roles/globex.md".into(), "# Globex\nOn-site".into()),
        ]))
    }

    #[test]
    fn glob_star_stays_within_one_directory() {
        let workspace = workspace();
        assert_eq!(workspace.glob("*.md").unwrap(), vec!["profile.md"]);
        assert_eq!(
            workspace.glob("**/*.md").unwrap(),
            vec!["profile.md", "roles/acme.md", "roles/globex.md"]
        );
        assert_eq!(
            workspace.glob("roles/*.md").unwrap(),
            vec!["roles/acme.md", "roles/globex.md"]
        );
    }

    #[test]
    fn grep_reports_one_based_lines_and_respects_the_glob_and_limit() {
        let workspace = workspace();
        let all = workspace.grep("Remote", None, 10).unwrap();
        assert_eq!(
            all,
            vec![
                GrepMatch {
                    path: "profile.md",
                    line: 3,
                    text: "Remote"
                },
                GrepMatch {
                    path: "roles/acme.md",
                    line: 2,
                    text: "Remote first"
                },
            ]
        );
        assert_eq!(
            workspace.grep("Remote", Some("roles/*"), 10).unwrap().len(),
            1
        );
        assert_eq!(workspace.grep("Remote", None, 1).unwrap().len(), 1);
    }

    #[test]
    fn bad_patterns_are_errors_not_panics() {
        let workspace = workspace();
        assert!(matches!(
            workspace.grep("(", None, 10),
            Err(WorkspaceError::Regex(_))
        ));
        assert!(matches!(workspace.glob("[",), Err(WorkspaceError::Glob(_))));
    }
}
