use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use proptest::prelude::*;
use serde_json::{Value, json};

use super::*;

const SECRET: &str = "classified-contents";
const BOUND: Duration = Duration::from_secs(5);
const PAGES: [&str; 3] = ["profile.md", "roles/acme.md", "long.md"];

fn workspace() -> Workspace {
    Workspace::new(BTreeMap::from([
        ("profile.md".to_owned(), "Remote only\nBerlin".to_owned()),
        ("roles/acme.md".to_owned(), SECRET.to_owned()),
        ("long.md".to_owned(), "a".repeat(200_000)),
    ]))
}

fn run(name: &str, arguments: Value) -> (ToolOutput, Duration) {
    let call = ToolCall {
        id: "c1".to_owned(),
        name: name.to_owned(),
        arguments,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let started = Instant::now();
    let output = runtime.block_on(run_workspace_tool(&call, &workspace()));
    (output, started.elapsed())
}

fn path_like() -> impl Strategy<Value = String> {
    prop_oneof![
        "(\\.\\./|\\./|/|\\\\|roles/|profile|\\.md|\0|~|%2e%2e|é|\u{202e}){0,8}",
        "[a-z./]{0,20}",
        Just("a".repeat(1_000_000)),
        Just("../".repeat(50_000)),
    ]
}

fn pattern_like() -> impl Strategy<Value = String> {
    prop_oneof![
        any::<String>(),
        "([a-z.*+?|(){}\\[\\]\\\\^$!,/-]|\\\\p\\{L\\}|\\(\\?i\\)){0,16}",
        path_like(),
    ]
}

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|number| Value::Number(number.into())),
        any::<String>().prop_map(Value::String),
        path_like().prop_map(Value::String),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map("(paths|pattern|glob|[a-z]{1,4})", inner, 0..4)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn any_arguments_finish_in_bounded_time(
        name in prop::sample::select(vec!["read_file", "glob", "grep"]),
        arguments in json_value(),
    ) {
        let (_, elapsed) = run(name, arguments);
        prop_assert!(elapsed < BOUND, "{elapsed:?}");
    }

    #[test]
    fn read_file_returns_only_pages_that_exist(paths in prop::collection::vec(path_like(), 1..4)) {
        let (output, elapsed) = run("read_file", json!({ "paths": paths }));
        prop_assert!(elapsed < BOUND);
        let reachable = paths.iter().any(|path| path == "roles/acme.md");
        prop_assert_eq!(output.content.contains(SECRET), reachable);
        prop_assert!(!output.is_error);
    }

    #[test]
    fn glob_lists_only_workspace_paths(pattern in pattern_like()) {
        let (output, elapsed) = run("glob", json!({ "pattern": pattern }));
        prop_assert!(elapsed < BOUND, "{elapsed:?}");
        if !output.is_error {
            for line in output.content.lines() {
                prop_assert!(
                    PAGES.contains(&line) || line == "(no matching pages)",
                    "unexpected line {line:?}"
                );
            }
        }
    }

    #[test]
    fn grep_finds_only_workspace_lines(pattern in pattern_like(), within in prop::option::of(pattern_like())) {
        let mut arguments = json!({ "pattern": pattern });
        if let Some(within) = within {
            arguments["glob"] = json!(within);
        }
        let (output, elapsed) = run("grep", arguments);
        prop_assert!(elapsed < BOUND, "{elapsed:?}");
        if !output.is_error {
            for line in output.content.lines().filter(|line| !line.starts_with('(')) {
                let path = line.split(':').next().unwrap_or_default();
                prop_assert!(PAGES.contains(&path), "{line:?}");
            }
        }
    }
}

#[test]
fn pathological_regexes_and_globs_finish_in_bounded_time() {
    let patterns = [
        "(a*)*b".to_owned(),
        "(a|aa)+$".to_owned(),
        "(a+)+(a+)+(a+)+b".to_owned(),
        "a{1000}{1000}".to_owned(),
        "(?:a{100}){100}".to_owned(),
        "\\p{L}{100}".to_owned(),
        "(?i)\\w{200}".to_owned(),
        ".*.*.*.*.*.*.*.*x".to_owned(),
        "(".repeat(100_000),
        format!("{}a{}", "(".repeat(10_000), ")".repeat(10_000)),
        "[a-z]".repeat(100_000),
        "\\pL{50}\\pN{50}\\w{50}".to_owned(),
        "(?s).{0,2000}".to_owned(),
    ];
    for pattern in &patterns {
        for tool in ["grep", "glob"] {
            let (_, elapsed) = run(tool, json!({ "pattern": pattern, "glob": pattern }));
            assert!(
                elapsed < BOUND,
                "{tool} {:?} took {elapsed:?}",
                &pattern[..pattern.len().min(30)]
            );
        }
    }
    let globs = [
        format!("{}b", "*a".repeat(5_000)),
        "{a,b}".repeat(2_000),
        "**/".repeat(10_000),
        "[a-z]".repeat(50_000),
    ];
    for pattern in &globs {
        let (_, elapsed) = run("glob", json!({ "pattern": pattern }));
        assert!(elapsed < BOUND, "glob took {elapsed:?}");
        let (_, elapsed) = run("grep", json!({ "pattern": "a", "glob": pattern }));
        assert!(elapsed < BOUND, "grep glob took {elapsed:?}");
    }
}

#[test]
fn deeply_nested_braces_in_a_glob_do_not_overflow_the_stack() {
    let pattern = format!("{}{}", "{".repeat(5_000), "}".repeat(5_000));
    let (output, _) = run("glob", json!({ "pattern": pattern }));
    assert!(output.is_error);
}

#[test]
fn an_oversized_glob_is_an_error_result_not_a_panic() {
    let pattern = "a".repeat(1_000_000);
    let (output, _) = run("glob", json!({ "pattern": pattern }));
    assert!(output.is_error);
    let (output, _) = run("grep", json!({ "pattern": "a", "glob": pattern }));
    assert!(output.is_error);
}
