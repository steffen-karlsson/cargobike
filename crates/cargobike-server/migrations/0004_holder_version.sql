-- F-72's guard: never supersede a higher version. The lease row records
-- the holder's version so the supersede path compares by the scheme.
ALTER TABLE leases ADD COLUMN IF NOT EXISTS holder_version TEXT;
