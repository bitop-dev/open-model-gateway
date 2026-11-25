"""Access reviews: start a small campaign on a throwaway Team, decide, export, close; then a schedule on the
same Team: create, read, pause, resume and delete (it is never due during the run)."""
from ..client import uuid_of
from ..registry import journey
from .common import rows


# A start no valid request matches, so a refusal that has regressed cannot open a campaign.
BLANK = {"name": " ", "subjects": []}


@journey("access-reviews")
def access_reviews(t):
    anon = t.anonymous()
    operator, auditor, alex, blair = (t.session(p) for p in ("operator", "auditor", "alex", "blair"))
    ws = t.team_workspace(owner="alex")
    t.add_member(ws, "blair", "member")
    base = "/api/v1/platform/access-reviews"

    # Only this run's campaign is ever closed here. One left open by a run that was killed counts towards the
    # limit of 20 open campaigns; close it from the dashboard (its name starts with journeys-).
    t.expect("review.anonymous-list-refused", anon.get(base), 401)
    t.expect("review.member-list-refused", alex.get(base), 403)
    t.expect("review.member-cannot-start", alex.post(base, BLANK), 403)
    t.expect("review.auditor-cannot-start", auditor.post(base, BLANK), 403)
    t.expect("review.no-csrf-refused", operator.post(base, BLANK, csrf=False), 403)
    t.expect("review.blank-name-refused", operator.post(base, {"name": "  ", "subjects": ["workspace_memberships"], "workspace_ids": [ws]}), 400)
    t.expect("review.no-subjects-refused", operator.post(base, {"name": t.label("review"), "subjects": [], "workspace_ids": [ws]}), 400)
    t.expect("review.unknown-subject-refused", operator.post(base, {"name": t.label("review"), "subjects": ["everything"], "workspace_ids": [ws]}), 400)

    started = operator.post(base, {"name": t.label("review"), "subjects": ["workspace_memberships"], "workspace_ids": [ws], "due_in_days": 1})
    t.expect("review.start", started, 200)
    cid = started.id()
    if not cid:
        t.rec.fail("review.started-campaign-missing", "setup_failed")
        return
    # Closing twice is refused (409), which the undo accepts: it only has to leave the campaign closed.
    t.undo("close campaign", lambda: operator.post(f"{base}/{cid}/close"), gone=(409,))
    t.ok("review.counts-two-memberships", started.get("counts", "by_kind", "workspace_membership") == 2)
    t.expect("review.list-admin", operator.get(base), 200)
    t.ok("review.listed", any(c.get("id") == cid for c in rows(operator.get(f"{base}?limit=100")) or []))
    t.expect("review.list-auditor", auditor.get(base), 200)
    t.expect("review.detail-auditor", auditor.get(f"{base}/{cid}"), 200)

    items = rows(operator.get(f"{base}/{cid}/items?limit=100")) or []
    t.expect("review.items-admin", operator.get(f"{base}/{cid}/items"), 200)
    t.expect("review.items-auditor", auditor.get(f"{base}/{cid}/items"), 200)
    t.expect("review.items-member-refused", alex.get(f"{base}/{cid}/items"), 403)
    blair_item = uuid_of(next((i.get("id") for i in items if i.get("user_id") == t.user_id("blair")), None))
    alex_item = uuid_of(next((i.get("id") for i in items if i.get("user_id") == t.user_id("alex")), None))
    if not blair_item or not alex_item:
        t.rec.fail("review.snapshot-items-missing", "setup_failed")
        return

    # Reviewers: the workspace owner decides the member's item; nobody decides their own.
    inbox = alex.get("/api/v1/me/access-reviews?limit=200")
    t.expect("review.reviewer-inbox", inbox, 200)
    inbox_items = rows(inbox)
    t.ok("review.owner-sees-members-item", inbox_items is not None and any(i.get("id") == blair_item for i in inbox_items))
    t.ok("review.owner-does-not-see-own-item", inbox_items is not None and not any(i.get("id") == alex_item for i in inbox_items))
    t.expect("review.member-cannot-decide-own-item", blair.post(f"/api/v1/me/access-reviews/{blair_item}/decision", {"decision": "keep"}), 404)
    t.expect("review.owner-cannot-decide-own-item", alex.post(f"/api/v1/me/access-reviews/{alex_item}/decision", {"decision": "keep"}), 404)
    t.expect("review.decision-no-csrf-refused", alex.post(f"/api/v1/me/access-reviews/{blair_item}/decision", {"decision": "keep"}, csrf=False), 403)
    t.expect("review.unknown-decision-refused", alex.post(f"/api/v1/me/access-reviews/{blair_item}/decision", {"decision": "shrug"}), 400)
    flagged = alex.post(f"/api/v1/me/access-reviews/{blair_item}/decision", {"decision": "flag_idp"})
    t.expect("review.flag-requires-group-access", flagged, 409)
    t.ok("review.flag-reason", flagged.error_reason == "access_review_flag_requires_group")
    decided = alex.post(f"/api/v1/me/access-reviews/{blair_item}/decision", {"decision": "keep"})
    t.expect("review.decide-keep", decided, 200)
    t.ok("review.decision-recorded", decided.get("decision") == "keep")
    t.ok("review.counts-reflect-decision", operator.get(f"{base}/{cid}").get("counts", "by_decision", "keep") == 1)
    t.expect("review.clear-reviewer", operator.post(f"{base}/{cid}/items/{blair_item}/reviewer", {"user_id": None}), 200)
    t.expect("review.assign-by-member-refused", alex.post(f"{base}/{cid}/items/{blair_item}/reviewer", {"user_id": None}), 403)

    # Export.
    export = operator.get(f"{base}/{cid}/export")
    t.expect("review.export-admin", export, 200)
    t.ok("review.export-is-csv", export.header("content-type").startswith("text/csv") and export.body.startswith(b"item_id,"))
    t.ok("review.export-not-cached", "no-store" in export.header("cache-control"))
    t.expect("review.export-auditor", auditor.get(f"{base}/{cid}/export"), 200)
    t.expect("review.export-member-refused", alex.get(f"{base}/{cid}/export"), 403)
    t.expect("review.export-anonymous-refused", anon.get(f"{base}/{cid}/export"), 401)

    # Close.
    t.expect("review.close-by-auditor-refused", auditor.post(f"{base}/{cid}/close"), 403)
    t.expect("review.close-by-member-refused", alex.post(f"{base}/{cid}/close"), 403)
    t.expect("review.close", operator.post(f"{base}/{cid}/close"), 200)
    again = operator.post(f"{base}/{cid}/close")
    t.expect("review.close-twice-refused", again, 409)
    t.ok("review.close-twice-reason", again.error_reason == "access_review_closed")
    t.expect("review.decision-after-close-refused", alex.post(f"/api/v1/me/access-reviews/{blair_item}/decision", {"decision": "keep"}), 404)
    inbox_after = rows(alex.get("/api/v1/me/access-reviews?limit=200"))
    t.ok("review.closed-campaign-leaves-inbox", inbox_after is not None and not any(i.get("id") == blair_item for i in inbox_after))
    t.ok("review.detail-shows-closed", auditor.get(f"{base}/{cid}").get("closed_at") is not None)

    # Schedules (#481). The first run is the first period start after now, so this schedule never runs here.
    sched = "/api/v1/platform/access-review-schedules"
    schedule_body = {"name": t.label("schedule"), "subjects": ["workspace_memberships"], "workspace_ids": [ws], "cadence": "monthly", "start_day": 1}
    t.expect("schedule.anonymous-list-refused", anon.get(sched), 401)
    t.expect("schedule.member-list-refused", alex.get(sched), 403)
    t.expect("schedule.member-cannot-create", alex.post(sched, schedule_body), 403)
    t.expect("schedule.auditor-cannot-create", auditor.post(sched, schedule_body), 403)
    t.expect("schedule.monthly-start-month-refused", operator.post(sched, {**schedule_body, "start_month": 1}), 400)
    created = operator.post(sched, schedule_body)
    t.expect("schedule.create", created, 200)
    sid = created.id()
    if not sid:
        t.rec.fail("schedule.created-schedule-missing", "setup_failed")
        return
    # Deleting twice is refused (404), which the undo accepts: it only has to leave the schedule deleted.
    t.undo("delete schedule", lambda: operator.delete(f"{sched}/{sid}"), gone=(404,))
    t.ok("schedule.next-run-is-future", bool(created.get("next_run_at")) and created.get("last_run") is None)
    t.ok("schedule.listed", any(s.get("id") == sid for s in rows(auditor.get(f"{sched}?limit=200")) or []))
    t.expect("schedule.detail-auditor", auditor.get(f"{sched}/{sid}"), 200)
    t.expect("schedule.auditor-cannot-pause", auditor.patch(f"{sched}/{sid}", {"paused": True}), 403)
    paused = operator.patch(f"{sched}/{sid}", {"paused": True})
    t.expect("schedule.pause", paused, 200)
    t.ok("schedule.paused-has-no-next-run", paused.get("paused_at") is not None and paused.get("next_run_at") is None)
    resumed = operator.patch(f"{sched}/{sid}", {"paused": False})
    t.expect("schedule.resume", resumed, 200)
    t.ok("schedule.resumed-has-next-run", resumed.get("paused_at") is None and bool(resumed.get("next_run_at")))
    t.expect("schedule.member-cannot-delete", alex.delete(f"{sched}/{sid}"), 403)
    t.expect("schedule.delete", operator.delete(f"{sched}/{sid}"), 200)
    t.expect("schedule.deleted-detail-gone", operator.get(f"{sched}/{sid}"), 404)
