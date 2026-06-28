use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{postgres::PgPoolOptions, postgres::PgRow, PgPool, Row};
use tracing::warn;

/// A single stored gallery document, keyed by (lowercased) wallet address.
#[derive(Debug, Clone)]
pub struct GalleryRow {
    pub address: String,
    pub data: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct Db {
    pub pool: PgPool,
}

impl Db {
    pub async fn new(database_url: &str, max_connections: u32) -> Self {
        let pool = Self::connect_with_retry(database_url, max_connections).await;
        tracing::info!("Postgres connection is healthy");

        // Migrations are embedded into the binary at compile time, so a fresh
        // deployment self-provisions its schema without sqlx-cli or the
        // migrations directory present in the distroless runtime image.
        // Already-applied migrations are skipped via the _sqlx_migrations table.
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("Failed to run database migrations");
        tracing::info!("Database migrations applied");

        Db { pool }
    }

    /// Connect to Postgres, retrying with a fixed backoff to tolerate a database
    /// that is still starting up. Panics once attempts are exhausted.
    async fn connect_with_retry(database_url: &str, max_connections: u32) -> PgPool {
        const MAX_ATTEMPTS: u32 = 10;
        const RETRY_DELAY: Duration = Duration::from_secs(1);

        for attempt in 1..=MAX_ATTEMPTS {
            match Self::try_connect(database_url, max_connections).await {
                Ok(pool) => return pool,
                Err(e) if attempt < MAX_ATTEMPTS => {
                    warn!(
                        "Postgres not ready (attempt {attempt}/{MAX_ATTEMPTS}): {e}. \
                         Retrying in {RETRY_DELAY:?}..."
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                Err(e) => {
                    panic!("Failed to connect to Postgres after {MAX_ATTEMPTS} attempts: {e}")
                }
            }
        }
        unreachable!("loop either returns a pool or panics on the final attempt")
    }

    /// Open a pool and verify the connection is usable with a trivial query.
    async fn try_connect(database_url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(database_url)
            .await?;
        sqlx::query("SELECT 1").execute(&pool).await?;
        Ok(pool)
    }

    /// Fetch the gallery document for an address, if it exists.
    pub async fn get_gallery(&self, address: &str) -> Result<Option<GalleryRow>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT address, data, created_at, updated_at FROM galleries WHERE address = $1",
        )
        .bind(address)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_gallery))
    }

    /// Insert or replace the gallery document for an address, returning the
    /// stored row (with refreshed timestamps).
    pub async fn upsert_gallery(
        &self,
        address: &str,
        data: &Value,
    ) -> Result<GalleryRow, sqlx::Error> {
        let row = sqlx::query(
            r#"
            INSERT INTO galleries (address, data, created_at, updated_at)
            VALUES ($1, $2, NOW(), NOW())
            ON CONFLICT (address) DO UPDATE SET
                data = EXCLUDED.data,
                updated_at = NOW()
            RETURNING address, data, created_at, updated_at
            "#,
        )
        .bind(address)
        .bind(data)
        .fetch_one(&self.pool)
        .await?;
        Ok(row_to_gallery(row))
    }

    /// Delete the gallery document for an address. Idempotent: deleting a
    /// missing address is a no-op (no error).
    pub async fn delete_gallery(&self, address: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM galleries WHERE address = $1")
            .bind(address)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn row_to_gallery(row: PgRow) -> GalleryRow {
    GalleryRow {
        address: row.get("address"),
        data: row.get("data"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}
