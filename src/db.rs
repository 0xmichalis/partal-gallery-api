use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, postgres::PgPoolOptions, types::Json};
use tracing::warn;

/// A single stored gallery document, keyed by (lowercased) wallet address.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct GalleryRow {
    pub address: String,
    pub revision: i64,
    pub data: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug)]
pub enum WriteCondition {
    /// Compatibility for existing callers during the frontend rollout.
    Any,
    Absent,
    Matches(i64),
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
        sqlx::query_as(
            "SELECT address, data, created_at, updated_at, revision FROM galleries WHERE address = $1",
        )
        .bind(address)
        .fetch_optional(&self.pool)
        .await
    }

    /// Insert or replace the gallery document for an address, returning the
    /// stored row (with refreshed timestamps).
    pub async fn upsert_gallery(
        &self,
        address: &str,
        data: &[Value],
        condition: WriteCondition,
    ) -> Result<Option<GalleryRow>, sqlx::Error> {
        let query = match condition {
            WriteCondition::Any => {
                r#"
                INSERT INTO galleries (address, data) VALUES ($1, $2)
                ON CONFLICT (address) DO UPDATE SET data = EXCLUDED.data,
                    updated_at = NOW(), revision = nextval('gallery_revision_seq')
                RETURNING address, data, created_at, updated_at, revision
            "#
            }
            WriteCondition::Absent => {
                r#"
                INSERT INTO galleries (address, data) VALUES ($1, $2)
                ON CONFLICT (address) DO NOTHING
                RETURNING address, data, created_at, updated_at, revision
            "#
            }
            WriteCondition::Matches(_) => {
                r#"
                UPDATE galleries SET data = $2, updated_at = NOW(),
                    revision = nextval('gallery_revision_seq')
                WHERE address = $1 AND revision = $3
                RETURNING address, data, created_at, updated_at, revision
            "#
            }
        };
        let mut query = sqlx::query_as(query).bind(address).bind(Json(data));
        if let WriteCondition::Matches(revision) = condition {
            query = query.bind(revision);
        }
        query.fetch_optional(&self.pool).await
    }

    /// Conditional deletes cannot remove a document updated since the caller's read.
    pub async fn delete_gallery(
        &self,
        address: &str,
        condition: WriteCondition,
    ) -> Result<bool, sqlx::Error> {
        match condition {
            // This precondition only observes whether the key exists.
            WriteCondition::Absent => {
                sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM galleries WHERE address = $1)")
                    .bind(address)
                    .fetch_one(&self.pool)
                    .await
            }
            WriteCondition::Matches(revision) => {
                let result =
                    sqlx::query("DELETE FROM galleries WHERE address = $1 AND revision = $2")
                        .bind(address)
                        .bind(revision)
                        .execute(&self.pool)
                        .await?;
                Ok(result.rows_affected() == 1)
            }
            WriteCondition::Any => {
                sqlx::query("DELETE FROM galleries WHERE address = $1")
                    .bind(address)
                    .execute(&self.pool)
                    .await?;
                Ok(true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Db, WriteCondition};
    use serde_json::json;
    use sqlx::PgPool;

    #[sqlx::test]
    #[ignore = "requires an isolated Postgres DATABASE_URL"]
    async fn only_one_concurrent_create_or_update_succeeds(pool: PgPool) {
        let db = Db { pool };
        let a = [json!({"id": "a"})];
        let b = [json!({"id": "b"})];
        let (first, second) = tokio::join!(
            db.upsert_gallery("owner", &a, WriteCondition::Absent),
            db.upsert_gallery("owner", &b, WriteCondition::Absent),
        );
        assert_eq!(
            usize::from(first.unwrap().is_some()) + usize::from(second.unwrap().is_some()),
            1
        );
        let revision = db.get_gallery("owner").await.unwrap().unwrap().revision;
        let (first, second) = tokio::join!(
            db.upsert_gallery("owner", &a, WriteCondition::Matches(revision)),
            db.upsert_gallery("owner", &b, WriteCondition::Matches(revision)),
        );
        assert_eq!(
            usize::from(first.unwrap().is_some()) + usize::from(second.unwrap().is_some()),
            1
        );
        assert!(
            db.upsert_gallery("missing", &a, WriteCondition::Matches(revision))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires an isolated Postgres DATABASE_URL"]
    async fn stale_delete_and_delete_recreate_cannot_reuse_a_revision(pool: PgPool) {
        let db = Db { pool };
        let data = [];
        let first = db
            .upsert_gallery("owner", &data, WriteCondition::Absent)
            .await
            .unwrap()
            .unwrap();
        let second = db
            .upsert_gallery("owner", &data, WriteCondition::Matches(first.revision))
            .await
            .unwrap()
            .unwrap();
        assert!(
            !db.delete_gallery("owner", WriteCondition::Matches(first.revision))
                .await
                .unwrap()
        );
        assert!(
            db.delete_gallery("owner", WriteCondition::Matches(second.revision))
                .await
                .unwrap()
        );
        let recreated = db
            .upsert_gallery("owner", &data, WriteCondition::Absent)
            .await
            .unwrap()
            .unwrap();
        assert!(recreated.revision > second.revision);
        assert!(
            db.upsert_gallery("owner", &data, WriteCondition::Matches(second.revision))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !db.delete_gallery("owner", WriteCondition::Absent)
                .await
                .unwrap()
        );
    }

    #[sqlx::test]
    #[ignore = "requires an isolated Postgres DATABASE_URL"]
    async fn absence_check_does_not_decode_legacy_data(pool: PgPool) {
        let db = Db { pool };
        // JSONB can store numbers outside serde_json's range.
        sqlx::query("INSERT INTO galleries (address, data) VALUES ('owner', '[1e400]')")
            .execute(&db.pool)
            .await
            .unwrap();

        assert!(matches!(
            db.get_gallery("owner").await,
            Err(sqlx::Error::ColumnDecode { .. })
        ));
        assert!(
            !db.delete_gallery("owner", WriteCondition::Absent)
                .await
                .unwrap()
        );
        assert!(
            db.delete_gallery("owner", WriteCondition::Any)
                .await
                .unwrap()
        );
        assert!(
            db.delete_gallery("owner", WriteCondition::Absent)
                .await
                .unwrap()
        );
    }
}
