use super::{Listed, PageKey, State, Template};

impl Template {
    /// The `{name}` parts of the template's path and what `path` puts in
    /// each, in order, when `path` is one of its pages. A part matches one
    /// path segment or a piece of one, never a `/`.
    pub fn captures(&self, path: &str) -> Option<Vec<(String, String)>> {
        let mut pattern = String::from("^");
        let mut names = Vec::new();
        let mut rest = self.path.as_str();
        while let Some(open) = rest.find('{') {
            let close = rest[open..].find('}')? + open;
            pattern.push_str(&regex::escape(&rest[..open]));
            pattern.push_str("([^/]+)");
            names.push(rest[open + 1..close].to_owned());
            rest = &rest[close + 1..];
        }
        pattern.push_str(&regex::escape(rest));
        pattern.push('$');
        let found = regex::Regex::new(&pattern).ok()?.captures(path)?;
        Some(
            names
                .into_iter()
                .zip(found.iter().skip(1))
                .map(|(name, value)| (name, value.map_or("", |value| value.as_str()).to_owned()))
                .collect(),
        )
    }

    /// The key of the template's page at `path`: the template's key, then
    /// the path.
    pub fn page_key(&self, path: &str) -> PageKey {
        let mut key = self.key.clone();
        key.push(path.to_owned());
        key
    }
}

impl State {
    /// Lists `path` when it is not listed and one of the templates serves
    /// it, so it can be read like any listed page.
    pub(super) fn list_from_template(&mut self, path: &str) {
        if self.knows(path) {
            return;
        }
        if let Some(template) = self
            .templates
            .iter()
            .find(|template| template.captures(path).is_some())
        {
            let key = template.page_key(path);
            self.index.insert(path.to_owned(), Listed::new(key, path));
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
            key: vec!["orders".to_owned()],
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
    fn a_page_key_is_the_templates_key_and_the_path() {
        assert_eq!(
            template("orders/{id}.md").page_key("orders/17.md"),
            vec!["orders".to_owned(), "orders/17.md".to_owned()]
        );
    }
}
