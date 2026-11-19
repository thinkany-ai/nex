//! Postgres coordinates owners and reservations. WebSocket payloads travel
//! directly between relay processes; they never pass through the database.

use crate::{db::DeviceRow, lock, util::now, AppState, TicketEntry};
use nex_proto::ServiceInfo;
use sqlx::{types::Json, FromRow};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

pub(crate) const LEASE: Duration = Duration::from_secs(15);
const HEARTBEAT: Duration = Duration::from_secs(1);

#[derive(FromRow)]
pub(crate) struct OnlineHost {
    device_id: String,
    pub services: Json<Vec<ServiceInfo>>,
}

impl AppState {
    pub(crate) async fn renew_lease(&self) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO relay_instances (id, peer_addr, expires_at)
             VALUES ($1, $2, clock_timestamp() + make_interval(secs => $3))
             ON CONFLICT (id) DO UPDATE SET expires_at = EXCLUDED.expires_at",
        )
        .bind(&self.instance_id)
        .bind(self.config.peer_addr.map(|addr| addr.to_string()))
        .bind(LEASE.as_secs_f64())
        .execute(&self.db.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn claim_host(
        &self,
        device: &str,
        connection: &str,
        services: &[ServiceInfo],
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "INSERT INTO relay_hosts (device_id, instance_id, connection_id, services)
             SELECT d.id, $2, $3, $4 FROM devices d
             JOIN accounts a ON a.id = d.account_id JOIN apps p ON p.id = a.app_id
             WHERE d.id = $1 AND d.revoked_at IS NULL AND NOT p.disabled
             ON CONFLICT (device_id) DO UPDATE SET
               instance_id = EXCLUDED.instance_id, connection_id = EXCLUDED.connection_id,
               services = EXCLUDED.services",
        )
        .bind(device)
        .bind(&self.instance_id)
        .bind(connection)
        .bind(Json(services))
        .execute(&self.db.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub(crate) async fn update_host(
        &self,
        device: &str,
        connection: &str,
        services: &[ServiceInfo],
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE relay_hosts SET services = $3 WHERE device_id = $1 AND connection_id = $2",
        )
        .bind(device)
        .bind(connection)
        .bind(Json(services))
        .execute(&self.db.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn release_host(&self, device: &str, connection: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM relay_hosts WHERE device_id = $1 AND connection_id = $2")
            .bind(device)
            .bind(connection)
            .execute(&self.db.pool)
            .await?;
        Ok(())
    }

    pub(crate) async fn online_hosts(
        &self,
        devices: &[DeviceRow],
    ) -> anyhow::Result<HashMap<String, OnlineHost>> {
        let ids: Vec<_> = devices.iter().map(|d| d.id.as_str()).collect();
        let rows = sqlx::query_as::<_, OnlineHost>(
            "SELECT h.device_id, h.services
             FROM relay_hosts h JOIN relay_instances i ON i.id = h.instance_id
             WHERE h.device_id = ANY($1) AND i.expires_at > clock_timestamp()",
        )
        .bind(&ids)
        .fetch_all(&self.db.pool)
        .await?;
        Ok(rows.into_iter().map(|h| (h.device_id.clone(), h)).collect())
    }

    pub(crate) async fn peer_for_device(&self, device: &str) -> anyhow::Result<Option<SocketAddr>> {
        let peer: Option<String> = sqlx::query_scalar(
            "SELECT i.peer_addr FROM relay_hosts h JOIN relay_instances i ON i.id = h.instance_id
             WHERE h.device_id = $1 AND h.instance_id <> $2 AND i.expires_at > clock_timestamp()",
        )
        .bind(device)
        .bind(&self.instance_id)
        .fetch_optional(&self.db.pool)
        .await?
        .flatten();
        peer.map(|addr| addr.parse().map_err(Into::into))
            .transpose()
    }

    pub(crate) async fn insert_ticket(
        &self,
        token_hash: &str,
        entry: &TicketEntry,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO relay_tickets (token_hash, from_device_id, target_device_id, service, expires_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(token_hash)
        .bind(&entry.from_device_id)
        .bind(&entry.target_device_id)
        .bind(&entry.service)
        .bind(entry.expires_at)
        .execute(&self.db.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn peer_for_ticket(&self, hash: &str) -> anyhow::Result<Option<SocketAddr>> {
        let addr: Option<String> = sqlx::query_scalar(
            "SELECT i.peer_addr FROM relay_tickets t
             JOIN relay_hosts h ON h.device_id = t.target_device_id
             JOIN relay_instances i ON i.id = h.instance_id
             WHERE t.token_hash = $1 AND t.expires_at > $2
               AND h.instance_id <> $3 AND i.expires_at > clock_timestamp()",
        )
        .bind(hash)
        .bind(now())
        .bind(&self.instance_id)
        .fetch_optional(&self.db.pool)
        .await?
        .flatten();
        addr.map(|addr| addr.parse().map_err(Into::into))
            .transpose()
    }

    pub(crate) async fn local_host_connection(
        &self,
        device: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT h.connection_id FROM relay_hosts h JOIN relay_instances i ON i.id = h.instance_id
             WHERE h.device_id = $1 AND h.instance_id = $2 AND i.expires_at > clock_timestamp()",
        )
        .bind(device)
        .bind(&self.instance_id)
        .fetch_optional(&self.db.pool)
        .await?)
    }

    pub(crate) async fn consume_ticket(&self, hash: &str) -> anyhow::Result<Option<TicketEntry>> {
        Ok(sqlx::query_as(
            "DELETE FROM relay_tickets WHERE token_hash = $1
             RETURNING from_device_id, target_device_id, service, expires_at",
        )
        .bind(hash)
        .fetch_optional(&self.db.pool)
        .await?)
    }

    pub(crate) async fn reserve_stream(
        &self,
        id: &str,
        entry: &TicketEntry,
    ) -> anyhow::Result<bool> {
        // All reservations sharing either device serialize on the same rows.
        // Acquire locks in ID order so opposite-direction opens cannot deadlock.
        let ids = [&entry.from_device_id, &entry.target_device_id];
        let mut tx = self.db.pool.begin().await?;
        let locked: Vec<String> = sqlx::query_scalar(
            "SELECT d.id FROM devices d JOIN accounts a ON a.id = d.account_id
             JOIN apps p ON p.id = a.app_id
             WHERE d.id = ANY($1) AND d.revoked_at IS NULL AND NOT p.disabled
             ORDER BY d.id FOR NO KEY UPDATE OF d",
        )
        .bind(&ids[..])
        .fetch_all(&mut *tx)
        .await?;
        let same_device = ids[0] == ids[1];
        if locked.len() != if same_device { 1 } else { 2 } {
            return Ok(false);
        }
        let full: bool = sqlx::query_scalar(
            "SELECT EXISTS (
               SELECT device_id FROM (
                 SELECT s.from_device_id AS device_id FROM relay_streams s
                 JOIN relay_instances i ON i.id = s.instance_id
                 WHERE s.from_device_id = ANY($1) AND i.expires_at > clock_timestamp()
                 UNION ALL
                 SELECT s.target_device_id FROM relay_streams s
                 JOIN relay_instances i ON i.id = s.instance_id
                 WHERE s.target_device_id = ANY($1) AND i.expires_at > clock_timestamp()
               ) used GROUP BY device_id HAVING count(*) + $2 > $3
             )",
        )
        .bind(&ids[..])
        .bind(if same_device { 2_i64 } else { 1_i64 })
        .bind(self.config.max_streams_per_device as i64)
        .fetch_one(&mut *tx)
        .await?;
        if full || self.config.max_streams_per_device < if same_device { 2 } else { 1 } {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO relay_streams (id, instance_id, from_device_id, target_device_id)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(&self.instance_id)
        .bind(&entry.from_device_id)
        .bind(&entry.target_device_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub(crate) async fn release_stream(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM relay_streams WHERE id = $1")
            .bind(id)
            .execute(&self.db.pool)
            .await?;
        Ok(())
    }

    async fn retry_releases(&self) -> anyhow::Result<()> {
        let hosts: Vec<_> = lock(&self.closed_hosts).iter().cloned().collect();
        if !hosts.is_empty() {
            sqlx::query(
                "DELETE FROM relay_hosts WHERE instance_id = $1 AND connection_id = ANY($2)",
            )
            .bind(&self.instance_id)
            .bind(&hosts)
            .execute(&self.db.pool)
            .await?;
            lock(&self.closed_hosts).retain(|id| !hosts.contains(id));
        }
        let streams: Vec<_> = lock(&self.closed_streams).iter().cloned().collect();
        if !streams.is_empty() {
            sqlx::query("DELETE FROM relay_streams WHERE instance_id = $1 AND id = ANY($2)")
                .bind(&self.instance_id)
                .bind(&streams)
                .execute(&self.db.pool)
                .await?;
            lock(&self.closed_streams).retain(|id| !streams.contains(id));
        }
        Ok(())
    }

    async fn reconcile(&self) -> anyhow::Result<()> {
        let watched: Vec<_> = {
            let mut guards = lock(&self.guards);
            guards.retain(|_, guard| guard.strong_count() > 0);
            guards
                .iter()
                .filter_map(|(id, guard)| Some((id.clone(), guard.upgrade()?)))
                .collect()
        };
        let hosts: Vec<_> = lock(&self.hosts)
            .iter()
            .map(|(id, host)| (id.clone(), host.conn_id.clone()))
            .collect();
        let ids: Vec<_> = watched.iter().map(|(id, _)| id.as_str()).collect();
        let active: Vec<String> = sqlx::query_scalar(
            "SELECT d.id FROM devices d JOIN accounts a ON a.id = d.account_id
             JOIN apps p ON p.id = a.app_id
             WHERE d.id = ANY($1) AND d.revoked_at IS NULL AND NOT p.disabled",
        )
        .bind(&ids)
        .fetch_all(&self.db.pool)
        .await?;
        let active: std::collections::HashSet<_> = active.into_iter().collect();
        for (id, guard) in watched {
            if !active.contains(&id) {
                guard.cancel();
            }
        }
        let owners: HashMap<String, String> = sqlx::query_as(
            "SELECT device_id, connection_id FROM relay_hosts WHERE instance_id = $1",
        )
        .bind(&self.instance_id)
        .fetch_all(&self.db.pool)
        .await?
        .into_iter()
        .collect();
        let mut local = lock(&self.hosts);
        for (device, conn) in hosts {
            if owners.get(&device) != Some(&conn)
                && local.get(&device).is_some_and(|h| h.conn_id == conn)
            {
                local.remove(&device);
            }
        }
        Ok(())
    }
}

pub(crate) async fn maintain(state: Arc<AppState>) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(HEARTBEAT);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut cleanup = tokio::time::Instant::now();
    let mut last_success = tokio::time::Instant::now();
    loop {
        tick.tick().await;
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            state.renew_lease().await?;
            state.reconcile().await?;
            state.retry_releases().await?;
            if cleanup.elapsed() >= LEASE {
                sqlx::query("DELETE FROM relay_instances WHERE expires_at <= clock_timestamp()")
                    .execute(&state.db.pool)
                    .await?;
                sqlx::query("DELETE FROM relay_tickets WHERE expires_at <= $1")
                    .bind(now())
                    .execute(&state.db.pool)
                    .await?;
                cleanup = tokio::time::Instant::now();
            }
            Ok::<_, anyhow::Error>(())
        })
        .await;
        match result {
            Ok(Ok(())) => last_success = tokio::time::Instant::now(),
            err => {
                tracing::warn!(?err, "relay coordination heartbeat failed");
                // Fail closed before other pods can reclaim this instance's slots.
                if last_success.elapsed() >= LEASE - Duration::from_secs(4) {
                    anyhow::bail!("relay coordination lease unavailable");
                }
            }
        }
    }
}
