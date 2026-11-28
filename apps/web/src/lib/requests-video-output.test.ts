import { describe, expect, it } from "vitest";
import { videoOutputText } from "./requests";

describe("a completed video's output in Logs (#52)", () => {
  it("says whether it is stored, not yet, or never (and that the job is still charged)", () => {
    expect(videoOutputText({ output_stored: true, output_pending: false, output_failure: null })).toBe("Stored in the gateway");
    expect(videoOutputText({ output_stored: false, output_pending: true, output_failure: null })).toBe("Not stored yet");
    expect(videoOutputText({ output_stored: false, output_pending: false, output_failure: null })).toBe("Not stored: completed before the gateway stored videos. The video was generated and is charged.");
    expect(videoOutputText({ output_stored: false, output_pending: false, output_failure: "upstream_id_reused" })).toMatch(/another job/);
    expect(videoOutputText({ output_stored: false, output_failure: "too_large" })).toBe("Not stored, for good: larger than the 512 MiB the gateway stores. The video was generated and is charged.");
    expect(videoOutputText({ output_stored: false, output_failure: "new_code" })).toMatch(/^Not stored, for good: new_code\./);
    expect(videoOutputText({ output_stored: false, output_expired: true, output_pending: false, output_failure: null })).toBe("Stored, then removed by the video retention");
    expect(videoOutputText({ output_stored: false, output_pending: false, output_failure: null, deleted_at: "2026-11-28T00:00:00Z" })).toBe("Deleted with the job");
    expect(videoOutputText({})).toBe("Unknown");
  });
});
