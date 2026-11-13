#![cfg(feature = "embedded-postgres")]
#![expect(clippy::unwrap_used, reason = "tests fail loudly on setup errors")]

use newt_server::config::Database;
use newt_server::db::connect_and_migrate;

#[tokio::test]
async fn newt_runs_its_own_postgres_and_finds_its_data_after_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let config = Database {
        url_env: None,
        embedded: Some(dir.path().join("pg")),
        max_connections: 2,
    };

    let first = connect_and_migrate(&config).await.unwrap();
    sqlx::query("create table probe (answer int)")
        .execute(&first.pool)
        .await
        .unwrap();
    sqlx::query("insert into probe values (42)")
        .execute(&first.pool)
        .await
        .unwrap();
    first.pool.close().await;
    std::mem::forget(first);

    let second = connect_and_migrate(&config).await.unwrap();
    let answer: i32 = sqlx::query_scalar("select answer from probe")
        .fetch_one(&second.pool)
        .await
        .unwrap();
    assert_eq!(answer, 42);
    let migrated: i64 = sqlx::query_scalar("select count(*) from _sqlx_migrations")
        .fetch_one(&second.pool)
        .await
        .unwrap();
    assert!(migrated > 0);
    second.pool.close().await;
    drop(second);
    assert!(!dir.path().join("pg/data/postmaster.pid").exists());
}
