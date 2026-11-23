-- Route capacity counts pending attempts under the deployment lock. Include
-- the history join keys so each partition can answer the route lookup directly.
CREATE INDEX governance_pending_route
 ON governance_reservations(deployment_id, execution_id, admitted_at)
 WHERE state='pending';
