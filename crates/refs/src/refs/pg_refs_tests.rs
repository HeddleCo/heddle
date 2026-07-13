use std::{sync::Arc, time::Duration};

use objects::object::{MarkerName, StateId, ThreadName};
use sqlx::{AssertSqlSafe, Executor, PgPool, postgres::PgPoolOptions};

use super::*;

const REF_SCHEMA: &str = include_str!("fixtures/pg_state_refs.sql");

fn test_database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://heddle-test@127.0.0.1:1/heddle_test".to_string())
}

fn quoted_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

async fn create_fixture(database_url: &str) -> (PgPool, String) {
    let schema = format!("refs_state_test_{}", Uuid::new_v4().simple());
    let quoted_schema = quoted_ident(&schema);
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(2))
        .connect(database_url)
        .await
        .expect("connect to postgres for refs fixture");
    sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {quoted_schema}")))
        .execute(&admin)
        .await
        .expect("create refs fixture schema");

    let search_path_sql = format!("SET search_path TO {quoted_schema}");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .after_connect(move |conn, _meta| {
            let search_path_sql = search_path_sql.clone();
            Box::pin(async move {
                conn.execute(AssertSqlSafe(search_path_sql)).await?;
                Ok(())
            })
        })
        .connect(database_url)
        .await
        .expect("connect to refs fixture schema");

    for statement in REF_SCHEMA
        .split(';')
        .map(str::trim)
        .filter(|sql| !sql.is_empty())
    {
        sqlx::query(AssertSqlSafe(statement.to_string()))
            .execute(&pool)
            .await
            .expect("apply refs fixture schema");
    }
    drop(admin);
    (pool, schema)
}

async fn drop_fixture(database_url: &str, schema: &str) {
    let quoted_schema = quoted_ident(schema);
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(2))
        .connect(database_url)
        .await
        .expect("connect to postgres for refs fixture cleanup");
    sqlx::query(AssertSqlSafe(format!(
        "DROP SCHEMA {quoted_schema} CASCADE"
    )))
    .execute(&admin)
    .await
    .expect("drop refs fixture schema");
}

fn full_width_state(seed: u8) -> StateId {
    let mut bytes = [0; 32];
    for (offset, byte) in bytes.iter_mut().enumerate() {
        *byte = seed.wrapping_add(offset as u8);
    }
    StateId::from_bytes(bytes)
}

#[test]
fn state_id_codec_requires_32_bytes() {
    let state = full_width_state(1);
    let encoded = PgRefBackend::id_to_bytes(&state);
    assert_eq!(encoded.len(), 32);
    assert_eq!(PgRefBackend::bytes_to_id(encoded).unwrap(), state);

    let error = PgRefBackend::bytes_to_id(vec![0; 16]).unwrap_err();
    assert!(matches!(error, HeddleError::InvalidObject(_)));
    assert!(error.to_string().contains("expected 32 bytes, found 16"));
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "postgres integration: needs DATABASE_URL (CI postgres-tests job)"]
async fn pg_refs_methods_do_not_panic_on_current_thread_runtime() {
    let pool = PgPoolOptions::new()
        .acquire_timeout(Duration::from_millis(200))
        .connect_lazy(&test_database_url())
        .expect("connect_lazy accepts the URL");
    let backend = PgRefBackend::new(Arc::new(pool), Uuid::new_v4());
    let _ = backend.read_head();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "postgres integration: needs DATABASE_URL (CI postgres-tests job)"]
async fn state_ids_round_trip_at_32_bytes() {
    let database_url = test_database_url();
    let (pool, schema) = create_fixture(&database_url).await;
    let backend = PgRefBackend::new(Arc::new(pool.clone()), Uuid::new_v4());
    let thread = ThreadName::new("main");
    let marker = MarkerName::new("release");
    let thread_state = full_width_state(1);
    let marker_state = full_width_state(64);
    let detached_state = full_width_state(128);

    backend.set_thread(&thread, &thread_state).unwrap();
    backend.create_marker(&marker, &marker_state).await.unwrap();
    backend
        .write_head(&Head::Detached {
            state: detached_state,
        })
        .unwrap();

    assert_eq!(
        backend.get_thread(&thread).await.unwrap(),
        Some(thread_state)
    );
    assert_eq!(
        backend.get_marker(&marker).await.unwrap(),
        Some(marker_state)
    );
    assert_eq!(
        backend.read_head().unwrap(),
        Head::Detached {
            state: detached_state
        }
    );
    let widths = sqlx::query_scalar::<_, i32>(
        "SELECT octet_length(state_id) FROM refs WHERE repo_id = $1 ORDER BY name",
    )
    .bind(backend.repo_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(widths, vec![32, 32]);
    let head_width =
        sqlx::query_scalar::<_, i32>("SELECT octet_length(state_id) FROM heads WHERE repo_id = $1")
            .bind(backend.repo_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(head_width, 32);

    drop(backend);
    pool.close().await;
    drop_fixture(&database_url, &schema).await;
}
