"""The second mode of scripts/spark-env.py: the chart as shipped (#601).

TEST ONLY. Loaded by spark-env.py, which owns every safety check (the Spark
kubeconfig only, the server-name check, the scrubbed child environment, the
<base>-omg-<name> namespace rule, the digest-pinned gateway image built from
the ref's commit, the arm64 check). This module only builds on its `Spark`.

`up <name> --ref <ref> --mode replicas` deploys the chart's own gateway
Deployment with two replicas, the CloudNativePG Cluster and Pooler, the
migrate Job, the Service and the PodDisruptionBudget, in production mode, and
the chart's NetworkPolicy when the platform lets the account create one. Sign-in
uses a per-environment Keycloak issuer in the namespace (deploy/spark/replicas)
with a generated client secret and generated user passwords, whose HTTPS
certificate is signed by a CA generated for the environment (the gateway trusts
it through GATEWAY_OIDC_CA_FILE). The CA's private key never leaves a
temporary directory, which is deleted at once. The issuer is reached only from
inside the namespace and through `kubectl port-forward` bound to 127.0.0.1.
The first admin is created by `provision-user --platform-admin` as a Job. A
free model server runs in the namespace.

`check <name>` runs the scripted rehearsal and prints one PASS, FAIL or SKIP
line per step; its summary holds statuses, counts and timings, never a secret.
"""
import http.client
import html
import json
import os
import re
import shlex
import secrets
import shutil
import socket
import ssl
import subprocess
import tempfile
import threading
import time
import urllib.parse
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy" / "spark" / "replicas"
REALM = "omg"
CLIENT_ID = "omg-gateway"
ADMIN_EMAIL = "admin@spark.invalid"
UNENTITLED_EMAIL = "unentitled@spark.invalid"
# Local ports of `forward` and `check` (loopback only).
GATEWAY_PORT = 3000
ISSUER_PORT = 18443
POD_PORTS = (3101, 3102)
METRICS_PORTS = (3201, 3202, 3203)
MODEL = "spark/mock"
FIRST_ADMIN_JOB = "omg-first-admin"


def issuer_host(namespace):
    return f"omg-issuer.{namespace}.svc.cluster.local"


def issuer_url(namespace):
    return f"https://{issuer_host(namespace)}:8443/realms/{REALM}"


def public_url(namespace):
    """The gateway's canonical HTTPS origin. Nothing serves it: the check and
    a browser reach the gateway through the loopback port-forward and present
    this origin, which only the gateway and the issuer's redirect URI use."""
    return f"https://omg.{namespace}.invalid"


def fill(text, values):
    """Substitute every `__NAME__` placeholder; each must occur and none may remain."""
    for name, value in values.items():
        marker = f"__{name}__"
        if marker not in text:
            raise ValueError(f"manifest has no {marker} placeholder")
        if not re.fullmatch(r"[A-Za-z0-9.:/@_-]+", value):
            raise ValueError(f"unusable value for {marker}")
        text = text.replace(marker, value)
    if re.search(r"__[A-Z_]+__", text):
        raise ValueError("manifest has an unfilled placeholder")
    return text


def generate_pki(directory, host, runner=subprocess.run, path=os.defpath):
    """A CA and the issuer's certificate (P-256, one year) with the `openssl`
    CLI in `directory`: returns (CA certificate, issuer certificate, issuer
    key) as PEM text. Arguments name files only; the CA key stays in the
    directory, which the caller deletes. openssl gets only PATH and an empty
    configuration: no ambient OPENSSL_CONF, engines or providers."""
    d = Path(directory)
    (d / "issuer.ext").write_text(
        f"subjectAltName=DNS:{host}\nbasicConstraints=critical,CA:FALSE\n"
        "keyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n")
    steps = [
        ["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
         "-keyout", str(d / "ca.key"), "-out", str(d / "ca.crt"), "-days", "365", "-subj", "/CN=omg Spark environment CA",
         "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign"],
        ["openssl", "req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
         "-keyout", str(d / "tls.key"), "-out", str(d / "tls.csr"), "-subj", "/CN=omg-issuer"],
        ["openssl", "x509", "-req", "-in", str(d / "tls.csr"), "-CA", str(d / "ca.crt"), "-CAkey", str(d / "ca.key"),
         "-CAcreateserial", "-out", str(d / "tls.crt"), "-days", "365", "-extfile", str(d / "issuer.ext")],
    ]
    for step in steps:
        result = runner(step, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
                        env={"PATH": path, "OPENSSL_CONF": os.devnull, "LC_ALL": "C"})
        if result.returncode:
            raise RuntimeError(f"openssl {step[1]} failed (exit {result.returncode})")
    return ((d / "ca.crt").read_text(), (d / "tls.crt").read_text(), (d / "tls.key").read_text())


def realm(callback, client_secret, passwords):
    """The issuer's realm: the gateway's confidential client (authorization
    code with S256 PKCE, one redirect URI, a groups claim in the ID token) and
    the users, with verified emails and the generated passwords."""
    users = []
    for (name, email), password in zip((("admin", ADMIN_EMAIL), ("unentitled", UNENTITLED_EMAIL)), passwords):
        users.append({"username": name, "email": email, "emailVerified": True, "enabled": True,
                      "firstName": "Spark", "lastName": name.capitalize(),
                      "credentials": [{"type": "password", "value": password, "temporary": False}],
                      "groups": ["/omg-users"]})
    return {
        "realm": REALM, "enabled": True, "sslRequired": "all", "registrationAllowed": False,
        "resetPasswordAllowed": False, "rememberMe": False, "loginWithEmailAllowed": True,
        "groups": [{"name": "omg-users"}], "users": users,
        "clients": [{
            "clientId": CLIENT_ID, "enabled": True, "protocol": "openid-connect", "publicClient": False,
            "clientAuthenticatorType": "client-secret", "secret": client_secret,
            "standardFlowEnabled": True, "implicitFlowEnabled": False, "directAccessGrantsEnabled": False,
            "serviceAccountsEnabled": False, "redirectUris": [callback], "webOrigins": [],
            "attributes": {"pkce.code.challenge.method": "S256"},
            "protocolMappers": [{
                "name": "groups", "protocol": "openid-connect", "protocolMapper": "oidc-group-membership-mapper",
                "config": {"claim.name": "groups", "full.path": "false", "id.token.claim": "true",
                           "access.token.claim": "false", "userinfo.token.claim": "true", "multivalued": "true"}}],
        }],
    }


def secret(name, string_data, secret_type="Opaque"):
    return {"apiVersion": "v1", "kind": "Secret", "metadata": {"name": name}, "type": secret_type, "stringData": string_data}


# Pods of the check that the chart's NetworkPolicy admits to the gateway
# (the load pod and the allowed probe); the refused probe has another name.
ALLOWED_CLIENT = "omg-check-load"
REFUSED_CLIENT = "omg-check-probe"


def helm_values(namespace, issuer_ip, mock_ip, network_policy):
    """The per-environment values on top of values.yaml, as --set-json pairs."""
    upstreams = json.dumps([{"endpoint": f"http://{mock_ip}:8000/v1", "addresses": [mock_ip]}], separators=(",", ":"))
    values = {
        "gateway.env.GATEWAY_PUBLIC_URL": public_url(namespace),
        # The issuer's ClusterIP is private: the identity egress policy allows it alone.
        "gateway.env.GATEWAY_EGRESS_PRIVATE_ALLOW": f"{issuer_ip}/32",
        "gateway.env.GATEWAY_LOCAL_UPSTREAMS": upstreams,
        "networkPolicy.enabled": network_policy,
    }
    if network_policy:
        # The chart's policy as shipped, with this environment as its
        # neighbourhood: ingress from the check's own client pods (there is no
        # ingress controller), egress also to the issuer and the free model.
        values.update({
            "networkPolicy.ingress.fromNamespace": namespace,
            "networkPolicy.ingress.fromPodLabels": {"app.kubernetes.io/name": ALLOWED_CLIENT},
            "networkPolicy.egress.extraEgress": [
                {"to": [{"podSelector": {"matchLabels": {"app.kubernetes.io/name": "omg-issuer"}}}],
                 "ports": [{"protocol": "TCP", "port": 8443}]},
                {"to": [{"podSelector": {"matchLabels": {"app.kubernetes.io/name": "omg-mock-upstream"}}}],
                 "ports": [{"protocol": "TCP", "port": 8000}]},
            ],
        })
    return values


def ip(value):
    if not re.fullmatch(r"(?:\d{1,3}\.){3}\d{1,3}", value or ""):
        raise ValueError("a Service has no IPv4 ClusterIP")
    return value


class Replicas:
    """Deploy and check `--mode replicas` environments through a spark-env `Spark`."""

    def __init__(self, env, spark):
        self.env = env
        self.spark = spark

    def error(self, message):
        return self.env.SparkError(message)

    def can_create_network_policy(self, namespace):
        """`kubectl auth can-i` answers "no" with exit status 1."""
        try:
            answer = self.spark.kubectl(["auth", "can-i", "create", "networkpolicies"], namespace=namespace, capture=True)
        except self.env.SparkError:
            return False
        return answer.strip() == "yes"

    def service_ip(self, namespace, name):
        value = self.spark.kubectl(["get", f"service/{name}", "-o", "jsonpath={.spec.clusterIP}"], namespace=namespace, capture=True)
        try:
            return ip(value.strip())
        except ValueError as error:
            raise self.error(f"{name}: {error}") from None

    def deploy(self, namespace, resolved, image, digest, source):
        """Everything after the namespace's isolation policies, in the order the
        invariants require: role Secrets, the issuer and the free model server,
        the chart (CloudNativePG Cluster, migrate Job with runtime grants, then
        the gateway, which waits for the schema), then the first admin."""
        spark = self.spark
        spark.kubectl(["create", "-f", "-"], namespace=namespace, stdin=self.env.role_secrets())
        client_secret = secrets.token_urlsafe(32)
        passwords = [secrets.token_urlsafe(18) for _ in range(2)]
        # Keycloak's master-realm admin, created at start so that no request
        # (a port-forward arrives from the pod's own loopback) can create one
        # on the welcome page. Never used by the tool.
        bootstrap = secrets.token_urlsafe(24)
        self.env._REGISTERED_SECRETS.extend([client_secret, *passwords, bootstrap])
        host = issuer_host(namespace)
        with tempfile.TemporaryDirectory() as directory:
            ca, certificate, key = generate_pki(directory, host, path=self.env.child_environment().get("PATH", os.defpath))
        self.env._REGISTERED_SECRETS.append(key.strip())
        callback = f"{public_url(namespace)}/api/v1/auth/callback"
        items = [
            secret("omg-issuer-tls", {"tls.crt": certificate, "tls.key": key}, "kubernetes.io/tls"),
            secret("omg-issuer-realm", {"realm.json": json.dumps(realm(callback, client_secret, passwords))}),
            secret("omg-issuer-users", {"admin": passwords[0], "unentitled": passwords[1]}),
            secret("omg-issuer-bootstrap", {"password": bootstrap}),
            secret("omg-oidc", {"GATEWAY_OIDC_ISSUER": issuer_url(namespace), "GATEWAY_OIDC_CLIENT_ID": CLIENT_ID,
                                "GATEWAY_OIDC_CLIENT_SECRET": client_secret}),
            {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "omg-issuer-ca"}, "data": {"ca.crt": ca}},
        ]
        # `create`, never `apply`: apply would copy the values into an annotation.
        spark.kubectl(["create", "-f", "-"], namespace=namespace, stdin=json.dumps({"apiVersion": "v1", "kind": "List", "items": items}))
        spark.kubectl(["create", "configmap", "omg-spark-env", f"--from-literal=ref={resolved.name}", f"--from-literal=sha={resolved.sha}",
                       f"--from-literal=image={image}", "--from-literal=mode=replicas"], namespace=namespace)
        spark.kubectl(["create", "configmap", "omg-replicas-scripts", f"--from-file=mock-upstream.mjs={DEPLOY / 'mock-upstream.mjs'}",
                       f"--from-file=load.mjs={DEPLOY / 'load.mjs'}", f"--from-file=probe.mjs={DEPLOY / 'probe.mjs'}"], namespace=namespace)
        spark.kubectl(["apply", "-f", "-"], namespace=namespace, stdin=fill((DEPLOY / "issuer.yaml").read_text(), {"ISSUER_HOST": host}))
        spark.kubectl(["apply", "-f", "-"], namespace=namespace, stdin=(DEPLOY / "mock-upstream.yaml").read_text())
        issuer_ip = self.service_ip(namespace, "omg-issuer")
        mock_ip = self.service_ip(namespace, "omg-mock-upstream")
        spark.kubectl(["rollout", "status", "deployment/omg-issuer", "--timeout=600s"], namespace=namespace)
        spark.kubectl(["rollout", "status", "deployment/omg-mock-upstream", "--timeout=300s"], namespace=namespace)
        network_policy = self.can_create_network_policy(namespace)
        args = ["install", "omg", f"{source}/deploy/helm/open-model-gateway", "-f", str(DEPLOY / "values.yaml"),
                "--set", f"image.repository={image.split('@')[0]}", "--set", f"image.digest={digest}"]
        for key_name, value in helm_values(namespace, issuer_ip, mock_ip, network_policy).items():
            args += ["--set-json", f"{key_name}={json.dumps(value)}"]
        spark.helm(args + ["--wait", "--wait-for-jobs", "--timeout", "15m", "--hide-notes"], namespace)
        admin_job = fill(self.env.render((DEPLOY / "admin-job.yaml").read_text(), image), {"ADMIN_EMAIL": ADMIN_EMAIL})
        spark.kubectl(["apply", "-f", "-"], namespace=namespace, stdin=admin_job)
        spark.wait_for_job(namespace, FIRST_ADMIN_JOB)
        return network_policy

    def forward_command(self, namespace, target, ports):
        return self.env.kubectl_command(self.spark.config, self.spark.base,
                                        ["port-forward", "--address", "127.0.0.1", target, *ports], namespace)

    def forwards(self, namespace):
        return [self.forward_command(namespace, "service/omg", [f"{GATEWAY_PORT}:8080"]),
                self.forward_command(namespace, "service/omg-issuer", [f"{ISSUER_PORT}:8443"])]

    def creds_text(self, namespace):
        read = shlex.join(self.env.kubectl_command(self.spark.config, self.spark.base,
                                                 ["get", "secret", "omg-issuer-users", "-o", "jsonpath={.data.admin}"], namespace))
        return (f"{namespace}: sign in at http://127.0.0.1:{GATEWAY_PORT} after `forward` (the gateway's origin is {public_url(namespace)};\n"
                f"a browser must also reach the issuer at {issuer_host(namespace)}:8443, for example with a hosts entry for 127.0.0.1,\n"
                f"port {ISSUER_PORT} forwarded and the environment's CA from ConfigMap omg-issuer-ca trusted).\n"
                f"  Platform Admin         admin ({ADMIN_EMAIL})\n"
                f"  Unentitled             unentitled ({UNENTITLED_EMAIL}): authenticates, platform access denied\n"
                f"The passwords are generated and stay in the cluster (Secret omg-issuer-users); this command never prints them.\n"
                f"To read one yourself: {read} | base64 -d\n")


# --- the scripted check --------------------------------------------------------

class Http:
    """Plain HTTP to the forwarded gateway, or HTTPS to the forwarded issuer
    verified against the environment's CA under the issuer's own name. Cookies
    are kept by hand (the gateway's are Secure, set for its HTTPS origin)."""

    def __init__(self, port, host=None, ca=None, child=None):
        self.port = port
        self.host = host
        self.child = child
        self.context = None
        if ca is not None:
            self.context = ssl.create_default_context(cadata=ca)

    def request(self, method, path, headers=None, body=None):
        # A forward whose kubectl has exited frees its port for any local
        # listener: nothing (a cookie, a key) is sent to it then.
        if self.child is not None and self.child.poll() is not None:
            raise ConnectionError(f"the port-forward on 127.0.0.1:{self.port} has stopped")
        if self.context is None:
            connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=30)
        else:
            connection = _PinnedHTTPS(self.host, self.port, self.context)
        try:
            connection.request(method, path, body=body, headers=headers or {})
            response = connection.getresponse()
            return response.status, {k.lower(): v for k, v in response.getheaders()}, response.getheaders(), response.read()
        finally:
            connection.close()


class _PinnedHTTPS(http.client.HTTPSConnection):
    """HTTPS to 127.0.0.1:<port>, checking the certificate for `host`."""

    def __init__(self, host, port, context):
        super().__init__(host, 8443, timeout=30, context=context)
        self._local = port

    def connect(self):
        raw = socket.create_connection(("127.0.0.1", self._local), timeout=30)
        self.sock = self._context.wrap_socket(raw, server_hostname=self.host)


def set_cookies(raw_headers):
    out = {}
    for name, value in raw_headers:
        if name.lower() == "set-cookie":
            pair = value.split(";", 1)[0]
            if "=" in pair:
                key, val = pair.split("=", 1)
                out[key.strip()] = val.strip()
    return out


def cookie_header(jar):
    return "; ".join(f"{k}={v}" for k, v in jar.items())


class Browser:
    """One person's sign-in through the real issuer and their gateway session."""

    def __init__(self, gateway, issuer, namespace):
        self.gateway = gateway
        self.issuer = issuer
        self.namespace = namespace
        self.cookies = {}

    def sign_in(self, username, password):
        """Returns ("signed-in", status) or the step at which sign-in stopped."""
        status, headers, raw, _ = self.gateway.request("GET", "/api/v1/auth/login")
        if status not in (302, 303):
            return ("login", status)
        self.cookies.update(set_cookies(raw))
        authorize = urllib.parse.urlsplit(headers["location"])
        if f"{authorize.scheme}://{authorize.netloc}" != f"https://{issuer_host(self.namespace)}:8443":
            return ("issuer", 0)
        issuer_jar = {}
        status, _, raw, body = self.issuer.request("GET", f"{authorize.path}?{authorize.query}")
        issuer_jar.update(set_cookies(raw))
        form = re.search(rb'<form[^>]*id="kc-form-login"[^>]*action="([^"]+)"', body)
        if status != 200 or not form:
            return ("issuer-form", status)
        action = urllib.parse.urlsplit(html.unescape(form.group(1).decode()))
        status, headers, raw, body = self.issuer.request(
            "POST", f"{action.path}?{action.query}",
            {"content-type": "application/x-www-form-urlencoded", "cookie": cookie_header(issuer_jar)},
            urllib.parse.urlencode({"username": username, "password": password, "credentialId": ""}))
        if status not in (302, 303) or "location" not in headers:
            # Keycloak refuses a wrong password by showing its form again.
            again = status == 200 and re.search(rb'id="kc-form-login"', body or b"")
            return ("issuer-refused" if again else "issuer-error", status)
        callback = urllib.parse.urlsplit(headers["location"])
        if f"{callback.scheme}://{callback.netloc}" != public_url(self.namespace):
            return ("redirect", 0)
        status, headers, raw, _ = self.gateway.request("GET", f"{callback.path}?{callback.query}", {"cookie": cookie_header(self.cookies)})
        self.cookies.update(set_cookies(raw))
        if not self.cookies.get("omg_session"):
            return ("gateway-refused", status)
        return ("signed-in", status)

    def api(self, method, path, body=None, client=None):
        headers = {"cookie": cookie_header(self.cookies), "origin": public_url(self.namespace)}
        if method != "GET":
            headers["x-csrf-token"] = self.cookies.get("omg_csrf", "")
        data = None
        if body is not None:
            headers["content-type"] = "application/json"
            data = json.dumps(body)
        status, _, _, payload = (client or self.gateway).request(method, path, headers, data)
        try:
            return status, json.loads(payload or b"null")
        except ValueError:
            return status, None


def inference(client, key, model=MODEL):
    status, _, _, _ = client.request("POST", "/v1/chat/completions",
                                     {"authorization": f"Bearer {key}", "content-type": "application/json"},
                                     json.dumps({"model": model, "messages": [{"role": "user", "content": "hello"}], "max_tokens": 4}))
    return status


def models(client, key):
    """The listed model ids, or None when the listing failed (never an empty set)."""
    status, _, _, body = client.request("GET", "/v1/models", {"authorization": f"Bearer {key}"})
    if status != 200:
        return status, None
    return status, {m.get("id") for m in json.loads(body).get("data", [])}


UUID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")


class Check:
    """`spark-env.py check <name>` on a `--mode replicas` environment."""

    def __init__(self, env, spark, namespace):
        self.env = env
        self.spark = spark
        self.namespace = namespace
        self.results = []
        self.children = []

    # -- cluster helpers --
    def kubectl(self, args, **kwargs):
        return self.spark.kubectl(args, namespace=self.namespace, **kwargs)

    def sql(self, statement):
        """One statement in the database as its owner, through the instance pod."""
        pod = self.kubectl(["get", "pods", "-l", "cnpg.io/cluster=omg-db,cnpg.io/instanceRole=primary", "-o", "jsonpath={.items[0].metadata.name}"], capture=True).strip()
        return self.kubectl(["exec", pod, "-c", "postgres", "--", "psql", "-U", "postgres", "-d", "gateway", "-Atqc", statement], capture=True).strip()

    def gateway_pods(self):
        out = self.kubectl(["get", "pods", "-l", "app.kubernetes.io/name=open-model-gateway,app.kubernetes.io/component=gateway",
                            "-o", "json"], capture=True)
        pods = json.loads(out)["items"]
        return [p for p in pods if p["metadata"].get("deletionTimestamp") is None and p["status"].get("phase") == "Running"]

    def forward(self, target, ports):
        """A port-forward that this process's own kubectl holds: the port is
        free just before, and the forward counts as open only once kubectl
        reports it listens there. Anything else that answered on the port (a
        listener another local user started) would receive the session cookie
        or the key, so the check refuses instead."""
        port = int(ports[0].split(":")[0])
        if not self.spark.port_free(port):
            raise self.env.SparkError(f"local port {port} is in use; stop what holds it and retry")
        command = Replicas(self.env, self.spark).forward_command(self.namespace, target, ports)
        child = subprocess.Popen(command, env=self.env.child_environment(), stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                 start_new_session=True, text=True)
        self.children.append(child)
        ready = threading.Event()
        expected = f"Forwarding from 127.0.0.1:{port} -> "

        def drain():
            # Read every line, so kubectl never blocks on a full pipe.
            with child.stdout:
                for line in child.stdout:
                    if line.startswith(expected):
                        ready.set()

        threading.Thread(target=drain, daemon=True).start()
        if ready.wait(20) and child.poll() is None:
            return child
        self.stop(child)
        raise self.env.SparkError(f"port-forward to {target} did not open on 127.0.0.1:{port}")

    def stop(self, child):
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)

    def record(self, step, name, outcome, detail=""):
        self.results.append({"step": step, "name": name, "result": outcome, "detail": detail})
        print(f"{outcome.upper():4} {step}.{name}" + (f" ({detail})" if detail else ""))

    def expect(self, step, name, ok, detail=""):
        self.record(step, name, "pass" if ok else "fail", detail)
        return ok

    # -- the run --
    def guarded(self, step, name, action, *args):
        """Run one step; an error fails that step and the check goes on with
        the next. The summary gets the error's type only (it may be attached
        to a public PR, and cluster error text can name accounts and
        addresses); the terminal also gets its redacted, shortened text."""
        try:
            return action(*args)
        except Exception as error:  # noqa: BLE001 - every error is that step's FAIL
            message = re.sub(r"\s+", " ", self.env.redact(str(error)))[:300]
            print(f"     step {step} error: {message}")
            self.record(step, name, "fail", f"error ({type(error).__name__}; its text is in the terminal output only)")
            return None

    def run(self):
        started = time.monotonic()
        ca = self.kubectl(["get", "configmap", "omg-issuer-ca", "-o", "jsonpath={.data.ca\\.crt}"], capture=True)
        users = {name: self.read_secret("omg-issuer-users", name) for name in ("admin", "unentitled")}
        self.env._REGISTERED_SECRETS.extend(users.values())
        admin = key = None
        try:
            gateway_forward = self.forward("service/omg", [f"{GATEWAY_PORT}:8080"])
            issuer_forward = self.forward("service/omg-issuer", [f"{ISSUER_PORT}:8443"])
            gateway = Http(GATEWAY_PORT, child=gateway_forward)
            issuer = Http(ISSUER_PORT, issuer_host(self.namespace), ca, child=issuer_forward)
            admin = self.guarded(1, "sign-in", self.sign_in_step, gateway, issuer, users)
            key = self.guarded(2, "inference", self.inference_step, gateway, admin) if admin else None
            self.stop(gateway_forward)
            self.guarded(3, "rolling-restart", self.restart_step, key)
            self.guarded(4, "lease-takeover", self.lease_step)
            self.guarded(5, "change-crosses-replicas", self.notification_step, admin, key)
            self.guarded(6, "network-policy", self.network_policy_step, key)
            self.guarded(7, "runtime-traffic-through-the-pooler", self.pooler_step)
        finally:
            for child in self.children:
                self.stop(child)
            self.cleanup(admin)
        return {"seconds": round(time.monotonic() - started), "steps": self.results,
                "passed": all(r["result"] != "fail" for r in self.results)}

    def read_secret(self, name, key):
        import base64
        value = self.kubectl(["get", "secret", name, "-o", f"jsonpath={{.data.{key}}}"], capture=True).strip()
        return base64.b64decode(value).decode()

    def cleanup(self, admin=None):
        """Remove what the check created in the namespace and revoke its key;
        each removal is tried even if another fails."""
        for kind, name in (("pod", "omg-check-load"), ("pod", "omg-check-allowed"), ("pod", "omg-check-refused"),
                           ("secret", "omg-check-key")):
            try:
                self.kubectl(["delete", kind, name, "--ignore-not-found", "--wait=false"], capture=True)
            except Exception as error:  # noqa: BLE001 - reported; the next item is still removed
                print(f"cleanup: deleting {kind}/{name} failed: {self.env.redact(str(error))[:200]}")
        if admin is not None and getattr(self, "key_path", None):
            try:
                child = self.forward("service/omg", [f"{GATEWAY_PORT}:8080"])
                try:
                    status, _ = admin.api("DELETE", self.key_path, client=Http(GATEWAY_PORT, child=child))
                finally:
                    self.stop(child)
                if status not in (200, 204):
                    print(f"cleanup: revoking the check's key returned HTTP {status} (it expires in a day)")
            except Exception as error:  # noqa: BLE001 - reported; cleanup never hides the check's result
                print(f"cleanup: revoking the check's key failed: {self.env.redact(str(error))[:200]} (it expires in a day)")

    def sign_in_step(self, gateway, issuer, users):
        admin = Browser(gateway, issuer, self.namespace)
        outcome = admin.sign_in("admin", users["admin"])
        signed = self.expect(1, "admin-signs-in", outcome[0] == "signed-in", f"{outcome[0]} {outcome[1]}")
        if signed:
            status, me = admin.api("GET", "/api/v1/me")
            self.expect(1, "admin-is-platform-admin", status == 200 and (me or {}).get("user", {}).get("platform_role") == "admin", f"GET /api/v1/me {status}")
        # The refusals mean something only if the same path signs the admin in.
        if not signed:
            self.record(1, "wrong-password-refused", "fail", "the admin's sign-in failed, so a refusal proves nothing")
            self.record(1, "unentitled-refused", "fail", "the admin's sign-in failed, so a refusal proves nothing")
            return None
        wrong = Browser(gateway, issuer, self.namespace).sign_in("admin", users["admin"] + "-wrong")
        self.expect(1, "wrong-password-refused", wrong == ("issuer-refused", 200), f"stopped at {wrong[0]} {wrong[1]}")
        # Refused by the gateway for its platform role: the identity is
        # linked (the token exchange and ID token checks passed) and no
        # session exists for it. A first run shows the link appear; a rerun
        # finds it from before (the admin's sign-in, through the same realm,
        # client and mappers, shows the token checks pass).
        link = f"SELECT count(*) FROM oidc_identities i JOIN users u ON u.id=i.user_id WHERE u.email='{UNENTITLED_EMAIL}'"
        before = self.sql(link)
        unentitled = Browser(gateway, issuer, self.namespace).sign_in("unentitled", users["unentitled"])
        linked = self.sql(link)
        sessions = self.sql(f"SELECT count(*) FROM browser_sessions s JOIN users u ON u.id=s.user_id WHERE u.email='{UNENTITLED_EMAIL}'")
        self.expect(1, "unentitled-refused", unentitled == ("gateway-refused", 403) and linked == "1" and sessions == "0",
                    f"stopped at {unentitled[0]} {unentitled[1]}, identity linked {before} -> {linked}"
                    f"{' (by an earlier run)' if before == '1' else ''}, sessions {sessions}")
        return admin

    def ensure_model(self, admin):
        """The free model, served by the in-namespace mock, priced, assigned to
        the admin's personal workspace: created once, found on later runs."""
        mock_ip = self.kubectl(["get", "service/omg-mock-upstream", "-o", "jsonpath={.spec.clusterIP}"], capture=True).strip()
        status, me = admin.api("GET", "/api/v1/me")
        workspace = next(w["id"] for w in me["workspaces"] if w["kind"] == "personal")
        status, listed = admin.api("GET", f"/api/v1/platform/models?limit=200")
        existing = [m for m in (listed or {}).get("data", []) if m.get("public_name") == MODEL]
        if existing:
            model = existing[0]["id"]
        else:
            status, provider = admin.api("POST", "/api/v1/platform/providers", {
                "name": "Spark mock", "provider": "openai_compatible", "credential_ref": "none",
                "endpoint": f"http://{ip(mock_ip)}:8000/v1", "enabled": True})
            if status not in (200, 201):
                raise self.env.SparkError(f"creating the mock connection failed: HTTP {status}")
            status, created = admin.api("POST", "/api/v1/platform/model-setup", {
                "model": {"public_name": MODEL, "display_name": "Spark mock", "description": None,
                          "supported_protocols": ["chat_completions"], "enabled": True},
                "route": {"provider_connection_id": provider["id"], "upstream_model": "mock", "enabled": True},
                "price": {"input_microusd_per_million": "1000000", "output_microusd_per_million": "1000000",
                          "input_token_limit": 4096, "output_token_limit": 256},
                "catalog_ids": []})
            if status not in (200, 201):
                raise self.env.SparkError(f"model setup failed: HTTP {status}")
            model = created["model_id"]
        admin.api("POST", f"/api/v1/platform/workspaces/{workspace}/models", {"model_id": model})
        status, issued = admin.api("POST", f"/api/v1/workspaces/{workspace}/keys", {"name": f"spark-check-{secrets.token_hex(3)}", "expires_in_days": 1})
        if status not in (200, 201):
            raise self.env.SparkError(f"issuing the check's key failed: HTTP {status}")
        key = issued["token"]
        self.env._REGISTERED_SECRETS.append(key)
        self.key_path = f"/api/v1/workspaces/{workspace}/keys/{issued['id']}"
        return model, key

    def inference_step(self, gateway, admin):
        model, key = self.ensure_model(admin)
        self.model_id = model
        before = int(self.sql("SELECT count(*) FROM governance_reservations WHERE state='settled'") or "0")
        status = inference(gateway, key)
        self.expect(2, "inference-through-the-mock", status == 200, f"HTTP {status}")
        latest = self.sql("SELECT state FROM governance_reservations ORDER BY admitted_at DESC LIMIT 1")
        after = int(self.sql("SELECT count(*) FROM governance_reservations WHERE state='settled'") or "0")
        self.expect(2, "reservation-settled", latest == "settled" and after == before + 1, f"latest {latest}, settled +{after - before}")
        return key

    def restart_step(self, key):
        if key is None:
            self.record(3, "rolling-restart-under-load", "fail", "no key: an earlier step failed")
            return
        self.kubectl(["create", "-f", "-"], stdin=json.dumps(secret("omg-check-key", {"key": key})), capture=True)
        seconds = 900
        self.kubectl(["apply", "-f", "-"], stdin=fill((DEPLOY / "load-pod.yaml").read_text(), {"MODEL": MODEL, "SECONDS": str(seconds)}), capture=True)
        self.kubectl(["wait", "--for=condition=Ready", "pod/omg-check-load", "--timeout=120s"], capture=True)
        time.sleep(10)
        minimum = [self.ready_replicas()]
        samples = [0]
        errors = [0]
        stop = threading.Event()

        def watch():
            while not stop.is_set():
                try:
                    minimum[0] = min(minimum[0], self.ready_replicas())
                    samples[0] += 1
                except Exception:  # noqa: BLE001 - counted; the step fails on any
                    errors[0] += 1
                time.sleep(0.5)

        watcher = threading.Thread(target=watch, daemon=True)
        watcher.start()
        restart = time.monotonic()
        try:
            self.kubectl(["rollout", "restart", "deployment/omg"], capture=True)
            try:
                self.kubectl(["rollout", "status", "deployment/omg", "--timeout=420s"], capture=True)
            except self.env.SparkError:
                self.record(3, "rolling-restart-no-failed-request", "fail", f"the rollout did not finish: {self.replica_failure()}")
                return
            rolled = round(time.monotonic() - restart)
        finally:
            stop.set()
            watcher.join()
        # Load covers the whole rollout and ten seconds after it, then stops
        # at the marker (a pod that already ended stopped too early).
        time.sleep(10)
        self.kubectl(["exec", "omg-check-load", "--", "node", "-e", "require('fs').writeFileSync('/tmp/stop', '')"], capture=True)
        self.kubectl(["wait", "--for=jsonpath={.status.phase}=Succeeded", "pod/omg-check-load", "--timeout=120s"], capture=True)
        counts = json.loads(self.kubectl(["logs", "pod/omg-check-load"], capture=True).strip().splitlines()[-1])
        ok = counts["statuses"].get("200", 0)
        failures = sum(counts["failures"].values()) + sum(v for k, v in counts["statuses"].items() if k != "200")
        # At least two requests a second over the whole window (the measured
        # rate is about a hundred): an idle load generator proves nothing.
        enough = ok >= 2 * (rolled + 20)
        self.expect(3, "rolling-restart-no-failed-request", failures == 0 and enough and counts.get("stopped") == "marker",
                    f"{ok} ok, {failures} failed of {counts['sent']}, stopped at {counts.get('stopped')}, "
                    f"sent from 10 s before the restart until 10 s after its {rolled} s rollout")
        sampled = samples[0] >= max(1, rolled)
        self.expect(3, "rolling-restart-kept-a-ready-replica", minimum[0] >= 1 and sampled and errors[0] == 0,
                    f"fewest ready replicas {minimum[0]} in {samples[0]} samples, {errors[0]} failed samples")
        self.pdb_check()

    def ready_replicas(self):
        return sum(1 for p in self.gateway_pods() if any(c.get("type") == "Ready" and c.get("status") == "True"
                                                        for c in p["status"].get("conditions", [])))

    def wait_for_ready_replicas(self, count=2, seconds=600):
        self.kubectl(["rollout", "status", "deployment/omg", f"--timeout={seconds}s"], capture=True)
        for _ in range(seconds):
            if self.ready_replicas() >= count:
                return
            time.sleep(1)
        raise self.env.SparkError(f"fewer than {count} ready replicas after {seconds} s")

    def replica_failure(self):
        """Why the Deployment cannot create a pod (for example the quota), if it says."""
        status = json.loads(self.kubectl(["get", "deployment/omg", "-o", "json"], capture=True)).get("status", {})
        for condition in status.get("conditions", []):
            if condition.get("type") == "ReplicaFailure" and condition.get("status") == "True":
                return re.sub(r"\s+", " ", condition.get("message", ""))[:300]
        return "no ReplicaFailure condition"

    def evict(self, pod):
        body = json.dumps({"apiVersion": "policy/v1", "kind": "Eviction", "metadata": {"name": pod, "namespace": self.namespace}})
        try:
            self.kubectl(["create", "--raw", f"/api/v1/namespaces/{self.namespace}/pods/{pod}/eviction", "-f", "-"], stdin=body, capture=True)
            return "evicted"
        except self.env.SparkError as error:
            # The API's answer only, never the command's own text (which names
            # the namespace and pod).
            text = re.split(r" failed \(exit \d+\): ", str(error), maxsplit=1)[-1].lower()
            if "forbidden" in text and "pods/eviction" in text:
                return "forbidden"
            return "refused" if "violate the pod's disruption budget" in text or "toomanyrequests" in text else "error"

    def pdb_check(self):
        """The budget covers the gateway pods (one may be disrupted of two), and
        an eviction of a second pod while one is being replaced is refused."""
        status = json.loads(self.kubectl(["get", "pdb", "omg", "-o", "json"], capture=True))["status"]
        self.expect(3, "pdb-covers-the-replicas", (status.get("expectedPods"), status.get("desiredHealthy"), status.get("disruptionsAllowed")) == (2, 1, 1),
                    f"expected {status.get('expectedPods')}, desired healthy {status.get('desiredHealthy')}, disruptions allowed {status.get('disruptionsAllowed')}")
        pods = [p["metadata"]["name"] for p in self.gateway_pods()]
        if len(pods) < 2:
            self.record(3, "pdb-refuses-a-second-eviction", "fail", f"{len(pods)} running replicas")
            return
        first = self.evict(pods[0])
        if first == "forbidden":
            self.record(3, "pdb-refuses-a-second-eviction", "skip", "the platform refuses pods/eviction in this namespace (can-i answers yes)")
            return
        second = self.evict(pods[1])
        self.expect(3, "pdb-refuses-a-second-eviction", (first, second) == ("evicted", "refused"), f"{first}, then {second}")
        self.wait_for_ready_replicas()

    def lease_step(self, lease="metrics"):
        """Delete the pod holding a lease; another replica takes it within the
        documented time (TTL 30 s + renewal 10 s) and the old term is fenced."""
        # Just after the rollout of step 3 the lease can be between holders
        # (the old pod's term runs out, then a replica takes it): wait for a
        # current holder, at most the documented takeover time and a margin.
        current = []
        for _ in range(60):
            current = self.sql(f"SELECT holder||' '||epoch FROM work_leases WHERE name='{lease}' AND expires_at>clock_timestamp()").split()
            if len(current) == 2:
                break
            time.sleep(1)
        if len(current) != 2:
            self.record(4, "lease-takeover", "fail", f"no replica holds the {lease} lease")
            return
        holder, epoch = current
        if not UUID.fullmatch(holder) or not epoch.isdigit():
            self.record(4, "lease-takeover", "fail", "the lease row is not a holder UUID and an epoch")
            return
        # The holder is the replica whose own metrics say it holds the lease.
        owner = None
        for pod, port in zip(self.gateway_pods(), METRICS_PORTS):
            child = self.forward(f"pod/{pod['metadata']['name']}", [f"{port}:9464"])
            try:
                status, _, _, body = Http(port, child=child).request("GET", "/metrics")
            finally:
                self.stop(child)
            if status == 200 and re.search(rf'^gateway_work_leases_held\{{lease="{lease}"\}} 1$', body.decode(), re.M):
                owner = pod["metadata"]["name"]
        if owner is None:
            self.record(4, "lease-takeover", "fail", f"no replica's metrics show the {lease} lease held")
            return
        deleted = time.monotonic()
        self.kubectl(["delete", "pod", owner, "--wait=false"], capture=True)
        took = None
        for _ in range(90):
            now = self.sql(f"SELECT holder||' '||epoch FROM work_leases WHERE name='{lease}' AND expires_at>clock_timestamp()").split()
            if len(now) == 2 and now[0] != holder and int(now[1]) > int(epoch):
                took = round(time.monotonic() - deleted, 1)
                break
            time.sleep(1)
        self.expect(4, "lease-taken-over-by-another-replica", took is not None and took <= 40, f"{lease} lease after {took}s (documented: at most 40 s)")
        # The old term's fence check must fail with 55006 (object_in_use).
        probe = (f"SELECT CASE WHEN EXISTS (SELECT FROM work_leases WHERE name='{lease}' AND holder='{holder}'::uuid "
                 f"AND epoch={int(epoch)} AND expires_at>clock_timestamp()) THEN 'current' ELSE 'stale' END")
        try:
            self.sql(f"SELECT omg_lease_fence('{lease}','{holder}'::uuid,{int(epoch)})")
            fenced = False
        except self.env.SparkError as error:
            fenced = "is no longer held" in str(error)
        self.expect(4, "stale-term-fenced", fenced and self.sql(probe) == "stale",
                    "the old term's fence check (omg_lease_fence) fails in the database (55006)")
        self.wait_for_ready_replicas()

    def notification_step(self, admin, key):
        """A change made through one replica is seen through the other."""
        if admin is None or key is None:
            self.record(5, "change-crosses-replicas", "fail", "an earlier step failed")
            return
        pods = sorted(p["metadata"]["name"] for p in self.gateway_pods())
        if len(pods) < 2:
            self.record(5, "change-crosses-replicas", "fail", f"{len(pods)} running replicas")
            return
        clients = []
        for pod, port in zip(pods, POD_PORTS):
            clients.append(Http(port, child=self.forward(f"pod/{pod}", [f"{port}:8080"])))
        a, b = clients
        _, before = models(b, key)
        if before is None or MODEL not in before:
            self.record(5, "change-crosses-replicas", "fail", "the model is not listed through the second replica")
            return
        timings = []
        # Replicas also poll the configuration versions every second, so a
        # bound of 300 ms on six changes in a row is met by the change
        # notification, not by a poll that happened to come soon.
        for enabled in (False, True) * 3:
            status, _ = admin.api("PATCH", f"/api/v1/platform/models/{self.model_id}", {"enabled": enabled}, client=a)
            if status != 200:
                self.record(5, "change-crosses-replicas", "fail", f"PATCH through the first replica: HTTP {status}")
                return
            changed = time.monotonic()
            seen = None
            for _ in range(200):
                listed = models(b, key)[1]
                if listed is not None and (MODEL in listed) == enabled:
                    seen = round((time.monotonic() - changed) * 1000)
                    break
                time.sleep(0.01)
            timings.append(seen)
            # A random spacing, so no change lands at a fixed phase of the poll.
            time.sleep(1.0 + secrets.randbelow(1000) / 1000)
        ok = all(t is not None and t <= 300 for t in timings)
        self.expect(5, "change-crosses-replicas", ok,
                    f"through {pods[0][-5:]}, seen through {pods[1][-5:]} after {', '.join(str(t) for t in timings)} ms (disable, enable, three times)")

    def network_policy_step(self, key):
        """With the chart's policy deployed: a pod it admits reaches the
        gateway, one it does not is refused."""
        policies = self.kubectl(["get", "networkpolicy", "-l", "app.kubernetes.io/name=open-model-gateway", "-o", "name"], capture=True).split()
        if not policies:
            if not Replicas(self.env, self.spark).can_create_network_policy(self.namespace):
                self.record(6, "network-policy", "skip", "the platform refuses NetworkPolicy to this account")
            else:
                self.record(6, "network-policy", "fail", "the platform now allows NetworkPolicy, but this environment was created without it: recreate it")
            return
        outcomes = {}
        pods = (("omg-check-allowed", ALLOWED_CLIENT), ("omg-check-refused", REFUSED_CLIENT))
        for pod, label in pods:
            # A fresh pod each run: never the result of an earlier one.
            self.kubectl(["delete", "pod", pod, "--ignore-not-found", "--wait=true", "--timeout=60s"], capture=True)
            self.kubectl(["create", "-f", "-"], stdin=fill((DEPLOY / "probe-pod.yaml").read_text(), {"NAME": pod, "LABEL": label}), capture=True)
        for pod, _ in pods:
            self.kubectl(["wait", "--for=jsonpath={.status.phase}=Succeeded", f"pod/{pod}", "--timeout=120s"], capture=True)
            lines = self.kubectl(["logs", f"pod/{pod}"], capture=True).strip().splitlines()
            outcomes[pod] = json.loads(lines[-1]) if lines else {}
        allowed, refused = outcomes["omg-check-allowed"], outcomes["omg-check-refused"]
        self.expect(6, "network-policy-allows-a-client", allowed.get("control") == 200 and allowed.get("probe") == 200,
                    f"{ALLOWED_CLIENT}: control {allowed.get('control')}, gateway {allowed.get('probe')} ({len(policies)} chart policies)")
        # The refused pod has a working network (it reaches the free model,
        # which the chart's policy does not cover), so a timeout is the policy.
        if refused.get("control") == 200 and refused.get("probe") == 200:
            others = sorted(set(self.kubectl(["get", "networkpolicy", "-o", "name"], capture=True).split()) - set(policies))
            if others:
                self.record(6, "network-policy-refuses-another-pod", "skip",
                            f"another policy in the namespace ({len(others)}) admits {REFUSED_CLIENT}: policies add up, so the chart's refusal cannot be shown here")
                return
        self.expect(6, "network-policy-refuses-another-pod", refused.get("control") == 200 and refused.get("probe") in ("timeout", "refused"),
                    f"{REFUSED_CLIENT}: control {refused.get('control')}, gateway {refused.get('probe')}")

    def pooler_step(self):
        pooler = set(self.kubectl(["get", "pods", "-l", "cnpg.io/poolerName=omg-pooler", "-o", "jsonpath={.items[*].status.podIP}"], capture=True).split())
        gateways = {p["status"].get("podIP") for p in self.gateway_pods()}
        rows = [r.split("|") for r in self.sql(
            "SELECT host(client_addr)||'|'||CASE WHEN query ILIKE 'LISTEN%' THEN 'listen' ELSE 'other' END "
            "FROM pg_stat_activity WHERE usename='gateway_runtime' AND client_addr IS NOT NULL").split()]
        via_pooler = sum(1 for address, _ in rows if address in pooler)
        listening = {address for address, kind in rows if address in gateways and kind == "listen"}
        # Any address but the pooler's: a current replica, or one still
        # terminating after step 4 (whose LISTEN session may linger).
        other_direct = sum(1 for address, kind in rows if address not in pooler and kind != "listen")
        # PgBouncer's transaction mode cannot carry LISTEN, so each replica
        # holds one direct LISTEN session by design; anything else goes
        # through the pooler.
        self.expect(7, "runtime-traffic-through-the-pooler",
                    via_pooler >= 1 and other_direct == 0 and listening == gateways,
                    f"{via_pooler} sessions from the pooler, {len(listening)} replicas with a direct LISTEN session, {other_direct} other direct sessions")


def quota(spark, namespace):
    """What the account uses of its shared quota (requests, limits, pods,
    volumes): the platform's quota objects, read in the environment's namespace."""
    out = spark.kubectl(["get", "resourcequota", "-o", "json"], namespace=namespace, capture=True)
    used = {}
    for item in json.loads(out)["items"]:
        for k, v in item.get("status", {}).get("used", {}).items():
            if k in ("requests.cpu", "requests.memory", "limits.cpu", "limits.memory", "pods", "requests.storage"):
                used[k] = v
    return used


def require_openssl(which=shutil.which):
    if not which("openssl"):
        raise RuntimeError("openssl is not installed")
