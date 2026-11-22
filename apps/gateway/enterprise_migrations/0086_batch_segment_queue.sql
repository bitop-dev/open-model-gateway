-- A settled provider answer can release its route slot before the writer
-- publishes a result. The line remains running until the segment commits.
ALTER TABLE batch_lines ADD COLUMN awaiting_segment boolean NOT NULL DEFAULT false;
ALTER TABLE batch_lines ADD CONSTRAINT batch_lines_awaiting_segment_check CHECK(
 NOT awaiting_segment OR (state='running' AND execution_id IS NOT NULL AND segment IS NULL)
);

DROP INDEX batch_lines_running_route;
CREATE INDEX batch_lines_running_route ON batch_lines(deployment_id,workspace_id)
 WHERE state='running' AND NOT awaiting_segment;
