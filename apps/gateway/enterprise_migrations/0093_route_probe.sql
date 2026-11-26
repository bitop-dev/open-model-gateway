-- #49 D1: a route whose cooldown has passed is half-open, and one real
-- request decides whether traffic resumes. That request holds the route's
-- probe slot on its health row: `probe_holder` and `probe_epoch` name the
-- claim (the epoch only increases), and `probe_until` ends it on its own (the
-- request timeout plus the settlement deadline), so a holder that dies, is
-- dropped or never decides cannot keep the route closed. `reopen_count` is how
-- many times the circuit has opened since the route last served a complete
-- success; each opening lasts the deployment's cooldown doubled that many
-- times, capped. No row means a route that has never failed, as before.
SET lock_timeout = '10s';

ALTER TABLE deployment_health
 ADD COLUMN probe_holder uuid,
 ADD COLUMN probe_until timestamptz,
 ADD COLUMN probe_epoch bigint NOT NULL DEFAULT 0 CHECK (probe_epoch >= 0),
 ADD COLUMN reopen_count integer NOT NULL DEFAULT 0 CHECK (reopen_count >= 0),
 ADD CONSTRAINT deployment_health_probe_whole CHECK ((probe_holder IS NULL) = (probe_until IS NULL));
RESET lock_timeout;
