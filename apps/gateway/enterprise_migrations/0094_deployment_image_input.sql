-- #24: inline image input on Chat Completions and Responses is opt-in per
-- route. A deployment is one upstream route, so the operator who knows that
-- its upstream model accepts images (and bills them as input tokens) allows
-- them here; candidates without it are skipped before the route plan.
--
-- A constant default makes this a catalog-only change (no table rewrite or
-- scan); the deployment triggers already in place bump the catalog version
-- on a change, and the enablement revision and history (0067) move only
-- when `enabled` changes, so this column adds nothing to them.
SET lock_timeout = '10s';

ALTER TABLE deployments ADD COLUMN image_input boolean NOT NULL DEFAULT false;
