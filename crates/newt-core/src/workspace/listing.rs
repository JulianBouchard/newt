use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Notify;

use super::{Listed, Listing, Template, Workspace, WorkspaceError};

/// A workspace's pages and collections as last listed, without contents.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Index {
    pub pages: Vec<Listed>,
    pub templates: Vec<Template>,
}

enum Step {
    Ready,
    Wait(Pin<Box<tokio::sync::futures::OwnedNotified>>, u64),
    List(Arc<Notify>, Attempt),
}

#[derive(Clone, Copy)]
struct Attempt {
    number: u64,
    clock: u64,
    epoch: u64,
}

struct ListingDone<'a> {
    workspace: &'a Workspace,
    notify: Arc<Notify>,
}

impl Drop for ListingDone<'_> {
    fn drop(&mut self) {
        let mut state = self.workspace.lock();
        if state
            .listing
            .as_ref()
            .is_some_and(|listing| Arc::ptr_eq(listing, &self.notify))
        {
            state.listing = None;
        }
        drop(state);
        self.notify.notify_waiters();
    }
}

impl Index {
    /// The index as the model reads it at the start of a turn, `None` when
    /// there is nothing to list. Top-level pages, pages with a priority and
    /// a folder's only page are listed by name, priority first; every other
    /// folder is one line with its page count and the glob that lists it.
    pub fn render(&self) -> Option<String> {
        if self.pages.is_empty() && self.templates.is_empty() {
            return None;
        }
        let mut folders: BTreeMap<&str, Vec<&Listed>> = BTreeMap::new();
        let mut named: Vec<&Listed> = Vec::new();
        for page in &self.pages {
            match page.path.split_once('/') {
                Some((folder, _)) if page.priority.is_none() => {
                    folders.entry(folder).or_default().push(page);
                }
                _ => named.push(page),
            }
        }
        let mut summaries = Vec::new();
        for (folder, pages) in folders {
            match pages.as_slice() {
                [only] => named.push(only),
                _ => summaries.push(format!(
                    "- {folder}/: {} pages, glob `{folder}/**` to list them",
                    pages.len()
                )),
            }
        }
        named.sort_by(|a, b| {
            b.priority
                .unwrap_or(f32::NEG_INFINITY)
                .total_cmp(&a.priority.unwrap_or(f32::NEG_INFINITY))
                .then_with(|| a.path.cmp(&b.path))
        });
        let mut lines: Vec<String> = named.into_iter().map(line).collect();
        lines.extend(summaries);
        lines.extend(self.templates.iter().map(|template| {
            match one_line(template.description.as_deref()) {
                Some(description) => format!("- {} — {description}", template.path),
                None => format!("- {}", template.path),
            }
        }));
        Some(format!(
            "<workspace>\nPages you can read with read_file:\n{}\n</workspace>",
            lines.join("\n")
        ))
    }
}

fn line(page: &Listed) -> String {
    let size = page
        .size
        .map(|size| format!(" ({})", human_size(size)))
        .unwrap_or_default();
    let description = one_line(page.description.as_deref())
        .map(|description| format!(" — {description}"))
        .unwrap_or_default();
    format!("- {}{size}{description}", page.path)
}

fn one_line(text: Option<&str>) -> Option<String> {
    let text = text?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a page size shown to one decimal"
)]
fn human_size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

impl Workspace {
    /// Asks for a fresh listing before the index is next used.
    pub fn relist(&self) {
        if self.inner.source.is_some() {
            let mut state = self.lock();
            state.list_wanted = true;
            state.list_epoch += 1;
        }
    }

    /// The pages and collections as last listed, listing first when a
    /// fresh listing was asked for.
    pub async fn index(&self) -> Result<Index, WorkspaceError> {
        self.listed().await?;
        let state = self.lock();
        Ok(Index {
            pages: state.index.values().cloned().collect(),
            templates: state.templates.clone(),
        })
    }

    pub(super) async fn listed(&self) -> Result<(), WorkspaceError> {
        loop {
            let step = {
                let mut state = self.lock();
                if !state.list_wanted {
                    Step::Ready
                } else if let Some(listing) = &state.listing {
                    let mut waiting = Box::pin(Arc::clone(listing).notified_owned());
                    waiting.as_mut().enable();
                    Step::Wait(waiting, state.list_attempt)
                } else {
                    let notify = Arc::new(Notify::new());
                    state.listing = Some(Arc::clone(&notify));
                    state.list_attempt += 1;
                    Step::List(
                        notify,
                        Attempt {
                            number: state.list_attempt,
                            clock: state.clock,
                            epoch: state.list_epoch,
                        },
                    )
                }
            };
            match step {
                Step::Ready => return Ok(()),
                Step::Wait(waiting, attempt) => {
                    waiting.await;
                    if let Some((failed, error)) = &self.lock().list_failure
                        && *failed == attempt
                    {
                        return Err(WorkspaceError::Refresh(error.clone()));
                    }
                }
                Step::List(notify, attempt) => {
                    let _done = ListingDone {
                        workspace: self,
                        notify,
                    };
                    let listed = match &self.inner.source {
                        Some(source) => source.list().await,
                        None => Err("this workspace has no page source".to_owned()),
                    };
                    return match listed {
                        Ok(listing) => {
                            self.apply_listing(listing, attempt);
                            Ok(())
                        }
                        Err(error) => {
                            self.lock().list_failure = Some((attempt.number, error.clone()));
                            Err(WorkspaceError::Refresh(error))
                        }
                    };
                }
            }
        }
    }

    fn apply_listing(&self, listing: Listing, attempt: Attempt) {
        let started = attempt.clock;
        let mut state = self.lock();
        let mut index: BTreeMap<String, Listed> = listing
            .pages
            .into_iter()
            .map(|listed| (listed.path.clone(), listed))
            .collect();
        let gone: Vec<String> = state
            .pages
            .keys()
            .filter(|path| !index.contains_key(*path) && state.generation_of(path) <= started)
            .cloned()
            .collect();
        for path in &gone {
            state.pages.remove(path);
        }
        let unlisted: Vec<String> = state
            .stale
            .iter()
            .filter(|path| !index.contains_key(*path) && !state.pages.contains_key(*path))
            .cloned()
            .collect();
        for path in &unlisted {
            state.stale.remove(path);
        }
        for (path, listed) in &state.index {
            if !index.contains_key(path) && state.generation_of(path) > started {
                index.insert(path.clone(), listed.clone());
            }
        }
        state.index = index;
        state.templates = listing.templates;
        state.list_failure = None;
        if state.list_epoch == attempt.epoch {
            state.list_wanted = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(path: &str) -> Listed {
        Listed::new(vec![path.to_owned()], path)
    }

    fn index(pages: Vec<Listed>) -> Index {
        Index {
            pages,
            templates: Vec::new(),
        }
    }

    #[test]
    fn nothing_listed_renders_nothing() {
        assert_eq!(index(Vec::new()).render(), None);
    }

    #[test]
    fn top_level_and_prioritised_pages_are_named_highest_priority_first_and_folders_summed_up() {
        let mut urgent = page("roles/acme.md");
        urgent.priority = Some(1.0);
        let mut notes = page("notes.md");
        notes.priority = Some(0.4);
        let mut pages = vec![page("profile.md"), notes, urgent, page("archive/2024.md")];
        pages.extend((0..3).map(|n| page(&format!("roles/r{n}.md"))));
        assert_eq!(
            index(pages).render().as_deref(),
            Some(
                "<workspace>\nPages you can read with read_file:\n\
                 - roles/acme.md\n\
                 - notes.md\n\
                 - archive/2024.md\n\
                 - profile.md\n\
                 - roles/: 3 pages, glob `roles/**` to list them\n\
                 </workspace>"
            )
        );
    }

    #[test]
    fn a_line_carries_the_size_and_a_one_line_description() {
        let mut page = page("profile.md");
        page.size = Some(2_150);
        page.description = Some("What the person\n  wants\tnext ".to_owned());
        assert_eq!(
            line(&page),
            "- profile.md (2.1 KB) — What the person wants next"
        );
        assert_eq!(human_size(12), "12 B");
        assert_eq!(human_size(5 * 1_048_576), "5.0 MB");
    }
}
