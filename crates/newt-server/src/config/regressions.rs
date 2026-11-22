use super::ServerConfig;

#[test]
fn a_database_pool_of_zero_connections_is_refused_with_the_key_named() {
    let raw = r#"
        server = { listen = "127.0.0.1:0" }
        database = { url_env = "X", max_connections = 0 }
    "#;

    let error = ServerConfig::parse(raw)
        .expect_err("a pool of zero connections cannot serve anything")
        .to_string();

    assert!(error.contains("max_connections"), "{error}");
}
