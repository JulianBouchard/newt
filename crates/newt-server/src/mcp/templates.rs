//! MCP resource templates as workspace collections: a template's name is
//! a path pattern such as `orders/{id}.md`, and reading a path it matches
//! fills its URI template with the same values.

use newt_core::workspace::{Listed, PageKey, Template};
use rmcp::model::ResourceTemplate;

use super::Owner;

/// The first part of every template's key, after the server's own tag. It
/// cannot start with the NUL that marks an added server's keys.
const TEMPLATE_KEY: &str = "newt/template";

/// A template a server serves: its workspace collection and the URI
/// template its pages are read by.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Served {
    pub(super) template: Template,
    uri: String,
}

impl Served {
    /// A server's template, when newt can serve it: a name with at least
    /// one `{part}`, a URI template that uses only simple `{part}`
    /// expressions, and the same parts in both.
    pub(super) fn new(listed: ResourceTemplate, owner: &Owner, server: &str) -> Option<Self> {
        let parts = |text: &str| -> Option<Vec<String>> {
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
            Some(parts)
        };
        let named = parts(&listed.name);
        if named.as_ref().is_none_or(Vec::is_empty) || named != parts(&listed.uri_template) {
            tracing::warn!(server, name = %listed.name, uri = %listed.uri_template, "MCP resource template left out: its name and URI template need the same simple {{part}}s");
            return None;
        }
        Some(Self {
            template: Template {
                path: owner.path(&listed.name),
                description: listed.description,
                key: owner.key(vec![TEMPLATE_KEY.to_owned(), listed.uri_template.clone()]),
            },
            uri: listed.uri_template,
        })
    }

    /// The URI of the template page `key` names, when it is one of this
    /// template's pages.
    pub(super) fn uri_for(&self, key: &PageKey) -> Option<String> {
        let (path, prefix) = key.split_last()?;
        if prefix != self.template.key.as_slice() {
            return None;
        }
        let mut uri = self.uri.clone();
        for (part, value) in self.template.captures(path)? {
            uri = uri.replace(&format!("{{{part}}}"), &encoded(&value));
        }
        Some(uri)
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
        match (
            served.iter().find_map(|served| served.uri_for(key)),
            key.last(),
        ) {
            (Some(uri), Some(path)) => {
                templated.push((Listed::new(key.clone(), path.clone()), uri));
            }
            _ => listed.push(key.clone()),
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
    use rmcp::model::ResourceTemplate;

    use super::{Owner, Served};

    fn served(name: &str, uri: &str) -> Option<Served> {
        Served::new(
            ResourceTemplate::new(uri, name),
            &Owner::added("crm"),
            "crm",
        )
    }

    #[test]
    #[expect(clippy::unwrap_used, reason = "the test fails loudly on setup")]
    fn a_page_of_a_template_is_read_by_its_filled_uri() {
        let orders = served("orders/{id}.md", "crm://orders/{id}").unwrap();
        assert_eq!(orders.template.path, "crm/orders/{id}.md");
        let key = orders.template.page_key("crm/orders/a b.md");
        assert_eq!(orders.uri_for(&key).as_deref(), Some("crm://orders/a%20b"));
        assert_eq!(
            orders.uri_for(&orders.template.page_key("crm/roles/1.md")),
            None
        );
        assert_eq!(orders.uri_for(&vec!["crm/orders/1.md".to_owned()]), None);
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
    fn templates_newt_cannot_fill_are_left_out() {
        assert!(served("orders.md", "crm://orders").is_none(), "no parts");
        assert!(
            served("orders/{id}.md", "crm://orders/{order}").is_none(),
            "other parts"
        );
        assert!(
            served("search/{q}.md", "crm://search{?q}").is_none(),
            "an operator"
        );
        assert!(
            served("{a}/{b}.md", "crm://{b}/{a}").is_some(),
            "order may differ"
        );
    }
}
