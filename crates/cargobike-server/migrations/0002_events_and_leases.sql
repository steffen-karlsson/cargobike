-- The immutable event log: deterministic IDs, per-release
-- sequence for SSE ids, ON CONFLICT DO NOTHING for at-least-once execution.
CREATE TABLE IF NOT EXISTS events (
 id TEXT PRIMARY KEY,
 release_id UUID NOT NULL,
 sequence BIGINT NOT NULL,
 at TIMESTAMPTZ NOT NULL,
 actor JSONB NOT NULL,
 kind TEXT NOT NULL,
 reason TEXT NOT NULL,
 data JSONB,
 UNIQUE (release_id, sequence)
);

-- Webhook payloads under their own retention .
CREATE TABLE IF NOT EXISTS webhook_payloads (
 delivery_id TEXT PRIMARY KEY,
 provider TEXT NOT NULL,
 payload JSONB NOT NULL,
 received_at TIMESTAMPTZ NOT NULL
);

-- Lease rows serialising work per (application, environment) .
CREATE TABLE IF NOT EXISTS leases (
 application TEXT NOT NULL,
 environment TEXT NOT NULL,
 holder_release_id UUID NOT NULL,
 PRIMARY KEY (application, environment)
);
