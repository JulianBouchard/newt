use proptest::prelude::*;
use toml::{Table, Value};

use super::ServerConfig;

const BASE: &str = r#"
server = { listen = "127.0.0.1:7878" }
database = { url_env = "DB", max_connections = 10 }
events = { jsonl = "events.jsonl", capture_content = false }
runner = { max_sessions = 8, cache_key_buckets = "auto" }
retention = { inbox_days = 30, live_days = 7 }

[routes.r]
protocol = "openai-responses"
model = "m"
base_url = "https://example.test/v1"
key_env = "K"
max_concurrent = 2
replay_reasoning = true

[models.default]
chain = ["r"]

[agents.demo]
mcp = "http://localhost:4000/mcp"
token_env = "T"
live_meta = false
live_stream = false
max_pending_events = 100

[tracing]
endpoint = "https://example.test/traces"
profile = "generic"
capture_content = true
service_name = "newt"
"#;

const KEYS: &[(&[&str], &str)] = &[
    (&["server"], "listen"),
    (&["database"], "url_env"),
    (&["database"], "embedded"),
    (&["database"], "max_connections"),
    (&["events"], "jsonl"),
    (&["events"], "capture_content"),
    (&["runner"], "max_sessions"),
    (&["runner"], "reserved_interactive"),
    (&["runner"], "max_waiting"),
    (&["runner"], "cache_key_buckets"),
    (&["retention"], "inbox_days"),
    (&["retention"], "live_days"),
    (&["routes", "r"], "protocol"),
    (&["routes", "r"], "model"),
    (&["routes", "r"], "base_url"),
    (&["routes", "r"], "key_env"),
    (&["routes", "r"], "api_key_header"),
    (&["routes", "r"], "headers_env"),
    (&["routes", "r"], "max_concurrent"),
    (&["routes", "r"], "replay_reasoning"),
    (&["models", "default"], "chain"),
    (&["agents", "demo"], "mcp"),
    (&["agents", "demo"], "token_env"),
    (&["agents", "demo"], "live_meta"),
    (&["agents", "demo"], "live_stream"),
    (&["agents", "demo"], "max_pending_events"),
    (&["tracing"], "endpoint"),
    (&["tracing"], "profile"),
    (&["tracing"], "headers_env"),
    (&["tracing"], "capture_content"),
    (&["tracing"], "service_name"),
];

fn toml_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        3 => any::<i64>().prop_map(Value::Integer),
        6 => proptest::sample::select(vec![0_i64, 1, 2, -1, 3, 7, 8, 9, 4_294_967_295, 4_294_967_296, i64::MAX, i64::MIN]).prop_map(Value::Integer),
        1 => any::<f64>().prop_map(Value::Float),
        2 => any::<bool>().prop_map(Value::Boolean),
        4 => ".{0,12}".prop_map(Value::String),
        4 => proptest::sample::select(vec![
            "", "auto", "AUTO", "openai-chat", "anthropic-messages", "openai-responses", "langfuse",
            "http://x", "https://x", "ftp://x", "stdio:", "stdio:cmd", "127.0.0.1:1", "[::1]:80", "not an address", "r", "missing",
        ])
        .prop_map(|text| Value::String(text.to_owned())),
    ];
    leaf.prop_recursive(2, 8, 3, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..3).prop_map(Value::Array),
            proptest::collection::btree_map("[a-z]{0,4}", inner, 0..3)
                .prop_map(|map| Value::Table(map.into_iter().collect())),
        ]
    })
}

fn mutated(index: usize, value: Option<Value>) -> (String, &'static str, &'static [&'static str]) {
    let (path, key) = KEYS[index];
    let mut root: Table = toml::from_str(BASE).expect("the base config is valid TOML");
    let mut table = &mut root;
    for step in path {
        table = table
            .get_mut(*step)
            .and_then(Value::as_table_mut)
            .expect("the base config has every table");
    }
    match value {
        Some(value) => {
            table.insert(key.to_owned(), value);
        }
        None => {
            table.remove(key);
        }
    }
    (
        toml::to_string(&root).expect("a table serialises"),
        key,
        path,
    )
}

#[test]
fn the_base_config_is_valid() {
    ServerConfig::parse(BASE).expect("the base config parses");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20000))]

    #[test]
    fn any_value_for_any_key_is_refused_with_the_key_named_or_leaves_a_usable_config(
        index in 0..KEYS.len(),
        value in proptest::option::weighted(0.85, toml_value()),
    ) {
        let (raw, key, path) = mutated(index, value);
        match ServerConfig::parse(&raw) {
            Err(error) => {
                let message = format!("{error:#}");
                let owner = path.last().copied().unwrap_or_default();
                prop_assert!(
                    message.contains("TOML parse error")
                        || message.contains(key)
                        || message.contains(owner)
                        || message.contains("`database`"),
                    "{key} in {path:?}: {message}\n{raw}"
                );
            }
            Ok(config) => {
                prop_assert!(config.runner.max_sessions >= 1, "{raw}");
                prop_assert!(config.runner.max_waiting() >= 1, "{raw}");
                prop_assert!(
                    config.runner.max_sessions == 1
                        || config.runner.reserved_interactive() < config.runner.max_sessions,
                    "{raw}"
                );
                prop_assert!(config.database.max_connections >= 1, "{raw}");
                for route in config.routes.values() {
                    prop_assert!(route.max_concurrent >= 1, "{raw}");
                    prop_assert!(route.base_url.starts_with("http"), "{raw}");
                }
                for chain in config.models.values() {
                    prop_assert!(!chain.chain.is_empty(), "{raw}");
                    prop_assert!(chain.chain.iter().all(|name| config.routes.contains_key(name)), "{raw}");
                }
                for agent in config.agents.values() {
                    prop_assert!(agent.max_pending_events >= 1, "{raw}");
                }
                prop_assert!(config.database.url_env.is_some() != config.database.embedded.is_some(), "{raw}");
            }
        }
    }
}
