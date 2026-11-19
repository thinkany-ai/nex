CREATE TABLE relay_instances (
    id TEXT PRIMARY KEY,
    peer_addr TEXT,
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE relay_hosts (
    device_id TEXT PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
    instance_id TEXT NOT NULL REFERENCES relay_instances(id) ON DELETE CASCADE,
    connection_id TEXT NOT NULL,
    services JSONB NOT NULL
);
CREATE INDEX relay_hosts_instance ON relay_hosts(instance_id);

CREATE TABLE relay_tickets (
    token_hash TEXT PRIMARY KEY,
    from_device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    target_device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    service TEXT NOT NULL,
    expires_at BIGINT NOT NULL
);
CREATE INDEX relay_tickets_expiry ON relay_tickets(expires_at);
CREATE INDEX relay_tickets_from ON relay_tickets(from_device_id);
CREATE INDEX relay_tickets_target ON relay_tickets(target_device_id);

CREATE TABLE relay_streams (
    id TEXT PRIMARY KEY,
    instance_id TEXT NOT NULL REFERENCES relay_instances(id) ON DELETE CASCADE,
    from_device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    target_device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE
);
CREATE INDEX relay_streams_instance ON relay_streams(instance_id);
CREATE INDEX relay_streams_from ON relay_streams(from_device_id);
CREATE INDEX relay_streams_target ON relay_streams(target_device_id);
