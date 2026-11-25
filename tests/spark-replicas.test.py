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


def fake_pki(directory, host, runner=None):
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
        outcome, gateway, _, _ = self.walk((200, {}, [], b"<html>Invalid username or password.</html>"))
        self.assertEqual(outcome[0], "issuer-refused")
        self.assertEqual(gateway.request.call_count, 1, "the callback is never called")

    def test_a_redirect_elsewhere_is_never_followed(self):
        outcome, gateway, _, _ = self.walk((302, {"location": "https://elsewhere.example/api/v1/auth/callback?code=k"}, [], b""))
        self.assertEqual(outcome[0], "redirect")
        self.assertEqual(gateway.request.call_count, 1)

    def test_an_unentitled_person_gets_no_session(self):
        callback = f"https://omg.{NAMESPACE}.invalid/api/v1/auth/callback?state=s1&code=k"
        outcome, _, _, browser = self.walk((302, {"location": callback}, [], b""), gateway_callback=(403, []))
        self.assertEqual(outcome, ("gateway-refused", 403))
        self.assertNotIn("omg_session", browser.cookies)


if __name__ == "__main__":
    unittest.main()
