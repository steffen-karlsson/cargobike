-- The create idempotency keys (F-109): a caller-supplied
-- `Idempotency-Key` header names the release the create produced, so a
-- retried request converges on the same row even when the
-- (application, version) pair differs (a key that raced a release's
-- deletion follows the row out: the retried create provisions fresh).
CREATE TABLE IF NOT EXISTS create_idempotency_keys (
    key TEXT PRIMARY KEY,
    release_id UUID NOT NULL REFERENCES releases (id) ON DELETE CASCADE
);
