"""Fake EC2 instance metadata service (IMDSv2) for tests/imds_e2e.sh. Python 3 stdlib only.

Hands out REAL temporary credentials: STS AssumeRole against the test MinIO, from one of two MinIO users that take
turns as the "role". Every ROT seconds the credentials rotate: the other user is enabled, new STS credentials are
issued from it, and GRACE seconds later the previous user is DISABLED, which makes MinIO refuse every STS credential
derived from it. So a client that does not refresh in time fails; one that refreshes keeps working.

The reported Expiration is issue time + ROT + 300 s, so a client refreshing 5 minutes early asks right when the
rotation is due. Rotation happens when a client asks after that point, or by itself ROT + 15 s after the last one.

PUT /_mode/<m> (the test, from inside this container): v2 (default)
                | v1only (the token PUT answers 405; GETs work without a token)
                | norole (no role attached: 404 on the role list)
/w/issued.txt     every AccessKeyId / SecretAccessKey / SessionToken handed out, one per line (the test greps logs
                  for them; never printed)
/w/imds_tokens.txt every IMDSv2 session token handed out
/w/requests.log   METHOD path token=yes|no, one line per request
/w/rotations.log  one line per rotation (time, parent user index)
"""
import datetime
import hashlib
import hmac
import json
import os
import re
import secrets
import threading
import time
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

S3 = os.environ["S3"]                      # http://pgbx-imds-s3:9000
ROT = int(os.environ.get("ROT", "30"))
GRACE = int(os.environ.get("GRACE", "10"))
ROLE = "pgbx-imds-role"
ROOT = open("/w/root").read().split()      # access key, secret of the MinIO root user (admin API)
USERS = [l.split() for l in open("/w/users").read().splitlines() if l.strip()]
LOCK = threading.Lock()
TOKENS = set()
CUR = {}


def w(name, line):
    with open("/w/" + name, "a") as f:
        f.write(line + "\n")


def sigv4(method, url, body, ak, sk, service, extra):
    u = urllib.parse.urlsplit(url)
    t = datetime.datetime.now(datetime.timezone.utc)
    amz, day = t.strftime("%Y%m%dT%H%M%SZ"), t.strftime("%Y%m%d")
    ph = hashlib.sha256(body).hexdigest()
    h = {"host": u.netloc, "x-amz-date": amz, "x-amz-content-sha256": ph}
    h.update(extra)
    q = sorted(urllib.parse.parse_qsl(u.query, keep_blank_values=True))
    cq = "&".join(urllib.parse.quote(k, safe="-_.~") + "=" + urllib.parse.quote(v, safe="-_.~") for k, v in q)
    names = sorted(h)
    creq = "\n".join([method, u.path or "/", cq, "".join(f"{k}:{h[k].strip()}\n" for k in names), ";".join(names), ph])
    scope = f"{day}/us-east-1/{service}/aws4_request"
    sts = "\n".join(["AWS4-HMAC-SHA256", amz, scope, hashlib.sha256(creq.encode()).hexdigest()])
    k = ("AWS4" + sk).encode()
    for p in (day, "us-east-1", service, "aws4_request"):
        k = hmac.new(k, p.encode(), hashlib.sha256).digest()
    sig = hmac.new(k, sts.encode(), hashlib.sha256).hexdigest()
    h["authorization"] = f"AWS4-HMAC-SHA256 Credential={ak}/{scope}, SignedHeaders={';'.join(names)}, Signature={sig}"
    req = urllib.request.Request(url, data=body if method != "GET" else None, method=method, headers=h)
    with urllib.request.urlopen(req, timeout=10) as r:
        return r.read().decode()


def set_status(i, status):
    sigv4("PUT", f"{S3}/minio/admin/v3/set-user-status?accessKey={USERS[i][0]}&status={status}", b"",
          ROOT[0], ROOT[1], "s3", {})


def assume(i):
    body = urllib.parse.urlencode({"Action": "AssumeRole", "Version": "2011-06-15", "DurationSeconds": "900"}).encode()
    xml = sigv4("POST", S3 + "/", body, USERS[i][0], USERS[i][1], "sts",
                {"content-type": "application/x-www-form-urlencoded"})
    get = lambda t: re.search(f"<{t}>([^<]+)</{t}>", xml).group(1)
    return {"AccessKeyId": get("AccessKeyId"), "SecretAccessKey": get("SecretAccessKey"), "Token": get("SessionToken")}


def rotate():
    """Under LOCK. The other user becomes the role; the previous one is disabled GRACE seconds later."""
    old = CUR.get("parent")
    i = 0 if old is None else 1 - old
    set_status(i, "enabled")
    c = assume(i)
    issued = int(time.time())
    c["Expiration"] = datetime.datetime.fromtimestamp(issued + ROT + 300, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    CUR.update(parent=i, issued=issued, creds=c, n=CUR.get("n", 0) + 1)
    for k in ("AccessKeyId", "SecretAccessKey", "Token"):
        w("issued.txt", c[k])
    w("rotations.log", f"{issued} rotation {CUR['n']} parent {i}")
    if old is not None:
        def disable(o=old, n=CUR["n"]):
            with LOCK:
                if CUR["n"] == n:            # still the rotation that retired it
                    set_status(o, "disabled")
                    w("rotations.log", f"{int(time.time())} disabled parent {o}")
        threading.Timer(GRACE, disable).start()
    else:
        set_status(1 - i, "disabled")


def ticker():
    while True:
        time.sleep(1)
        with LOCK:
            try:
                if time.time() >= CUR["issued"] + ROT + 15:
                    rotate()
            except Exception as e:
                w("rotations.log", f"rotation failed: {e}")


MODE = ["v2"]


def mode():
    return MODE[0]


class H(BaseHTTPRequestHandler):
    def reply(self, code, body=""):
        b = body.encode()
        self.send_response(code)
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_PUT(self):
        if self.path.startswith("/_mode/"):
            MODE[0] = self.path[len("/_mode/"):]
            return self.reply(200, MODE[0])
        w("requests.log", f"PUT {self.path} token=no")
        if self.path != "/latest/api/token":
            return self.reply(404)
        if mode() == "v1only":
            return self.reply(405)
        if not self.headers.get("X-aws-ec2-metadata-token-ttl-seconds"):
            return self.reply(400)
        t = secrets.token_hex(24)
        TOKENS.add(t)
        w("imds_tokens.txt", t)
        self.reply(200, t)

    def do_GET(self):
        tok = self.headers.get("X-aws-ec2-metadata-token")
        w("requests.log", f"GET {self.path} token={'yes' if tok else 'no'}")
        m = mode()
        if m != "v1only" and tok not in TOKENS:
            return self.reply(401)
        base = "/latest/meta-data/iam/security-credentials/"
        if m == "norole" and self.path.startswith(base):
            return self.reply(404)
        if self.path == base:
            return self.reply(200, ROLE)
        if self.path == base + ROLE:
            with LOCK:
                if time.time() >= CUR["issued"] + ROT:
                    rotate()
                c = dict(CUR["creds"])
            c.update(Code="Success", Type="AWS-HMAC",
                     LastUpdated=datetime.datetime.fromtimestamp(CUR["issued"], datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"))
            return self.reply(200, json.dumps(c))
        self.reply(404)

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    for _ in range(60):
        try:
            with LOCK:
                rotate()
            break
        except Exception as e:  # MinIO still starting
            time.sleep(1)
    threading.Thread(target=ticker, daemon=True).start()
    w("requests.log", "ready")
    ThreadingHTTPServer(("0.0.0.0", 80), H).serve_forever()
