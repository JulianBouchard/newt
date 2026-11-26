use newt_core::workspace::PageKey;
use rmcp::model::{CallToolResult, ContentBlock};

use super::{Owner, meta_field};

/// The page keys of the resources a result links to, read the way a
/// listing reads them: `_meta["newt/key"]`, or the URI.
pub(super) fn linked_pages(result: &CallToolResult, owner: &Owner) -> Vec<PageKey> {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ResourceLink(link) => {
                match meta_field::<PageKey>(link.meta.as_deref(), super::pages::META_KEY) {
                    Ok(key) => Some(owner.key(key.unwrap_or_else(|| vec![link.uri.clone()]))),
                    Err(error) => {
                        tracing::debug!(uri = %link.uri, %error, "a linked resource has an invalid page key; not reading it ahead");
                        None
                    }
                }
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use rmcp::model::{CallToolResult, ContentBlock, MetaObject, Resource};
    use serde_json::json;

    use super::{Owner, linked_pages};

    #[test]
    fn a_linked_resource_is_keyed_like_a_listed_one_and_scoped_to_its_server() {
        let plain = Resource::new("app://ana/tasks.md", "tasks.md");
        let mut keyed = Resource::new("app://ana/notes", "notes.md");
        let mut meta = MetaObject::new();
        meta.insert("newt/key".to_owned(), json!(["notes", "ana"]));
        keyed.meta = Some(meta);
        let result = CallToolResult::success(vec![
            ContentBlock::text("Two pages."),
            ContentBlock::resource_link(plain),
            ContentBlock::resource_link(keyed),
        ]);
        let crm = Owner::added("crm");
        assert_eq!(
            linked_pages(&result, &crm),
            vec![
                crm.key(vec!["app://ana/tasks.md".to_owned()]),
                crm.key(vec!["notes".to_owned(), "ana".to_owned()]),
            ]
        );
    }
}
