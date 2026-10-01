-- CR correlation (F-63): when a change request opens, the engine records
-- (provider, repo_id, cr_number) -> (release, environment, step, attempt).
-- A pull_request.closed webhook correlates via this row; the reconciler
-- sweeps it.
CREATE TABLE IF NOT EXISTS cr_correlation (
    provider    TEXT NOT NULL,
    repo_id     TEXT NOT NULL,
    cr_number   BIGINT NOT NULL,
    release_id  UUID NOT NULL,
    environment TEXT NOT NULL,
    step_id     TEXT NOT NULL,
    workflow_id TEXT NOT NULL,
    PRIMARY KEY (provider, repo_id, cr_number)
);
