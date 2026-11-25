"""Guardrail policy layers (#840) on a throwaway Team: an override, a local layer
that tightens it and adds a custom pattern, the refusals, and the read views.
The type defaults are read, never changed (they apply to every workspace)."""
from ..registry import journey

SECRETS = {"name": "Secrets", "protection": "secrets", "detector": {"kind": "pattern", "categories": ["secret"]}, "input": {"action": "monitor"}}
CUSTOM = {"name": "Code names", "protection": "custom", "detector": {"kind": "pattern", "categories": ["custom"], "patterns": ["JOURNEY-[0-9]{4}"]}, "input": {"action": "block"}}
MODERATION = {"name": "Moderation", "protection": "moderation", "detector": {"kind": "systemone", "categories": ["moderation"]}, "input": {"action": "block"}}


def first(items):
    """The first entry of a JSON list, or an empty dict."""
    return items[0] if isinstance(items, list) and items and isinstance(items[0], dict) else {}


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
    t.expect("guardrails.type-default-auditor-write-refused", auditor.put("/api/v1/platform/workspace-types/team/guardrails", {"rules": []}), 403)

    # The platform override, with a monitor rule a workspace may tighten.
    t.expect("guardrails.override-member-refused", alex.put(platform, {"rules": [SECRETS]}), 403)
    written = operator.put(platform, {"rules": [SECRETS]})
    t.expect("guardrails.override-write", written, 200)
    t.undo("remove guardrail override", lambda: operator.delete(platform))
    target = first(written.get("override", "rules")).get("id")
    t.ok("guardrails.override-replaces-type-default", written.get("platform", "source") == "workspace_override")
    t.ok("guardrails.not-enforced-yet", written.get("enforced") is False)
    t.expect("guardrails.override-auditor-read", auditor.get(platform), 200)

    # The local layer tightens, never loosens, and adds a custom pattern.
    t.undo("empty local guardrails", lambda: alex.put(local, {"rules": []}))
    loosen = alex.put(local, {"rules": [{"tightens": target, "input": {"action": "off"}}]})
    t.expect("guardrails.loosen-refused", loosen, 403)
    t.ok("guardrails.loosen-reason", loosen.error_reason == "guardrail_loosen_not_allowed")
    later = alex.put(local, {"rules": [MODERATION]})
    t.expect("guardrails.later-phase-refused", later, 400)
    t.ok("guardrails.later-phase-reason", later.error_reason == "guardrail_detector_unavailable")
    t.expect("guardrails.member-write-refused", blair.put(local, {"rules": []}), 403)
    tightened = alex.put(local, {"rules": [{"tightens": target, "input": {"action": "block"}}, CUSTOM]})
    t.expect("guardrails.local-write", tightened, 200)
    t.ok("guardrails.tightened-runs-stricter", first(tightened.get("effective")).get("input", {}).get("action") == "block")
    member = blair.get(local)
    t.expect("guardrails.member-read", member, 200)
    custom = [e for e in member.get("effective") or [] if isinstance(e, dict) and e.get("protection") == "custom"]
    t.ok("guardrails.member-sees-a-count", len(custom) == 1 and custom[0].get("detector", {}).get("pattern_count") == 1 and "patterns" not in custom[0].get("detector", {}) and "JOURNEY-" not in str(member.json()))
    t.ok("guardrails.member-cannot-edit", member.get("can_edit") is False)

    # Restoring the type default.
    reset = operator.delete(platform)
    t.expect("guardrails.override-reset", reset, 200)
    t.ok("guardrails.type-default-restored", reset.get("platform", "source") == "type_default")
