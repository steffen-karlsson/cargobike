-- The approvals table (F-59/F-96): one decision per principal per
-- (release, environment, step, attempt). The attempt is the ordinal
-- of the attempt the submission observed (the document's attempts
-- array length) — approvals do NOT carry over to a retry; the
-- head_sha binds the decision to the CR the wait parks against
-- (NULL when the environment's flow has no CR yet).
CREATE TABLE IF NOT EXISTS approvals (
  release_id UUID NOT NULL,
  environment TEXT NOT NULL,
  step_id TEXT NOT NULL,
  attempt INTEGER NOT NULL,
  principal TEXT NOT NULL,
  head_sha TEXT,
  decision TEXT NOT NULL,
  approver TEXT NOT NULL,
  comment TEXT,
  at TIMESTAMPTZ NOT NULL,
  PRIMARY KEY (release_id, environment, step_id, attempt, principal)
);

-- The threshold count reads the distinct approving principals of the
-- current attempt (an index covers the count's predicate).
CREATE INDEX IF NOT EXISTS approvals_threshold
  ON approvals (release_id, environment, step_id, attempt, decision);
