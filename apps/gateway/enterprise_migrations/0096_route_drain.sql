-- #49 D3: an operator drain on a deployment or a provider connection. A
-- drained route takes no new attempt (any deployment of a drained connection
-- is drained); requests already in flight finish, and the route stays as it
-- is everywhere else: enabled, listed, catalog-eligible and selectable. The
-- drain is one state: why (`drain_reason`, bounded operator text), until when
-- (`drain_until`, optional; evaluated whenever routes are read, so it lifts
-- on its own and no job clears it), who (`drained_by`: the governing user,
-- an admin token's owner) and when (`drained_at`). The four are set and
-- cleared together.
SET lock_timeout = '10s';

ALTER TABLE deployments
 ADD COLUMN drain_reason text,
 ADD COLUMN drain_until timestamptz,
 ADD COLUMN drained_by uuid REFERENCES users(id),
 ADD COLUMN drained_at timestamptz,
 ADD CONSTRAINT deployments_drain_whole CHECK (
  (drain_reason IS NULL) = (drained_at IS NULL)
  AND (drained_at IS NULL) = (drained_by IS NULL)
  AND (drain_until IS NULL OR drained_at IS NOT NULL)),
 ADD CONSTRAINT deployments_drain_reason CHECK (
  drain_reason IS NULL OR char_length(drain_reason) BETWEEN 1 AND 500);

ALTER TABLE provider_connections
 ADD COLUMN drain_reason text,
 ADD COLUMN drain_until timestamptz,
 ADD COLUMN drained_by uuid REFERENCES users(id),
 ADD COLUMN drained_at timestamptz,
 ADD CONSTRAINT provider_connections_drain_whole CHECK (
  (drain_reason IS NULL) = (drained_at IS NULL)
  AND (drained_at IS NULL) = (drained_by IS NULL)
  AND (drain_until IS NULL OR drained_at IS NOT NULL)),
 ADD CONSTRAINT provider_connections_drain_reason CHECK (
  drain_reason IS NULL OR char_length(drain_reason) BETWEEN 1 AND 500);

RESET lock_timeout;
