-- The releases table: JSONB documents, with indexed columns for the
-- filters; the (application, version) pair is unique among
-- non-terminal releases.
CREATE TABLE IF NOT EXISTS releases (
 id UUID PRIMARY KEY,
 application TEXT NOT NULL,
 version TEXT NOT NULL,
 phase TEXT NOT NULL,
 terminal BOOLEAN NOT NULL DEFAULT FALSE,
 resource_version BIGINT NOT NULL DEFAULT 1,
 created_at TIMESTAMPTZ NOT NULL,
 updated_at TIMESTAMPTZ NOT NULL,
 retried_from UUID,
 document JSONB NOT NULL
);

-- (application, version) unique among non-terminal releases .
CREATE UNIQUE INDEX IF NOT EXISTS releases_active_key
 ON releases (application, version)
 WHERE NOT terminal;
