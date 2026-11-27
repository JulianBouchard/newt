use super::{Listed, PageKey, State, Template};

/// What `text` puts in each `{part}` of `pattern`, in order, when it
/// matches. A part matches a run of characters with no `/` in it.
pub fn capture(pattern: &str, text: &str) -> Option<Vec<(String, String)>> {
    let mut regex = String::from("^");
    let mut names = Vec::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        let close = rest[open..].find('}')? + open;
        regex.push_str(&regex::escape(&rest[..open]));
        regex.push_str("([^/]+)");
        names.push(rest[open + 1..close].to_owned());
        rest = &rest[close + 1..];
    }
    regex.push_str(&regex::escape(rest));
    regex.push('$');
    let found = regex::Regex::new(&regex).ok()?.captures(text)?;
    Some(
        names
            .into_iter()
            .zip(found.iter().skip(1))
            .map(|(name, value)| (name, value.map_or("", |value| value.as_str()).to_owned()))
            .collect(),
    )
}

/// `pattern` with each `{part}` that has a value replaced by it.
pub fn fill(pattern: &str, values: &[(String, String)]) -> String {
    values
        .iter()
        .fold(pattern.to_owned(), |filled, (part, value)| {
            filled.replace(&format!("{{{part}}}"), value)
        })
}

impl Template {
    /// The `{name}` parts of the template's path and what `path` puts in
    /// each, in order, when `path` is one of its pages.
    pub fn captures(&self, path: &str) -> Option<Vec<(String, String)>> {
        capture(&self.path, path)
    }

    /// The key of the template's page at `path`, when it is one: the
    /// template's key with the path's parts filled in.
    pub fn page_key(&self, path: &str) -> Option<PageKey> {
        let values = self.captures(path)?;
        Some(self.key.iter().map(|part| fill(part, &values)).collect())
    }
}

impl State {
    /// Lists `path` when it is not listed and one of the templates serves
    /// it, so it can be read like any listed page. Whether it did.
    pub(super) fn list_from_template(&mut self, path: &str) -> bool {
        if self.knows(path) {
            return false;
        }
        let Some(key) = self
            .templates
            .iter()
            .find_map(|template| template.page_key(path))
        else {
            return false;
        };
        self.index.insert(path.to_owned(), Listed::new(key, path));
        true
    }

    /// Unlists a template page whose read failed, so a mistyped path does
    /// not stay in the index for `glob` and `grep` to trip on.
    pub(super) fn forget_unloaded(&mut self, path: &str) {
        if !self.pages.contains_key(path) {
            self.index.remove(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Template;

    fn template(path: &str) -> Template {
        Template {
            path: path.to_owned(),
            description: None,
            key: vec!["orders".to_owned(), "{id}".to_owned()],
        }
    }

    #[test]
    fn a_path_fills_each_part_with_one_segment_or_a_piece_of_one() {
        let orders = template("orders/{id}.md");
        assert_eq!(
            orders.captures("orders/17.md"),
            Some(vec![("id".to_owned(), "17".to_owned())])
        );
        assert_eq!(orders.captures("orders/a/17.md"), None);
        assert_eq!(orders.captures("orders/.md"), None);
        assert_eq!(orders.captures("roles/17.md"), None);
        let nested = template("{team}/roles/{role}");
        assert_eq!(
            nested.captures("acme/roles/backend.md"),
            Some(vec![
                ("team".to_owned(), "acme".to_owned()),
                ("role".to_owned(), "backend.md".to_owned()),
            ])
        );
        assert_eq!(
            template("a.b/{x}").captures("aXb/1"),
            None,
            "literals are literal"
        );
    }

    #[test]
    fn a_page_key_is_the_templates_key_with_the_paths_parts_filled_in() {
        assert_eq!(
            template("orders/{id}.md").page_key("orders/17.md"),
            Some(vec!["orders".to_owned(), "17".to_owned()])
        );
        assert_eq!(template("orders/{id}.md").page_key("roles/17.md"), None);
    }
}
