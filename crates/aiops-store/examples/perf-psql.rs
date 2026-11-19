//! A `psql -qtA` stand-in for the perf runners on machines without a client (a
//! sandbox where the stack's PostgreSQL answers on TCP but docker is out of reach):
//! `perf-psql <postgres-url> <sql>` runs one statement and prints each row's fields
//! tab-separated — bare values, matching `-qtA`. Dev tooling only; CI uses real psql.
//!
//! The runners ask it for DDL (`CREATE DATABASE`, `DROP DATABASE`) and the post-run
//! counts (`SELECT count(*) …`), so it does not need transactions, multiple
//! statements or a smarter type map than the scalar kinds the counts return.

use sqlx::{PgPool, Row};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(url), Some(sql)) = (args.next(), args.next()) else {
        eprintln!("usage: perf-psql <postgres-url> <one sql statement>");
        std::process::exit(2);
    };
    match run(&url, &sql).await {
        Ok(()) => {}
        Err(e) => {
            eprintln!("perf-psql: {e}");
            std::process::exit(1);
        }
    }
}

async fn run(url: &str, sql: &str) -> Result<(), sqlx::Error> {
    let pool = PgPool::connect(url).await?;
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_all(&pool)
        .await?;
    for row in &rows {
        let mut fields = Vec::with_capacity(row.len());
        for i in 0..row.len() {
            let field = row
                .try_get::<String, usize>(i)
                .or_else(|_| row.try_get::<i64, usize>(i).map(|v| v.to_string()))
                .or_else(|_| row.try_get::<i32, usize>(i).map(|v| v.to_string()))
                .or_else(|_| row.try_get::<f64, usize>(i).map(|v| v.to_string()))
                .or_else(|_| row.try_get::<bool, usize>(i).map(|v| v.to_string()))
                .unwrap_or_default();
            fields.push(field);
        }
        println!("{}", fields.join("\t"));
    }
    Ok(())
}
