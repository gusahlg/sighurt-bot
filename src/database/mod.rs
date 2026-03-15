pub mod models;

use anyhow::Result;
use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};
use std::path::Path;

pub async fn init_database(database_url: &str) -> Result<SqlitePool> {
    // Extract path from sqlite: or sqlite:// URL
    let db_path = database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"))
        .unwrap_or(database_url);

    // Create parent directory if needed
    if let Some(parent) = Path::new(db_path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Create database file if it doesn't exist
    if !Path::new(db_path).exists() {
        std::fs::File::create(db_path)?;
    }

    let pool = SqlitePoolOptions::new()
        .max_connections(3)
        .connect(database_url)
        .await?;

    // Configure SQLite PRAGMAs for performance and correctness
    sqlx::raw_sql(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=5000;
         PRAGMA synchronous=NORMAL;"
    )
    .execute(&pool)
    .await?;

    // Run migrations
    run_migrations(&pool).await?;

    tracing::info!("Database initialized successfully");
    Ok(pool)
}

async fn run_migrations(pool: &SqlitePool) -> Result<()> {
    let migration = include_str!("../../migrations/001_initial.sql");

    sqlx::raw_sql(migration).execute(pool).await?;

    tracing::info!("Database migrations completed");
    Ok(())
}
