use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;

use super::{Workspace, WorkspaceError};

const GLOB_LIMIT: usize = 512;

/// A line of a page that a grep pattern matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

impl Workspace {
    /// The listed paths that match `pattern`, without reading a page.
    pub async fn glob(&self, pattern: &str) -> Result<Vec<String>, WorkspaceError> {
        let matcher = matcher(pattern)?;
        self.listed().await?;
        Ok(self
            .lock()
            .index
            .keys()
            .filter(|path| matcher.is_match(path.as_str()))
            .cloned()
            .collect())
    }

    /// Searches the pages `within` matches, every listed page when it is
    /// `None`, fetching the ones not yet loaded or stale first.
    pub async fn grep(
        &self,
        pattern: &str,
        within: Option<&str>,
        limit: usize,
    ) -> Result<Vec<GrepMatch>, WorkspaceError> {
        let regex =
            Regex::new(pattern).map_err(|error| WorkspaceError::Regex(error.to_string()))?;
        let within = within.map(matcher).transpose()?;
        self.listed().await?;
        let candidates: Vec<String> = self
            .lock()
            .index
            .keys()
            .filter(|path| {
                within
                    .as_ref()
                    .is_none_or(|glob| glob.is_match(path.as_str()))
            })
            .cloned()
            .collect();
        self.refresh(candidates.clone()).await?;
        let state = self.lock();
        Ok(candidates
            .iter()
            .filter_map(|path| state.pages.get(path))
            .flat_map(|page| {
                page.content
                    .lines()
                    .enumerate()
                    .filter(|(_, text)| regex.is_match(text))
                    .map(|(index, text)| GrepMatch {
                        path: page.path.clone(),
                        line: index + 1,
                        text: text.to_owned(),
                    })
                    .collect::<Vec<_>>()
            })
            .take(limit)
            .collect())
    }
}

fn matcher(pattern: &str) -> Result<GlobMatcher, WorkspaceError> {
    if pattern.len() > GLOB_LIMIT {
        return Err(WorkspaceError::Glob("longer than 512 bytes".to_owned()));
    }
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|error| WorkspaceError::Glob(error.to_string()))?
        .compile_matcher())
}
