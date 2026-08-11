use std::path::Path;

use crate::remote::request_coordinator::{
    PersistedRemoteLane, RemoteCoordinatorError, RemoteCoordinatorStore, RemoteFuture,
    RemoteLaneKey,
};

#[derive(Clone)]
pub struct SqliteRemoteCoordinatorStore {
    pool: sqlx::SqlitePool,
}

impl SqliteRemoteCoordinatorStore {
    pub async fn open(path: &Path) -> Result<Self, RemoteCoordinatorError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(RemoteCoordinatorError::Io)?;
        }
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                super::pools::options(path, true, false)
                    .map_err(|error| RemoteCoordinatorError::Persistence(error.to_string()))?,
            )
            .await
            .map_err(persistence)?;
        sqlx::raw_sql(
            "create table if not exists remote_lane_cooldown (\
             canonical_host text not null, credential_profile text not null, \
             next_request_unix_ms integer not null, retry_count integer not null, \
             updated_unix_ms integer not null, primary key(canonical_host, credential_profile));\
             create table if not exists remote_observation_subscription (\
             observation_key text not null, subscriber_id text not null, created_unix_ms integer not null, \
             primary key(observation_key, subscriber_id));",
        )
        .execute(&pool)
        .await
        .map_err(persistence)?;
        Ok(Self { pool })
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

impl RemoteCoordinatorStore for SqliteRemoteCoordinatorStore {
    fn load_lanes<'a>(&'a self) -> RemoteFuture<'a, Vec<PersistedRemoteLane>> {
        Box::pin(async move {
            let rows = sqlx::query_as::<_, (String, String, i64, i64, i64)>(
                "select canonical_host, credential_profile, next_request_unix_ms, retry_count, updated_unix_ms from remote_lane_cooldown",
            )
            .fetch_all(&self.pool)
            .await
            .map_err(persistence)?;
            rows.into_iter()
                .map(
                    |(host, profile, next_request_unix_ms, retry_count, updated_unix_ms)| {
                        Ok(PersistedRemoteLane {
                            key: RemoteLaneKey::new(host, profile)?,
                            next_request_unix_ms,
                            retry_count: u32::try_from(retry_count).map_err(|_| {
                                RemoteCoordinatorError::Persistence(
                                    "remote lane retry count is out of range".into(),
                                )
                            })?,
                            updated_unix_ms,
                        })
                    },
                )
                .collect()
        })
    }

    fn save_lane<'a>(&'a self, lane: &'a PersistedRemoteLane) -> RemoteFuture<'a, ()> {
        Box::pin(async move {
            sqlx::query("insert into remote_lane_cooldown(canonical_host, credential_profile, next_request_unix_ms, retry_count, updated_unix_ms) values(?,?,?,?,?) on conflict(canonical_host, credential_profile) do update set next_request_unix_ms=excluded.next_request_unix_ms, retry_count=excluded.retry_count, updated_unix_ms=excluded.updated_unix_ms")
                .bind(&lane.key.canonical_host)
                .bind(&lane.key.credential_profile)
                .bind(lane.next_request_unix_ms)
                .bind(i64::from(lane.retry_count))
                .bind(lane.updated_unix_ms)
                .execute(&self.pool)
                .await
                .map_err(persistence)?;
            Ok(())
        })
    }
}

fn persistence(error: sqlx::Error) -> RemoteCoordinatorError {
    RemoteCoordinatorError::Persistence(error.to_string())
}
