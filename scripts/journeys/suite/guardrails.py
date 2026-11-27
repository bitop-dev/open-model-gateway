"""Guardrail policy layers (#840) on a throwaway Team: an override, a local layer
that tightens it and adds a custom pattern, the refusals, and the read views;
then a personal workspace's local layer, which platform staff never see. The
type defaults are read, never changed (they apply to every workspace)."""
from ..client import uuid_of
from ..registry import journey

SECRETS = {"name": "Secrets", "protection": "secrets", "detector": {"kind": "pattern", "categories": ["secret"]}, "input": {"action": "monitor"}}
CUSTOM = {"name": "Code names", "protection": "custom", "detector": {"kind": "pattern", "categories": ["custom"], "patterns": ["JOURNEY-[0-9]{4}"]}, "input": {"action": "block"}}
MODERATION = {"name": "Moderation", "protection": "moderation", "detector": {"kind": "systemone", "categories": ["moderation"]}, "input": {"action": "block"}}


def first(items):
    """The first entry of a JSON list, or an empty dict."""
    return items[0] if isinstance(items, list) and items and isinstance(items[0], dict) else {}


def expected(session, path, key):
    """The layer at `path` as `session` reads it now (`key`: the view's field
    for that layer), as a write names it: its id and revision, or None."""
    found = session.get(path).get(key)
    if not isinstance(found, dict):
        return None
    rule_set_id, revision = uuid_of(found.get("id")), found.get("revision")
    return {"rule_set_id": rule_set_id, "revision": revision} if rule_set_id and isinstance(revision, int) else None


def put(session, path, key, body):
    """A layer write built from the layer as `session` reads it now."""
    return session.put(path, {**body, "expected": expected(session, path, key)})


def reset(session, path):
    """Remove the override at `path`, naming it as `session` reads it now."""
    found = expected(session, path, "override")
    query = f"?rule_set_id={found['rule_set_id']}&revision={found['revision']}" if found else ""
    return session.delete(path + query)


@journey("guardrails")
def guardrails(t):
    anon = t.anonymous()
    ws = t.team_workspace(owner="alex")
    operator, auditor, alex, blair = (t.session(p) for p in ("operator", "auditor", "alex", "blair"))
    t.add_member(ws, "blair", "member")
    local = f"/api/v1/workspaces/{ws}/guardrails"
    platform = f"/api/v1/platform/workspaces/{ws}/guardrails"

    t.expect("guardrails.anonymous-refused", anon.get(local), 401)
    t.expect("guardrails.detectors", auditor.get("/api/v1/platform/guardrails/detectors"), 200)
    t.expect("guardrails.detectors-member-refused", alex.get("/api/v1/platform/guardrails/detectors"), 403)
    t.expect("guardrails.type-default-read", auditor.get("/api/v1/platform/workspace-types/team/guardrails"), 200)
    t.expect("guardrails.type-default-auditor-write-refused", put(auditor, "/api/v1/platform/workspace-types/team/guardrails", "rule_set", {"local_only": False, "rules": []}), 403)

    # The platform override, with a monitor rule a workspace may tighten.
    t.expect("guardrails.override-member-refused", put(alex, platform, "override", {"local_only": False, "rules": [SECRETS]}), 403)
    # A PUT replaces the layer, so the egress restriction is stated every time.
    t.expect("guardrails.override-local-only-required", put(operator, platform, "override", {"rules": [SECRETS]}), 400)
    # A write names the layer it was built from: none here, so null.
    unnamed = operator.put(platform, {"local_only": False, "rules": [SECRETS]})
    t.expect("guardrails.expected-required", unnamed, 400)
    t.ok("guardrails.expected-required-reason", unnamed.error_reason == "guardrail_expected_required")
    written = put(operator, platform, "override", {"local_only": False, "rules": [SECRETS]})
    t.expect("guardrails.override-write", written, 200)
    t.undo("remove guardrail override", lambda: reset(operator, platform))
    # A write built from no layer, now that there is one, is refused.
    stale = operator.put(platform, {"local_only": False, "expected": None, "rules": [SECRETS]})
    t.expect("guardrails.stale-write-refused", stale, 409)
    t.ok("guardrails.stale-write-reason", stale.error_reason == "guardrail_layer_changed")
    target = first(written.get("override", "rules")).get("id")
    t.ok("guardrails.override-replaces-type-default", written.get("platform", "source") == "workspace_override")
    t.ok("guardrails.enforced", written.get("enforced") is True)
    t.ok("guardrails.inspected-workloads", written.get("inspected_workloads") == ["generation", "embeddings"])
    t.expect("guardrails.override-auditor-read", auditor.get(platform), 200)

    # The local layer tightens, never loosens, and adds a custom pattern.
    t.undo("empty local guardrails", lambda: put(alex, local, "local", {"rules": []}))
    loosen = put(alex, local, "local", {"rules": [{"tightens": target, "input": {"action": "off"}}]})
    t.expect("guardrails.loosen-refused", loosen, 403)
    t.ok("guardrails.loosen-reason", loosen.error_reason == "guardrail_loosen_not_allowed")
    later = put(alex, local, "local", {"rules": [MODERATION]})
    t.expect("guardrails.later-phase-refused", later, 400)
    t.ok("guardrails.later-phase-reason", later.error_reason == "guardrail_detector_unavailable")
    broken = put(alex, local, "local", {"rules": [{**CUSTOM, "detector": {**CUSTOM["detector"], "patterns": ["JOURNEY-(unclosed"]}}]})
    t.expect("guardrails.pattern-invalid-refused", broken, 400)
    t.ok("guardrails.pattern-invalid-reason", broken.error_reason == "guardrail_pattern_invalid" and "JOURNEY-" not in str(broken.json()))
    t.expect("guardrails.member-write-refused", put(blair, local, "local", {"rules": []}), 403)
    tightened = put(alex, local, "local", {"rules": [{"tightens": target, "input": {"action": "block"}}, CUSTOM]})
    t.expect("guardrails.local-write", tightened, 200)
    t.ok("guardrails.tightened-runs-stricter", first(tightened.get("effective")).get("input", {}).get("action") == "block")
    member = blair.get(local)
    t.expect("guardrails.member-read", member, 200)
    custom = [e for e in member.get("effective") or [] if isinstance(e, dict) and e.get("protection") == "custom"]
    t.ok("guardrails.member-sees-a-count", len(custom) == 1 and custom[0].get("detector", {}).get("pattern_count") == 1 and "patterns" not in custom[0].get("detector", {}) and "JOURNEY-" not in str(member.json()))
    t.ok("guardrails.member-cannot-edit", member.get("can_edit") is False)

    # Restoring the type default.
    # A reset names the override it removes; one that names none is refused.
    t.expect("guardrails.stale-reset-refused", operator.delete(platform), 409)
    restored = reset(operator, platform)
    t.expect("guardrails.override-reset", restored, 200)
    t.ok("guardrails.type-default-restored", restored.get("platform", "source") == "type_default")

    # A personal workspace's local layer is the owner's choice: staff see its
    # platform layers, never the local one or its patterns.
    mine = t.personal_workspace("blair")
    own = f"/api/v1/workspaces/{mine}/guardrails"
    t.undo("empty personal guardrails", lambda: put(blair, own, "local", {"rules": []}))
    t.expect("guardrails.personal-local-write", put(blair, own, "local", {"rules": [CUSTOM]}), 200)
    staff = operator.get(f"/api/v1/platform/workspaces/{mine}/guardrails")
    t.expect("guardrails.personal-platform-read", staff, 200)
    t.ok("guardrails.personal-local-hidden", staff.get("local") is None and staff.get("local_visible") is False and staff.get("effective") is None and "JOURNEY-" not in str(staff.json()))
    # The workspace itself is not visible to them, as on its policy route.
    t.expect("guardrails.personal-staff-refused", auditor.get(own), 404)
    t.expect("guardrails.personal-staff-policy-refused", auditor.get(f"/api/v1/workspaces/{mine}/policy"), 404)
