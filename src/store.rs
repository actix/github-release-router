use std::{collections::BTreeMap, str::FromStr as _, time::Duration};

use anyhow::Result;
use sqlx::{
    Row as _, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

use crate::discord::Message;

#[derive(Clone)]
pub struct Store(pub SqlitePool);

pub struct Job {
    pub id: i64,
    pub delivery: String,
    pub repository: String,
    pub destination: String,
    pub routes: String,
    pub attempts: i64,
    pub message: Message,
}

impl Store {
    pub async fn open(path: &str) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(path)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::raw_sql("CREATE TABLE IF NOT EXISTS incoming (delivery TEXT PRIMARY KEY, created INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS jobs (
                id INTEGER PRIMARY KEY, delivery TEXT NOT NULL, repository TEXT NOT NULL,
                release_id INTEGER NOT NULL, destination TEXT NOT NULL, routes TEXT NOT NULL,
                payload TEXT, state TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
                next_attempt INTEGER NOT NULL DEFAULT 0, updated INTEGER NOT NULL,
                result TEXT, UNIQUE(repository, release_id, destination));
            CREATE INDEX IF NOT EXISTS jobs_due ON jobs(state, next_attempt);
            CREATE TABLE IF NOT EXISTS cooldown (id INTEGER PRIMARY KEY CHECK(id = 1), until INTEGER NOT NULL);
            INSERT OR IGNORE INTO cooldown VALUES (1, 0);")
            .execute(&pool).await?;
        Ok(Self(pool))
    }

    pub async fn recover(&self) -> Result<()> {
        sqlx::query("UPDATE jobs SET state = 'pending' WHERE state = 'sending'")
            .execute(&self.0)
            .await?;
        Ok(())
    }

    pub async fn enqueue(
        &self,
        delivery: &str,
        repository: &str,
        release_id: i64,
        destinations: &BTreeMap<String, Vec<String>>,
        message: &Message,
    ) -> Result<bool> {
        let mut tx = self.0.begin().await?;
        let inserted = sqlx::query("INSERT OR IGNORE INTO incoming VALUES (?, unixepoch())")
            .bind(delivery)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        if inserted {
            let payload = serde_json::to_string(message)?;
            for (destination, routes) in destinations {
                sqlx::query("INSERT OR IGNORE INTO jobs (delivery, repository, release_id, destination, routes, payload, updated) VALUES (?, ?, ?, ?, ?, ?, unixepoch())")
                    .bind(delivery).bind(repository).bind(release_id).bind(destination)
                    .bind(serde_json::to_string(routes)?).bind(&payload).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(inserted)
    }

    pub async fn claim(&self) -> Result<Option<Job>> {
        let row = sqlx::query("UPDATE jobs SET state = 'sending', attempts = attempts + 1, updated = unixepoch()
            WHERE id = (SELECT id FROM jobs WHERE state = 'pending' AND next_attempt <= unixepoch()
            AND (SELECT until FROM cooldown WHERE id = 1) <= unixepoch() ORDER BY next_attempt, id LIMIT 1)
            RETURNING id, delivery, repository, destination, routes, attempts, payload")
            .fetch_optional(&self.0).await?;
        row.map(|r| {
            Ok(Job {
                id: r.get("id"),
                delivery: r.get("delivery"),
                repository: r.get("repository"),
                destination: r.get("destination"),
                routes: r.get("routes"),
                attempts: r.get("attempts"),
                message: serde_json::from_str(r.get("payload"))?,
            })
        })
        .transpose()
    }

    pub async fn finish(
        &self,
        id: i64,
        state: &str,
        delay: i64,
        result: &str,
        rate_limited: bool,
    ) -> Result<()> {
        let mut tx = self.0.begin().await?;
        sqlx::query("UPDATE jobs SET state = ?, next_attempt = unixepoch() + ?, result = ?, updated = unixepoch() WHERE id = ?")
            .bind(state).bind(delay).bind(result).bind(id).execute(&mut *tx).await?;
        if rate_limited {
            // A single shared cooldown also respects global Discord limits.
            sqlx::query("UPDATE cooldown SET until = max(until, unixepoch() + ?) WHERE id = 1")
                .bind(delay)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn maintain(&self) -> Result<()> {
        let mut tx = self.0.begin().await?;
        sqlx::query("DELETE FROM incoming WHERE created < unixepoch() - 2592000")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE jobs SET payload = NULL WHERE state = 'sent' AND updated < unixepoch() - 2592000 AND payload IS NOT NULL").execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn retry(&self, id: i64) -> Result<bool> {
        Ok(sqlx::query("UPDATE jobs SET state = 'pending', attempts = 0, next_attempt = 0, updated = unixepoch(), result = 'operator retry' WHERE id = ? AND state = 'failed'").bind(id).execute(&self.0).await?.rows_affected() == 1)
    }

    pub async fn failed(&self) -> Result<Vec<serde_json::Value>> {
        let rows = sqlx::query("SELECT id, delivery, repository, destination, routes, attempts, result FROM jobs WHERE state = 'failed' ORDER BY id").fetch_all(&self.0).await?;
        Ok(rows.into_iter().map(|r| serde_json::json!({"id":r.get::<i64,_>("id"), "delivery":r.get::<String,_>("delivery"), "repository":r.get::<String,_>("repository"), "destination":r.get::<String,_>("destination"), "routes":r.get::<String,_>("routes"), "attempts":r.get::<i64,_>("attempts"), "result":r.get::<Option<String>,_>("result")})).collect())
    }
}
