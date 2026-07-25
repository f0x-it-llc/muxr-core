//! SQLite-backed registration store.
//!
//! rusqlite is synchronous; the connection lives behind a `std::sync::Mutex` and
//! every access runs inside `spawn_blocking` so the async runtime never stalls on
//! disk I/O.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::{Arc, Mutex};

/// A stored device registration.
#[derive(Debug, Clone)]
pub struct Registration {
    pub handle: String,
    pub platform: String,
    pub fcm_token: String,
    /// FCM reported the token UNREGISTERED/NOT_FOUND — further notifies → 410.
    pub pruned: bool,
    /// UTC day-index bucket (`unix_secs / 86400`) of the current daily counter.
    pub day_bucket: String,
    pub sends_today: u32,
}

/// Cloneable handle to the shared connection.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    /// Open (creating if absent) the SQLite database and ensure the schema.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS registrations (
                handle      TEXT PRIMARY KEY,
                platform    TEXT NOT NULL,
                fcm_token   TEXT NOT NULL,
                created_at  INTEGER NOT NULL,
                last_send   INTEGER,
                day_bucket  TEXT NOT NULL DEFAULT '',
                sends_today INTEGER NOT NULL DEFAULT 0,
                pruned      INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().expect("store mutex poisoned");
            f(&guard)
        })
        .await?
    }

    /// Register a device. When `existing` is a known handle, its row's token is
    /// refreshed in place (handle preserved, prune cleared) and that handle is
    /// returned; otherwise `new_handle` is minted. Returns the effective handle.
    pub async fn register(
        &self,
        platform: String,
        token: String,
        existing: Option<String>,
        new_handle: String,
        now: i64,
    ) -> Result<String> {
        self.with_conn(move |c| {
            if let Some(handle) = existing {
                let found: Option<i64> = c
                    .query_row(
                        "SELECT 1 FROM registrations WHERE handle = ?1",
                        params![handle],
                        |r| r.get(0),
                    )
                    .optional()?;
                if found.is_some() {
                    c.execute(
                        "UPDATE registrations
                         SET fcm_token = ?1, platform = ?2, pruned = 0
                         WHERE handle = ?3",
                        params![token, platform, handle],
                    )?;
                    return Ok(handle);
                }
            }
            c.execute(
                "INSERT INTO registrations
                 (handle, platform, fcm_token, created_at, sends_today, pruned)
                 VALUES (?1, ?2, ?3, ?4, 0, 0)",
                params![new_handle, platform, token, now],
            )?;
            Ok(new_handle)
        })
        .await
    }

    /// Fetch a registration by handle.
    pub async fn get(&self, handle: String) -> Result<Option<Registration>> {
        self.with_conn(move |c| {
            let reg = c
                .query_row(
                    "SELECT handle, platform, fcm_token, pruned, day_bucket, sends_today
                     FROM registrations WHERE handle = ?1",
                    params![handle],
                    |r| {
                        Ok(Registration {
                            handle: r.get(0)?,
                            platform: r.get(1)?,
                            fcm_token: r.get(2)?,
                            pruned: r.get::<_, i64>(3)? != 0,
                            day_bucket: r.get(4)?,
                            sends_today: r.get::<_, i64>(5)? as u32,
                        })
                    },
                )
                .optional()?;
            Ok(reg)
        })
        .await
    }

    /// Record a successful send: bump today's counter (resetting on a new UTC day)
    /// and stamp `last_send`.
    pub async fn record_send(&self, handle: String, day_bucket: String, now: i64) -> Result<()> {
        self.with_conn(move |c| {
            c.execute(
                "UPDATE registrations
                 SET sends_today = CASE WHEN day_bucket = ?2 THEN sends_today + 1 ELSE 1 END,
                     day_bucket  = ?2,
                     last_send   = ?3
                 WHERE handle = ?1",
                params![handle, day_bucket, now],
            )?;
            Ok(())
        })
        .await
    }

    /// Mark a handle as pruned (FCM said UNREGISTERED).
    pub async fn mark_pruned(&self, handle: String) -> Result<()> {
        self.with_conn(move |c| {
            c.execute(
                "UPDATE registrations SET pruned = 1 WHERE handle = ?1",
                params![handle],
            )?;
            Ok(())
        })
        .await
    }

    /// Delete a registration. Idempotent — succeeds whether or not the row exists.
    pub async fn delete(&self, handle: String) -> Result<()> {
        self.with_conn(move |c| {
            c.execute(
                "DELETE FROM registrations WHERE handle = ?1",
                params![handle],
            )?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open(path.to_str().unwrap()).unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn register_then_get_roundtrip() {
        let (store, _dir) = temp_store();
        let h = store
            .register("android".into(), "tok".into(), None, "handle-a".into(), 100)
            .await
            .unwrap();
        assert_eq!(h, "handle-a");
        let reg = store.get("handle-a".into()).await.unwrap().unwrap();
        assert_eq!(reg.fcm_token, "tok");
        assert!(!reg.pruned);
    }

    #[tokio::test]
    async fn existing_handle_refreshes_token() {
        let (store, _dir) = temp_store();
        store
            .register(
                "android".into(),
                "tok1".into(),
                None,
                "handle-a".into(),
                100,
            )
            .await
            .unwrap();
        // Re-register with the existing handle but a fresh minted candidate.
        let h = store
            .register(
                "android".into(),
                "tok2".into(),
                Some("handle-a".into()),
                "handle-b".into(),
                200,
            )
            .await
            .unwrap();
        assert_eq!(h, "handle-a", "existing handle must be preserved");
        let reg = store.get("handle-a".into()).await.unwrap().unwrap();
        assert_eq!(reg.fcm_token, "tok2", "token must be refreshed");
        assert!(store.get("handle-b".into()).await.unwrap().is_none());
    }
}
