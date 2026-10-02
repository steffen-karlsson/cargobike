-- The supersede guard: never a higher version. The lease row records
-- the holder's version so the supersede path compares by the scheme.
ALTER TABLE leases ADD COLUMN IF NOT EXISTS holder_version TEXT;
