-- F-113's tamper evidence: each event's `hash` covers its
-- predecessor's (`prev`), so any edit or deletion breaks the chain a
-- reader can re-derive; the empty string is the genesis. Both columns
-- are backfilled as empty for the rows the pre-chain writes left
-- (the log has been write-only so far — nothing reads them).
ALTER TABLE events ADD COLUMN IF NOT EXISTS prev TEXT NOT NULL DEFAULT '';
ALTER TABLE events ADD COLUMN IF NOT EXISTS hash TEXT NOT NULL DEFAULT '';
