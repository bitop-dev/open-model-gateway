"""`spark-env.py up --mode replicas` and `check` (#601): manifests, ordering,
secret handling, refusals and the sign-in walk. No cluster, registry, issuer
or external network access."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


spark = load("spark_env", ROOT / "scripts" / "spark-env.py")
replicas = load("spark_replicas", ROOT / "scripts" / "spark_replicas.py")
SHA = "0123456789abcdef" * 2 + "01234567"
DIGEST = "sha256:" + "ab" * 32
IMAGE = f"ghcr.io/bitop-dev/open-model-gateway@{DIGEST}"
NAMESPACE = "coder-03-omg-r601"
CONFIG = Path("/k/spark.yaml")


class Cluster:
    """A fake runner: answers the reads `deploy` makes and records every call."""

    def __init__(self, can_create_policy=False):
        self.calls = []
        self.can_create_policy = can_create_policy

    def __call__(self, command, stdin=None, capture=False):
        self.calls.append((command, stdin))
        text = " ".join(command)
        if "config view" in text:
            return json.dumps({"clusters": [{"cluster": {"server": "https://bitop-spark.taila06ed.ts.net:6443"}}],
                               "contexts": [{"context": {"namespace": "coder-03"}}]})
        if "service/omg-issuer" in text and "clusterIP" in text:
            return "10.43.0.10"
        if "service/omg-mock-upstream" in text and "clusterIP" in text:
            return "10.43.0.11"
        if "auth can-i" in text:
            if not self.can_create_policy:
                raise spark.SparkError("kubectl auth can-i failed (exit 1): no")
            return "yes"
        if "get job/" in text:
            return json.dumps({"status": {"conditions": [{"type": "Complete", "status": "True"}]}})
        return ""

    def commands(self):
        return [" ".join(c) for c, _ in self.calls]

    def index(self, needle, data=None):
        for i, (command, stdin) in enumerate(self.calls):
            if needle in " ".join(command) and (data is None or (stdin and data in stdin)):
                return i
        raise AssertionError(f"no call {needle!r} ({data!r}) in {self.commands()}")


def fake_pki(directory, host, runner=None, path=None):
    return ("-----BEGIN CERTIFICATE-----\nCA\n-----END CERTIFICATE-----\n",
            "-----BEGIN CERTIFICATE-----\nLEAF\n-----END CERTIFICATE-----\n",
            "-----BEGIN PRIVATE KEY-----\nSECRETKEYMATERIAL\n-----END PRIVATE KEY-----\n")


class Deploy(unittest.TestCase):
    def deploy(self, cluster):
        env = spark.Spark(CONFIG, cluster)
        env._base = "coder-03"
        resolved = spark.Ref("my-branch", SHA, False)
        with mock.patch.object(replicas, "generate_pki", fake_pki), contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            policy = replicas.Replicas(spark, env).deploy(NAMESPACE, resolved, IMAGE, DIGEST, "/src")
        return policy

    def test_the_order_the_invariants_require(self):
        cluster = Cluster()
        self.deploy(cluster)
        order = [cluster.index("create -f -", "omg-migrator-role"), cluster.index("create -f -", "omg-oidc"),
                 cluster.index("create configmap omg-spark-env"), cluster.index("create configmap omg-replicas-scripts"),
                 cluster.index("apply -f -", "omg-issuer"), cluster.index("apply -f -", "omg-mock-upstream"),
                 cluster.index("rollout status deployment/omg-issuer"), cluster.index("install omg"),
                 cluster.index("apply -f -", "provision-user"), cluster.index("get job/omg-first-admin")]
        self.assertEqual(order, sorted(order), cluster.commands())
        install = cluster.commands()[cluster.index("install omg")]
        self.assertIn("--wait-for-jobs", install)
        self.assertIn(f"image.digest={DIGEST}", install)
        self.assertNotIn("image.tag", install)
        self.assertIn("deploy/spark/replicas/values.yaml", install)

    def test_secrets_are_created_over_stdin_and_never_in_a_command_line(self):
        cluster = Cluster()
        self.deploy(cluster)
        listing = json.loads(cluster.calls[cluster.index("create -f -", "omg-oidc")][1])
        values = {item["metadata"]["name"]: item.get("stringData") or item.get("data") for item in listing["items"]}
        client_secret = values["omg-oidc"]["GATEWAY_OIDC_CLIENT_SECRET"]
        passwords = list(values["omg-issuer-users"].values())
        key = values["omg-issuer-tls"]["tls.key"]
        self.assertEqual(listing["items"][0]["type"], "kubernetes.io/tls")
        for value in [client_secret, *passwords, "SECRETKEYMATERIAL"]:
            self.assertGreaterEqual(len(value), 16)
            self.assertFalse(any(value in " ".join(command) for command, _ in cluster.calls), "a secret in a command line")
            self.assertFalse([1 for command, stdin in cluster.calls if "apply" in command and stdin and value in stdin], "a secret applied")
        self.assertIn("SECRETKEYMATERIAL", key)
        # Keycloak's master admin exists from the start, from a generated
        # password in its own Secret that the issuer reads by reference.
        bootstrap = values["omg-issuer-bootstrap"]["password"]
        self.assertGreaterEqual(len(bootstrap), 24)
        self.assertFalse(any(bootstrap in " ".join(command) or (stdin and "apply" in command and bootstrap in stdin)
                             for command, stdin in cluster.calls))
        issuer = cluster.calls[cluster.index("apply -f -", "omg-issuer")][1]
        self.assertIn("KC_BOOTSTRAP_ADMIN_PASSWORD, valueFrom: {secretKeyRef: {name: omg-issuer-bootstrap", issuer)
        self.assertIn("[redacted]", spark.redact(bootstrap))
        # The realm carries the same generated values; nothing else does.
        realm = json.loads(values["omg-issuer-realm"]["realm.json"])
        self.assertEqual(realm["clients"][0]["secret"], client_secret)
        self.assertEqual(sorted(u["credentials"][0]["value"] for u in realm["users"]), sorted(passwords))
        self.assertEqual(len(set(passwords + [client_secret])), 3, "distinct")
        self.assertIn("redacted", spark.redact(f"the secret {client_secret} and {passwords[0]}"))
        self.assertNotIn(client_secret, spark.redact(f"x {client_secret} y"))

    def test_every_image_is_pinned_by_digest(self):
        cluster = Cluster()
        self.deploy(cluster)
        images = []
        for command, stdin in cluster.calls:
            if "apply" in command and stdin:
                images += re.findall(r"image: (\S+)", stdin)
        self.assertIn(IMAGE, images)
        self.assertTrue(any(i.startswith("quay.io/keycloak/keycloak:26.8.1@sha256:") for i in images), images)
        self.assertTrue(images and all(re.search(r"@sha256:[0-9a-f]{64}$", i) for i in images), images)
        values = (ROOT / "deploy/spark/replicas/values.yaml").read_text()
        self.assertTrue(all("@sha256:" in i for i in re.findall(r"image: (\S+)", values)))

    def test_the_issuer_and_the_gateway_are_wired_to_each_other(self):
        cluster = Cluster()
        self.deploy(cluster)
        issuer = cluster.calls[cluster.index("apply -f -", "omg-issuer")][1]
        host = f"omg-issuer.{NAMESPACE}.svc.cluster.local"
        self.assertIn(f"--hostname=https://{host}:8443", issuer)
        self.assertIn("--http-enabled=false", issuer)
        self.assertNotIn("__", issuer)
        listing = json.loads(cluster.calls[cluster.index("create -f -", "omg-oidc")][1])
        oidc = next(i["stringData"] for i in listing["items"] if i["metadata"]["name"] == "omg-oidc")
        self.assertEqual(oidc["GATEWAY_OIDC_ISSUER"], f"https://{host}:8443/realms/omg")
        install = cluster.commands()[cluster.index("install omg")]
        self.assertIn('gateway.env.GATEWAY_EGRESS_PRIVATE_ALLOW="10.43.0.10/32"', install)
        self.assertIn(f'gateway.env.GATEWAY_PUBLIC_URL="https://omg.{NAMESPACE}.invalid"', install)
        upstreams = re.search(r'gateway\.env\.GATEWAY_LOCAL_UPSTREAMS=("[^ ]+")', install).group(1)
        self.assertEqual(json.loads(json.loads(upstreams)), [{"endpoint": "http://10.43.0.11:8000/v1", "addresses": ["10.43.0.11"]}])
        job = cluster.calls[cluster.index("apply -f -", "provision-user")][1]
        self.assertIn("--platform-admin", job)
        self.assertIn(replicas.ADMIN_EMAIL, job)
        self.assertIn("omg-db-rw:5432", job, "trusted access: the migrator, directly")

    def test_the_chart_network_policy_follows_the_platform_permission(self):
        for allowed in (False, True):
            cluster = Cluster(can_create_policy=allowed)
            self.assertEqual(self.deploy(cluster), allowed)
            install = cluster.commands()[cluster.index("install omg")]
            self.assertIn(f"networkPolicy.enabled={json.dumps(allowed)}", install)
            self.assertEqual("networkPolicy.ingress.fromNamespace" in install, allowed)

    def test_the_deployed_policy_admits_the_check_and_reaches_the_issuer_and_the_model(self):
        values = replicas.helm_values(NAMESPACE, "10.43.0.10", "10.43.0.11", True)
        self.assertEqual(values["networkPolicy.ingress.fromNamespace"], NAMESPACE)
        self.assertEqual(values["networkPolicy.ingress.fromPodLabels"], {"app.kubernetes.io/name": replicas.ALLOWED_CLIENT})
        egress = {(rule["to"][0]["podSelector"]["matchLabels"]["app.kubernetes.io/name"], rule["ports"][0]["port"])
                  for rule in values["networkPolicy.egress.extraEgress"]}
        self.assertEqual(egress, {("omg-issuer", 8443), ("omg-mock-upstream", 8000)})
        self.assertNotEqual(replicas.ALLOWED_CLIENT, replicas.REFUSED_CLIENT)
        load = (ROOT / "deploy/spark/replicas/load-pod.yaml").read_text()
        self.assertIn(f"app.kubernetes.io/name: {replicas.ALLOWED_CLIENT}", load)
        self.assertNotIn("networkPolicy.ingress.fromNamespace", replicas.helm_values(NAMESPACE, "10.43.0.10", "10.43.0.11", False))

    def test_the_spark_values_keep_the_gateway_off_any_ingress(self):
        values = (ROOT / "deploy/spark/replicas/values.yaml").read_text()
        self.assertIn("service: {type: ClusterIP}", values)
        self.assertIn("ingress: {enabled: false}", values)


class Pieces(unittest.TestCase):
    def test_placeholders_are_filled_strictly(self):
        self.assertEqual(replicas.fill("a __X__ b", {"X": "v1.2"}), "a v1.2 b")
        with self.assertRaises(ValueError):
            replicas.fill("a b", {"X": "v"})
        with self.assertRaises(ValueError):
            replicas.fill("__X__ __Y__", {"X": "v"})
        for hostile in ("a b", "x\ny", "$(rm)", "a;b", ""):
            with self.assertRaises(ValueError, msg=hostile):
                replicas.fill("__X__", {"X": hostile})

    def test_service_addresses_must_be_ipv4(self):
        self.assertEqual(replicas.ip("10.43.0.1"), "10.43.0.1")
        for bad in ("", "None", "omg-issuer", "10.43.0", "10.43.0.1/32"):
            with self.assertRaises(ValueError):
                replicas.ip(bad)

    def test_the_realm_is_one_confidential_pkce_client_with_a_groups_claim(self):
        realm = replicas.realm("https://omg.x.invalid/api/v1/auth/callback", "client-secret-value", ["pw-admin", "pw-unentitled"])
        client = realm["clients"][0]
        self.assertEqual(client["redirectUris"], ["https://omg.x.invalid/api/v1/auth/callback"])
        self.assertFalse(client["publicClient"])
        self.assertFalse(client["directAccessGrantsEnabled"])
        self.assertFalse(client["implicitFlowEnabled"])
        self.assertEqual(client["attributes"]["pkce.code.challenge.method"], "S256")
        mapper = client["protocolMappers"][0]
        self.assertEqual((mapper["protocolMapper"], mapper["config"]["claim.name"], mapper["config"]["id.token.claim"]),
                         ("oidc-group-membership-mapper", "groups", "true"))
        self.assertEqual([u["email"] for u in realm["users"]], [replicas.ADMIN_EMAIL, replicas.UNENTITLED_EMAIL])
        self.assertTrue(all(u["emailVerified"] and u["groups"] == ["/omg-users"] and not u["credentials"][0]["temporary"] for u in realm["users"]))
        self.assertEqual(realm["sslRequired"], "all")

    def test_forwards_bind_loopback_only(self):
        env = spark.Spark(CONFIG, Cluster())
        env._base = "coder-03"
        for command in replicas.Replicas(spark, env).forwards(NAMESPACE):
            text = " ".join(command)
            self.assertIn("--address 127.0.0.1", text)
            self.assertIn(f"-n {NAMESPACE}", text)
            self.assertIn("--kubeconfig /k/spark.yaml", text)
        targets = [c[c.index("port-forward") + 3:] for c in replicas.Replicas(spark, env).forwards(NAMESPACE)]
        self.assertEqual(targets, [["service/omg", "3000:8080"], ["service/omg-issuer", "18443:8443"]])

    def test_creds_never_print_a_password(self):
        env = spark.Spark(CONFIG, Cluster())
        env._base = "coder-03"
        text = replicas.Replicas(spark, env).creds_text(NAMESPACE)
        self.assertIn("never prints them", text)
        self.assertIn("omg-issuer-users", text)
        self.assertNotRegex(text, r"password\s*[:=]")

    def test_check_refuses_a_demo_environment(self):
        cluster = Cluster()
        env = spark.Spark(CONFIG, cluster)
        env._base = "coder-03"
        env.environment = lambda name: f"coder-03-omg-{name}"
        env.mode = lambda namespace: "demo"
        with self.assertRaisesRegex(spark.SparkError, "demo environment"):
            env.check("d599")

    def test_the_check_summary_is_private_and_redacted(self):
        cluster = Cluster()
        env = spark.Spark(CONFIG, cluster)
        env._base = "coder-03"
        env.environment = lambda name: f"coder-03-omg-{name}"
        env.mode = lambda namespace: "replicas"
        env.kubectl = lambda *args, **kwargs: "abc123"
        planted = "omg_sk_" + "x" * 24

        class Check:
            def __init__(self, *args):
                pass

            def run(self):
                spark._REGISTERED_SECRETS.append(planted)
                return {"seconds": 1, "steps": [{"step": 1, "detail": planted}], "passed": True}

        module = mock.Mock(Check=Check, quota=lambda *args: {"pods": "7"})
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(spark, "replicas_module", return_value=module), \
                mock.patch.dict(sys.modules, {spark.__name__: spark}), \
                contextlib.redirect_stdout(io.StringIO()):
            path = Path(directory) / "summary.json"
            path.write_text("old")
            path.chmod(0o644)
            env.check("r601", str(path))
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            text = path.read_text()
        self.assertNotIn(planted, text)
        self.assertEqual(json.loads(text)["quota_used"], {"pods": "7"})

    def test_the_mode_is_validated_before_anything_runs(self):
        cluster = Cluster()
        env = spark.Spark(CONFIG, cluster)
        with self.assertRaisesRegex(spark.SparkError, "unknown mode"):
            env.up("r601", "my-branch", "multi")
        self.assertEqual(cluster.calls, [])

    @unittest.skipUnless(shutil.which("openssl"), "openssl is not installed")
    def test_the_generated_pki_is_a_ca_and_a_server_certificate_for_the_issuer(self):
        seen = []

        def runner(command, **kwargs):
            seen.append(command)
            return subprocess.run(command, **kwargs)

        host = f"omg-issuer.{NAMESPACE}.svc.cluster.local"
        with tempfile.TemporaryDirectory() as directory:
            ca, leaf, key = replicas.generate_pki(directory, host, runner)
            Path(directory, "check-ca.pem").write_text(ca)
            Path(directory, "check-leaf.pem").write_text(leaf)
            verified = subprocess.run(["openssl", "verify", "-CAfile", f"{directory}/check-ca.pem", "-verify_hostname", host,
                                       "-purpose", "sslserver", f"{directory}/check-leaf.pem"], capture_output=True, text=True)
            text = subprocess.run(["openssl", "x509", "-noout", "-text", "-in", f"{directory}/check-ca.pem"], capture_output=True, text=True).stdout
        self.assertEqual(verified.returncode, 0, verified.stdout + verified.stderr)
        self.assertIn("CA:TRUE", text)
        self.assertIn("PRIVATE KEY", key)
        body = "".join(key.splitlines()[1:-1])
        self.assertFalse(any(body in " ".join(c) for c in seen), "key material in an argument")


class SignIn(unittest.TestCase):
    """The sign-in walk against canned gateway and issuer responses."""

    def walk(self, issuer_answer, gateway_callback=(303, [("set-cookie", "omg_session=s; Secure"), ("set-cookie", "omg_csrf=c")])):
        namespace = NAMESPACE
        host = f"https://omg-issuer.{namespace}.svc.cluster.local:8443"
        gateway = mock.Mock()
        gateway.request.side_effect = [
            (303, {"location": f"{host}/realms/omg/protocol/openid-connect/auth?state=s1&nonce=n1"}, [("set-cookie", "omg_oidc=o; Secure")], b""),
            (gateway_callback[0], {}, gateway_callback[1], b""),
        ]
        issuer = mock.Mock()
        form = f'<form id="kc-form-login" action="{host}/realms/omg/login-actions/authenticate?session_code=a&amp;tab_id=b">'
        issuer.request.side_effect = [(200, {}, [("set-cookie", "AUTH_SESSION_ID=x")], form.encode()), issuer_answer]
        browser = replicas.Browser(gateway, issuer, namespace)
        return browser.sign_in("admin", "pw"), gateway, issuer, browser

    def test_a_good_password_signs_in_through_the_issuer(self):
        callback = f"https://omg.{NAMESPACE}.invalid/api/v1/auth/callback?state=s1&code=k"
        outcome, gateway, issuer, browser = self.walk((302, {"location": callback}, [], b""))
        self.assertEqual(outcome, ("signed-in", 303))
        post = issuer.request.call_args_list[1]
        self.assertEqual(post.args[1], "/realms/omg/login-actions/authenticate?session_code=a&tab_id=b")
        self.assertIn("password=pw", post.args[3])
        self.assertIn("AUTH_SESSION_ID=x", post.args[2]["cookie"])
        self.assertEqual(gateway.request.call_args_list[1].args[1], "/api/v1/auth/callback?state=s1&code=k")
        self.assertIn("omg_oidc=o", gateway.request.call_args_list[1].args[2]["cookie"])
        self.assertEqual(browser.cookies["omg_session"], "s")

    def test_a_wrong_password_stops_at_the_issuer(self):
        again = b'<html>Invalid username or password.<form id="kc-form-login" action="x"></html>'
        outcome, gateway, _, _ = self.walk((200, {}, [], again))
        self.assertEqual(outcome, ("issuer-refused", 200))
        self.assertEqual(gateway.request.call_count, 1, "the callback is never called")

    def test_an_issuer_error_is_not_a_refusal(self):
        for answer in ((500, {}, [], b"oops"), (200, {}, [], b"<html>no form</html>")):
            outcome, _, _, _ = self.walk(answer)
            self.assertEqual(outcome[0], "issuer-error")

    def test_a_cleared_session_cookie_is_no_sign_in(self):
        callback = f"https://omg.{NAMESPACE}.invalid/api/v1/auth/callback?state=s1&code=k"
        outcome, _, _, _ = self.walk((302, {"location": callback}, [], b""),
                                     gateway_callback=(303, [("set-cookie", "omg_session=; Max-Age=0")]))
        self.assertEqual(outcome, ("gateway-refused", 303))

    def test_a_redirect_elsewhere_is_never_followed(self):
        outcome, gateway, _, _ = self.walk((302, {"location": "https://elsewhere.example/api/v1/auth/callback?code=k"}, [], b""))
        self.assertEqual(outcome[0], "redirect")
        self.assertEqual(gateway.request.call_count, 1)

    def test_an_unentitled_person_gets_no_session(self):
        callback = f"https://omg.{NAMESPACE}.invalid/api/v1/auth/callback?state=s1&code=k"
        outcome, _, _, browser = self.walk((302, {"location": callback}, [], b""), gateway_callback=(403, []))
        self.assertEqual(outcome, ("gateway-refused", 403))
        self.assertNotIn("omg_session", browser.cookies)


class CheckPieces(unittest.TestCase):
    """Parts of `check` that decide PASS or FAIL, against canned answers."""

    def check(self, kubectl=None):
        env = spark.Spark(CONFIG, Cluster())
        env._base = "coder-03"
        check = replicas.Check(spark, env, NAMESPACE)
        if kubectl is not None:
            check.kubectl = kubectl
        return check

    def test_an_eviction_is_refused_only_for_the_budget(self):
        def answer(stderr):
            def kubectl(args, **kwargs):
                raise spark.SparkError(f"kubectl create --raw /api/v1/namespaces/coder-03-omg-e429/pods/omg-429/eviction "
                                       f"-n coder-03-omg-e429 failed (exit 1): {stderr}")
            return kubectl
        cases = {
            "Error from server (InternalError): an error on the server": "error",
            "Error from server (TooManyRequests): Cannot evict pod as it would violate the pod's disruption budget.": "refused",
            'Error from server (Forbidden): pods "omg-429" is forbidden: User "u" cannot create resource "pods/eviction"': "forbidden",
        }
        for stderr, expected in cases.items():
            self.assertEqual(self.check(answer(stderr)).evict("omg-429"), expected, stderr)

    def test_the_lease_step_kills_only_a_running_replica_that_holds_the_lease(self):
        holder = "5f0c9a52-4c1e-4c8e-9a3e-0d9b2f4b7a10"
        check = self.check(lambda args, **kwargs: "")
        pods = [{"metadata": {"name": "omg-a"}}, {"metadata": {"name": "omg-b"}}]
        check.gateway_pods = lambda: pods
        check.forward = lambda target, ports: target
        check.stop = lambda child: None
        held = {"pod/omg-a": b"", "pod/omg-b": b""}

        class Http:
            def __init__(self, port, child):
                self.child = child

            def request(self, method, path):
                return 200, {}, [], held[self.child]
        rows = {"value": ""}
        check.sql = lambda statement: rows["value"]
        with mock.patch.object(replicas, "Http", Http):
            # No current term, then a term no running replica shows (a stopping
            # pod still holds it), then omg-b's own metrics show it held.
            self.assertEqual(check.lease_owner("metrics"), "free")
            rows["value"] = f"{holder} 7"
            held["pod/omg-a"] = b'gateway_work_leases_held{lease="metrics"} 0\n'
            self.assertEqual(check.lease_owner("metrics"), "elsewhere")
            held["pod/omg-b"] = b'gateway_work_leases_held{lease="metrics"} 1\n'
            self.assertEqual(check.lease_owner("metrics"), (holder, "7", "omg-b"))
            rows["value"] = "not-a-uuid 7"
            self.assertEqual(check.lease_owner("metrics"), "invalid")

    def test_the_lease_step_waits_for_a_running_holder_within_its_bound(self):
        check = self.check()
        owner = ("5f0c9a52-4c1e-4c8e-9a3e-0d9b2f4b7a10", "7", "omg-b")
        clock = {"now": 0.0}
        sleeps = []

        def sleep(seconds):
            sleeps.append(seconds)
            clock["now"] += seconds
        with mock.patch.object(replicas.time, "monotonic", lambda: clock["now"]), \
                mock.patch.object(replicas.time, "sleep", sleep):
            # A stopping pod holds the lease for a while: the step waits for
            # the event, not a fixed time.
            answers = iter(["elsewhere", "free", "elsewhere", owner])
            check.lease_owner = lambda lease: next(answers)
            self.assertEqual(check.wait_for_lease_owner("metrics"), owner)
            self.assertEqual(sleeps, [2, 2, 2])
            # Never past the bound: the last answer, and the step fails
            # without deleting a pod.
            sleeps.clear()
            clock["now"] = 0.0
            check.lease_owner = lambda lease: "elsewhere"
            self.assertEqual(check.wait_for_lease_owner("metrics"), "elsewhere")
            self.assertLessEqual(sum(sleeps), replicas.LEASE_OWNER_WAIT + 2)
            self.assertGreaterEqual(clock["now"], replicas.LEASE_OWNER_WAIT)
            deleted = []
            check.kubectl = lambda args, **kwargs: deleted.append(args)
            with contextlib.redirect_stdout(io.StringIO()):
                check.lease_step()
        self.assertEqual(deleted, [])
        self.assertEqual(check.results[-1]["result"], "fail")
        self.assertIn("no running replica's metrics show the metrics lease after 90 s", check.results[-1]["detail"])
        # An invalid row fails at once.
        check.lease_owner = lambda lease: "invalid"
        with contextlib.redirect_stdout(io.StringIO()):
            check.lease_step()
        self.assertEqual(check.results[-1]["detail"], "the lease row is not a holder UUID and an epoch")

    def test_only_the_listen_sessions_may_bypass_the_pooler(self):
        def run(rows):
            check = self.check(lambda args, **kwargs: "10.42.0.5 10.42.0.6" if "poolerName" in " ".join(args) else "")
            check.gateway_pods = lambda: [{"status": {"podIP": "10.42.0.7"}}, {"status": {"podIP": "10.42.0.8"}}]
            check.sql = lambda statement: "\n".join(rows)
            with contextlib.redirect_stdout(io.StringIO()):
                check.pooler_step()
            return check.results[-1]["result"]
        listen = ["10.42.0.7|listen", "10.42.0.8|listen"]
        self.assertEqual(run(["10.42.0.5|other", "10.42.0.6|other", *listen]), "pass")
        self.assertEqual(run(["10.42.0.5|other", *listen, "10.42.0.7|other"]), "fail", "a direct pool session")
        self.assertEqual(run(["10.42.0.5|other", "10.42.0.7|listen"]), "fail", "a replica without its LISTEN session")
        self.assertEqual(run([*listen]), "fail", "nothing through the pooler")

    def test_a_failed_listing_is_never_a_vanished_model(self):
        client = mock.Mock()
        client.request.return_value = (503, {}, [], b"")
        self.assertEqual(replicas.models(client, "k"), (503, None))
        client.request.return_value = (200, {}, [], b'{"data": []}')
        self.assertEqual(replicas.models(client, "k"), (200, set()))

    def test_an_error_fails_its_step_and_the_check_goes_on(self):
        check = self.check()
        planted = "omg_sk_" + "y" * 24
        spark._REGISTERED_SECRETS.append(planted)

        def broken():
            raise spark.SparkError(f"kubectl exec failed (exit 1): User system:serviceaccount:coder-03:x token {planted}")
        with contextlib.redirect_stdout(io.StringIO()) as out:
            self.assertIsNone(check.guarded(4, "lease-takeover", broken))
            # Any error at all is that step's FAIL, never the end of the check.
            self.assertIsNone(check.guarded(6, "network-policy", lambda: [][-1]))
            self.assertEqual(check.guarded(5, "x", lambda: 7), 7)
        self.assertEqual([(r["step"], r["result"]) for r in check.results], [(4, "fail"), (6, "fail")])
        # The summary holds the error's type only; the terminal its redacted text.
        self.assertEqual(check.results[0]["detail"], "error (SparkError; its text is in the terminal output only)")
        self.assertEqual(check.results[1]["detail"], "error (IndexError; its text is in the terminal output only)")
        self.assertNotIn("serviceaccount", json.dumps(check.results))
        self.assertIn("token [redacted]", out.getvalue())
        self.assertNotIn(planted, out.getvalue())

    def test_nothing_is_sent_through_a_forward_that_stopped(self):
        child = mock.Mock()
        child.poll.return_value = 1
        with mock.patch.object(replicas.http.client, "HTTPConnection") as connection:
            with self.assertRaisesRegex(ConnectionError, "has stopped"):
                replicas.Http(3101, child=child).request("PATCH", "/x", {"cookie": "omg_session=s"})
        connection.assert_not_called()

    def network_policy(self, outcomes, others=()):
        chart = ["networkpolicy.networking.k8s.io/omg-default-deny", "networkpolicy.networking.k8s.io/omg-ingress"]

        def kubectl(args, **kwargs):
            text = " ".join(args)
            if text.startswith("get networkpolicy -l"):
                return "\n".join(chart)
            if text.startswith("get networkpolicy"):
                return "\n".join([*chart, *others])
            if text.startswith("logs pod/"):
                return json.dumps(outcomes[args[1].split("/")[1]])
            return ""
        check = self.check(kubectl)
        with contextlib.redirect_stdout(io.StringIO()):
            check.network_policy_step("k")
        return [(r["name"], r["result"]) for r in check.results]

    def test_the_policy_step_needs_a_working_network_and_a_real_refusal(self):
        good = {"omg-check-allowed": {"control": 200, "probe": 200}, "omg-check-refused": {"control": 200, "probe": "timeout"}}
        self.assertEqual(self.network_policy(good), [("network-policy-allows-a-client", "pass"), ("network-policy-refuses-another-pod", "pass")])
        # A probe pod without a network proves no refusal.
        dead = {**good, "omg-check-refused": {"control": "timeout", "probe": "timeout"}}
        self.assertEqual(self.network_policy(dead)[1], ("network-policy-refuses-another-pod", "fail"))
        # Admitted anyway: a fail, unless another policy admits it (policies add up).
        through = {**good, "omg-check-refused": {"control": 200, "probe": 200}}
        self.assertEqual(self.network_policy(through)[1], ("network-policy-refuses-another-pod", "fail"))
        self.assertEqual(self.network_policy(through, ["networkpolicy.networking.k8s.io/platform-same-namespace"])[1],
                         ("network-policy-refuses-another-pod", "skip"))

    def test_a_forward_counts_only_when_its_own_kubectl_listens(self):
        check = self.check()
        check.spark.port_free = lambda port: True
        script = "import sys; print(sys.argv[1], flush=True); import time; time.sleep(30)"
        for line, opens in (("Forwarding from 127.0.0.1:3101 -> 8080", True),
                            ("Unable to listen on port 3101: address already in use", False)):
            check.children = []
            command = [sys.executable, "-c", script, line]
            with mock.patch.object(replicas.Replicas, "forward_command", return_value=command):
                if opens:
                    child = check.forward("pod/omg-a", ["3101:8080"])
                    self.assertIsNone(child.poll())
                    check.stop(child)
                else:
                    with mock.patch.object(replicas.threading.Event, "wait", lambda self, timeout=None: self.is_set()):
                        with self.assertRaisesRegex(spark.SparkError, "did not open"):
                            check.forward("pod/omg-a", ["3101:8080"])
                    self.assertIsNotNone(check.children[0].poll(), "the child is stopped")

    def test_a_port_in_use_is_refused_before_any_forward(self):
        check = self.check()
        check.spark.port_free = lambda port: False
        with mock.patch.object(replicas.subprocess, "Popen") as popen, self.assertRaisesRegex(spark.SparkError, "in use"):
            check.forward("pod/omg-a", ["3101:8080"])
        popen.assert_not_called()

    def test_openssl_gets_no_ambient_configuration(self):
        seen = []

        def runner(command, **kwargs):
            seen.append(kwargs["env"])
            return mock.Mock(returncode=1)
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(replicas.os.environ, {"OPENSSL_CONF": "/evil.cnf"}):
            with self.assertRaises(RuntimeError):
                replicas.generate_pki(directory, "h", runner, path="/usr/bin")
        self.assertEqual(seen, [{"PATH": "/usr/bin", "OPENSSL_CONF": replicas.os.devnull, "LC_ALL": "C"}])


if __name__ == "__main__":
    unittest.main()
