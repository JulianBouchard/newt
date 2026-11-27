//! MCP resource templates as workspace collections: a template's name is
//! a path pattern such as `orders/{id}.md`, and reading a path it matches
//! fills its URI template with the same values.

use newt_core::workspace::{Listed, PageKey, Template, capture, fill};
use rmcp::model::ResourceTemplate;

use super::pages::META_KEY;
use super::{Owner, meta_field};

/// A template a server serves: its workspace collection and the URI
/// template its pages are read by.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Served {
    pub(super) template: Template,
    uri: String,
}

/// The `{part}` names in `text`, sorted; `None` when one is not a simple
/// name, as an RFC 6570 operator such as `{?q}` is not.
fn parts(text: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let close = rest[open..].find('}')? + open;
        let part = &rest[open + 1..close];
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }
        parts.push(part.to_owned());
        rest = &rest[close + 1..];
    }
    parts.sort();
    parts.dedup();
    Some(parts)
}

impl Served {
    /// A server's template, when newt can serve it: a name with at least
    /// one `{part}`, and a URI template and page key with exactly the same
    /// simple parts. Its pages are keyed by `_meta["newt/key"]` with the
    /// parts filled in, or by the filled URI, as listed resources are.
    pub(super) fn new(listed: ResourceTemplate, owner: &Owner, server: &str) -> Option<Self> {
        let named = parts(&listed.name).filter(|named| !named.is_empty());
        let key = match meta_field::<PageKey>(listed.meta.as_deref(), META_KEY) {
            Ok(Some(key)) => key,
            Ok(None) => vec![listed.uri_template.clone()],
            Err(error) => {
                tracing::warn!(server, name = %listed.name, %error, "MCP resource template left out: its page key is not a list of strings");
                return None;
            }
        };
        let keyed = parts(&key.concat());
        if named.is_none() || named != parts(&listed.uri_template) || named != keyed {
            tracing::warn!(server, name = %listed.name, uri = %listed.uri_template, "MCP resource template left out: its name, URI template and page key need the same simple {{part}}s");
            return None;
        }
        Some(Self {
            template: Template {
                path: owner.path(&listed.name),
                description: listed.description,
                key: owner.key(key),
            },
            uri: listed.uri_template,
        })
    }

    /// The URI and path of the template page `key` names, when it is one of
    /// this template's pages: the key read back into its parts, and the
    /// URI template filled with them, percent-encoded.
    pub(super) fn page_for(&self, key: &PageKey) -> Option<(String, String)> {
        if key.len() != self.template.key.len() {
            return None;
        }
        let mut values = Vec::new();
        for (pattern, element) in self.template.key.iter().zip(key) {
            if pattern.contains('{') {
                values.extend(capture(pattern, element)?);
            } else if pattern != element {
                return None;
            }
        }
        let encoded_values: Vec<(String, String)> = values
            .iter()
            .map(|(part, value)| (part.clone(), encoded(value)))
            .collect();
        Some((
            fill(&self.uri, &encoded_values),
            fill(&self.template.path, &values),
        ))
    }
}

/// Whether a home template could serve paths inside an added server's
/// folder: its first segment is that folder or a `{part}`. Such a template
/// is left out, as a home resource named inside one fails the listing.
pub(super) fn inside_a_folder(name: &str, folders: &std::collections::HashSet<String>) -> bool {
    let first = name.split('/').next().unwrap_or_default();
    let inside = name.contains('/') && (first.contains('{') || folders.contains(first));
    if inside {
        tracing::warn!(
            name,
            "home resource template left out: it would serve paths inside an added server's folder"
        );
    }
    inside
}

/// The keys that name template pages, as listed pages with their URIs,
/// and the other keys.
pub(super) fn split(served: &[Served], keys: &[PageKey]) -> (Vec<(Listed, String)>, Vec<PageKey>) {
    let mut templated = Vec::new();
    let mut listed = Vec::new();
    for key in keys {
        match served.iter().find_map(|served| served.page_for(key)) {
            Some((uri, path)) => templated.push((Listed::new(key.clone(), path), uri)),
            None => listed.push(key.clone()),
        }
    }
    (templated, listed)
}

/// `value` with every byte outside RFC 3986's unreserved set percent-encoded,
/// as a simple URI template expression expands it.
fn encoded(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use rmcp::model::{MetaObject, ResourceTemplate};
    use serde_json::json;

    use super::{Owner, Served};

    fn served(name: &str, uri: &str, key: Option<serde_json::Value>) -> Option<Served> {
        let mut template = ResourceTemplate::new(uri, name);
        if let Some(key) = key {
            let mut meta = MetaObject::new();
            meta.insert("newt/key".to_owned(), key);
            template.meta = Some(meta);
        }
        Served::new(template, &Owner::added("crm"), "crm")
    }

    #[test]
    #[expect(clippy::unwrap_used, reason = "the test fails loudly on setup")]
    fn a_template_page_is_keyed_by_its_newt_key_and_read_by_its_filled_uri() {
        let issues = served(
            "issues/{key}.md",
            "crm://issues/{key}",
            Some(json!(["issues", "{key}"])),
        )
        .unwrap();
        assert_eq!(issues.template.path, "crm/issues/{key}.md");
        let key = issues.template.page_key("crm/issues/WEB 12.md").unwrap();
        assert_eq!(
            key,
            Owner::added("crm").key(vec!["issues".to_owned(), "WEB 12".to_owned()])
        );
        assert_eq!(
            issues.page_for(&key),
            Some((
                "crm://issues/WEB%2012".to_owned(),
                "crm/issues/WEB 12.md".to_owned()
            ))
        );
        assert_eq!(
            issues.page_for(&Owner::added("crm").key(vec!["roles".to_owned(), "1".to_owned()])),
            None
        );
    }

    #[test]
    #[expect(clippy::unwrap_used, reason = "the test fails loudly on setup")]
    fn without_a_newt_key_a_template_page_is_keyed_by_its_uri_as_a_listed_page_is() {
        let orders = served("orders/{id}.md", "crm://orders/{id}", None).unwrap();
        let key = orders.template.page_key("crm/orders/17.md").unwrap();
        assert_eq!(
            key,
            Owner::added("crm").key(vec!["crm://orders/17".to_owned()])
        );
        assert_eq!(
            orders.page_for(&key),
            Some(("crm://orders/17".to_owned(), "crm/orders/17.md".to_owned()))
        );
    }

    #[test]
    fn a_home_template_may_not_reach_into_an_added_servers_folder() {
        let folders = std::collections::HashSet::from(["crm".to_owned()]);
        assert!(super::inside_a_folder("crm/orders/{id}.md", &folders));
        assert!(super::inside_a_folder("{team}/orders/{id}.md", &folders));
        assert!(!super::inside_a_folder("orders/{id}.md", &folders));
        assert!(!super::inside_a_folder("{id}.md", &folders));
    }

    #[test]
    fn templates_newt_cannot_fill_or_key_are_left_out() {
        assert!(
            served("orders.md", "crm://orders", None).is_none(),
            "no parts"
        );
        assert!(
            served("orders/{id}.md", "crm://orders/{order}", None).is_none(),
            "other parts"
        );
        assert!(
            served("search/{q}.md", "crm://search{?q}", None).is_none(),
            "an operator"
        );
        assert!(
            served("{a}/{b}.md", "crm://{b}/{a}", None).is_some(),
            "order may differ"
        );
        assert!(
            served(
                "issues/{key}.md",
                "crm://issues/{key}",
                Some(json!(["issues"]))
            )
            .is_none(),
            "a key without the parts would give every page one key"
        );
        assert!(
            served(
                "issues/{key}.md",
                "crm://issues/{key}",
                Some(json!("issues"))
            )
            .is_none(),
            "a key that is not a list"
        );
    }
}
