"""Exercise the real user service, automatic update, rollback and uninstall.

Run with python -I run.py WORK. No test flags, alternate updater or timer seams.
Only diagnostics/ is safe to upload; config, init output and keys never are.
"""

from collections import Counter
from concurrent.futures import ThreadPoolExecutor
import functools
import http.client
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import platform
import socket
import subprocess
import sys
import threading
import time

import yaml


WORK = Path(sys.argv[1]).resolve()
OUT = WORK / "diagnostics"
LABEL = "io.github.loft-902-co-llc.open-ferry"
SECRETS = []


def clean(value):
    text = str(value)
    for secret in SECRETS:
        text = text.replace(secret, "[redacted]")
    return text


def write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as stream:
        stream.write(clean(text))


def record(message):
    message = clean(message)
    print(message, flush=True)
    with (OUT / "qualification.log").open("a", encoding="utf-8", newline="\n") as stream:
        stream.write(time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()) + " " + message + "\n")


def command(args, *, check=True, timeout=90, sensitive=False, **kwargs):
    result = subprocess.run([str(arg) for arg in args], stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            text=True, encoding="utf-8", errors="replace", timeout=timeout, **kwargs)
    if not sensitive:
        record("$ " + subprocess.list2cmdline([str(arg) for arg in args]) + "\n" + result.stdout)
    if check and result.returncode:
        raise RuntimeError(f"{Path(args[0]).name} failed with exit {result.returncode}"
                           + (" (installer output withheld: init may print keys)" if sensitive else ""))
    return result


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def free_port():
    with socket.socket() as stream:
        stream.bind(("127.0.0.1", 0))
        port = stream.getsockname()[1]
        require(port != 8317, "refusing the reserved development port")
        return port


class Mirror(SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    latest = None
    requests = Counter()
    lock = threading.Lock()

    def do_GET(self):
        with self.lock:
            self.requests[self.path] += 1
            latest = self.latest
        if self.path.startswith("/latest/download/"):
            self.path = f"/download/v{latest}/" + self.path[len("/latest/download/"):]
        super().do_GET()

    def log_message(self, *_args):
        pass


class Load:
    def __init__(self, port, key):
        self.port, self.key = port, key
        self.stop = threading.Event()
        self.lock = threading.Lock()
        self.counts = Counter()
        self.errors = []
        self.phase = "baseline"
        self.pool = ThreadPoolExecutor(max_workers=4)
        self.futures = [self.pool.submit(self.loop) for _ in range(4)]

    def loop(self):
        while not self.stop.is_set():
            with self.lock:
                phase = self.phase
            connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)
            error = None
            try:
                # New connections deliberately cover listener handovers, too.
                connection.request("GET", "/v1/models", headers={"Authorization": "Bearer " + self.key})
                response = connection.getresponse()
                body = response.read()
                if not 200 <= response.status < 300:
                    raise RuntimeError(f"HTTP {response.status}")
                if not isinstance(json.loads(body).get("data"), list):
                    raise RuntimeError("models response has no data list")
            except Exception as failure:  # Every refused/reset/timed-out/truncated request counts.
                error = clean(f"{type(failure).__name__}: {failure}")
            finally:
                connection.close()
            with self.lock:
                self.counts[f"{phase}.requests"] += 1
                self.counts[f"{phase}.failures"] += int(error is not None)
                if error and len(self.errors) < 30:
                    self.errors.append({"phase": phase, "error": error})
            self.stop.wait(0.05)

    def snapshot(self):
        with self.lock:
            result = {"counts": dict(self.counts), "errors": list(self.errors)}
        write(OUT / "requests.json", json.dumps(result, indent=2) + "\n")
        return result

    def healthy(self):
        result = self.snapshot()
        require(not any(value for name, value in result["counts"].items() if name.endswith(".failures")),
                "request load failed: " + json.dumps(result))

    def set_phase(self, phase):
        self.healthy()
        with self.lock:
            self.phase = phase

    def finish(self):
        self.stop.set()
        self.pool.shutdown(wait=True)
        for future in self.futures:
            future.result()
        self.healthy()


class Qualification:
    def __init__(self):
        self.os = platform.system()
        self.candidates = json.loads((WORK / "candidates.json").read_text(encoding="utf-8"))
        self.a, self.b = self.candidates["versions"]
        self.port = free_port()
        self.load = None
        self.installed_service = False
        self.env = dict(os.environ)
        self.env.pop("OPEN_FERRY_SELF_UPDATE", None)
        if self.os == "Windows":
            import ctypes
            require(not ctypes.windll.shell32.IsUserAnAdmin(),
                    "the user qualification needs an unelevated token; the limited task did not provide one")
            self.config = Path(os.environ["APPDATA"]) / "open-ferry/config.yaml"
            self.exe = Path(os.environ["LOCALAPPDATA"]) / "Programs/open-ferry/open-ferry.exe"
            self.data = Path(os.environ["LOCALAPPDATA"]) / "open-ferry"
        else:
            self.config = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))) / "open-ferry/config.yaml"
            self.exe = Path.home() / ".local/bin/open-ferry"
            self.data = Path(os.environ.get("XDG_DATA_HOME", str(Path.home() / ".local/share"))) / "open-ferry"
        require(not self.config.exists() and not self.exe.exists() and not self.data.exists(),
                "qualification requires a fresh hosted account with no existing open-ferry install")

    def api(self, suffix="", method="GET"):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)
        try:
            connection.request(method, "/open-ferry/api/v1/update" + suffix,
                               headers={"Authorization": "Bearer " + self.management})
            response = connection.getresponse()
            body = response.read()
            require(200 <= response.status < 300, f"update API returned HTTP {response.status}")
            return json.loads(body)
        finally:
            connection.close()

    def until(self, name, predicate, seconds=90):
        deadline = time.monotonic() + seconds
        last = None
        while time.monotonic() < deadline:
            if self.load:
                self.load.healthy()
            try:
                last = self.api()
                write(OUT / "status.json", json.dumps(last, indent=2) + "\n")
                require(not last.get("last_error"), "update failed: " + str(last.get("last_error")))
                if predicate(last):
                    record("Verified " + name)
                    write(OUT / (name.replace(" ", "-") + ".json"), json.dumps(last, indent=2) + "\n")
                    return last
            except (OSError, http.client.HTTPException) as error:
                # Startup/status polls are separate from the zero-tolerance request load.
                last = {"status_poll_error": clean(error)}
            time.sleep(2)
        raise RuntimeError(f"timed out waiting for {name}: {clean(json.dumps(last))}")

    def stable(self, status, version, how=None):
        supervisor = status.get("supervisor") or {}
        last = supervisor.get("last_switch") or {}
        return (status.get("running_version") == version
                and status.get("installed_version") == version
                and supervisor.get("form") == "supervised"
                and supervisor.get("running_version") == version
                and supervisor.get("phase") == "idle"
                and not supervisor.get("stale") and not supervisor.get("error")
                and not supervisor.get("switching_disabled")
                and not supervisor.get("watch_until")
                and (how is None or (last.get("how") == how and last.get("to") == version
                     and last.get("from") == (self.a if how == "update" else self.b)
                     and last.get("result") == ("switched" if how == "update" else "rolled-back"))))

    def prerequisites(self):
        if self.os == "Linux":
            uid = str(os.getuid())
            runtime = f"/run/user/{uid}"
            command(["sudo", "loginctl", "enable-linger", os.environ["USER"]])
            command(["sudo", "systemctl", "start", f"user@{uid}.service"])
            self.env.update(XDG_RUNTIME_DIR=runtime, DBUS_SESSION_BUS_ADDRESS=f"unix:path={runtime}/bus")
            command(["systemctl", "--user", "show-environment"], env=self.env, sensitive=True)
            command(["systemctl", "is-system-running"], check=False)
            record("systemd user manager available; machine form omitted: root installer ignores the loopback mirror")
        elif self.os == "Darwin":
            command(["launchctl", "print", f"gui/{os.getuid()}"], sensitive=True)
            record("GUI launchd domain available; machine form is refused by the product")
        else:
            command(["schtasks.exe", "/query", "/tn", "open-ferry"], check=False)
            record("Unelevated interactive account available; machine form omitted: elevated installer ignores the loopback mirror")

    def install(self, base):
        self.env["OPEN_FERRY_INSTALL_BASE_URL"] = base
        self.env["OPEN_FERRY_UPDATE_BASE_URL"] = base
        record("Installing candidate A with the release install script (init output withheld)")
        if self.os == "Windows":
            args = ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                    "-File", str(Path.cwd() / "install.ps1"), "-NoAttestation", "-NoMigrate"]
        else:
            args = ["sh", str(Path.cwd() / "install.sh"), "--no-attestation", "--no-migrate"]
        command(args, env=self.env, sensitive=True, timeout=180)
        config = yaml.safe_load(self.config.read_text(encoding="utf-8"))
        self.management = config["management"]["secret-key"]
        self.client = config["access"]["api-keys"][0]
        SECRETS.extend((self.management, *config["access"]["api-keys"]))
        config["server"].update(host="127.0.0.1", port=self.port)
        config["self-update"] = {"mode": "auto"}
        # Keep generated client/management keys, and use an empty local auth directory.
        config["oauth"]["auth-dir"] = str(WORK / "auth")
        (WORK / "auth").mkdir()
        with self.config.open("w", encoding="utf-8", newline="\n") as output:
            yaml.safe_dump(config, output, sort_keys=False)
        # Supported service working-directory .env, shared by supervisor and workers.
        write(self.config.parent / ".env", f"OPEN_FERRY_UPDATE_BASE_URL={base}\n")
        require(command([self.exe, "--version"]).stdout.strip() == f"open-ferry {self.a}",
                "installer did not install A")
        receipt = json.loads((self.data / "install-receipt.json").read_text(encoding="utf-8"))
        require(receipt["version"] == self.a and Path(receipt["binary"]) == self.exe,
                "installer receipt does not identify candidate A")
        self.installed_service = True
        command([self.exe, "service", "install", "-config", self.config], env=self.env)
        command([self.exe, "service", "status"], env=self.env)
        if self.os == "Linux":
            definition = command(["systemctl", "--user", "cat", "open-ferry"], env=self.env).stdout
        elif self.os == "Darwin":
            definition = (Path.home() / f"Library/LaunchAgents/{LABEL}.plist").read_text(encoding="utf-8")
        else:
            definition = command(["schtasks.exe", "/query", "/tn", "open-ferry", "/xml"]).stdout
        require("-supervise" in definition and "-data-dir" in definition,
                "service manager definition is not the supervised form")
        write(OUT / "service-definition.txt", definition)
        status = self.until("supervised A", lambda value: self.stable(value, self.a))
        require(status.get("trusts_release_key") and status.get("can_update_itself") and status["mode"] == "auto",
                "A must trust a key, be able to update itself and run in auto mode")

    def processes(self):
        if self.os == "Windows":
            result = command(["tasklist.exe", "/fi", "IMAGENAME eq open-ferry.exe", "/fo", "csv", "/nh"])
            return '"open-ferry.exe"' in result.stdout.lower()
        result = command(["pgrep", "-x", "open-ferry"], check=False)
        require(result.returncode in (0, 1), "could not query remaining processes")
        return result.returncode == 0

    def uninstall(self):
        if self.load:
            self.load.finish()
            self.load = None
        command([self.exe, "service", "uninstall"], env=self.env, timeout=120)
        self.installed_service = False
        deadline = time.monotonic() + 60
        while self.processes():
            require(time.monotonic() < deadline, "uninstall left an open-ferry process running")
            time.sleep(1)
        with socket.socket() as listener:
            if self.os != "Windows":
                listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", self.port))
        require(not (self.data / "service.json").exists(), "uninstall left the service record")
        if self.os == "Linux":
            definition = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))) / "systemd/user/open-ferry.service"
            result = command(["systemctl", "--user", "show", "open-ferry", "--property=LoadState", "--value"],
                             env=self.env, check=False)
            require("not-found" in result.stdout and not definition.exists(), "systemd service still exists")
        elif self.os == "Darwin":
            result = command(["launchctl", "print", f"gui/{os.getuid()}/{LABEL}"], check=False)
            require(result.returncode != 0 and not (Path.home() / f"Library/LaunchAgents/{LABEL}.plist").exists(),
                    "LaunchAgent still exists")
        else:
            result = command(["schtasks.exe", "/query", "/tn", "open-ferry"], check=False)
            require(result.returncode != 0, "scheduled task still exists")
        record("Verified uninstall: no open-ferry processes, port free, service and service record gone")

    def logs(self):
        if self.os == "Linux":
            result = command(["journalctl", "--user", "-u", "open-ferry", "--no-pager", "-o", "cat"],
                             env=self.env, check=False, sensitive=True)
            write(OUT / "supervisor-worker.log", result.stdout)
        else:
            source = (Path.home() / "Library/Logs/open-ferry.log" if self.os == "Darwin"
                      else self.config.parent / "service.log")
            if source.exists():
                write(OUT / "supervisor-worker.log", source.read_text(encoding="utf-8", errors="replace"))
        for name in ("update-state.json", "switch-journal.json"):
            source = self.data / name
            if source.exists():
                write(OUT / name, source.read_text(encoding="utf-8"))

    def run(self):
        self.prerequisites()
        Mirror.latest = self.a
        server = ThreadingHTTPServer(("127.0.0.1", 0),
                                     functools.partial(Mirror, directory=str(WORK / "mirror")))
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            self.install(f"http://127.0.0.1:{server.server_port}")
            self.load = Load(self.port, self.client)
            time.sleep(3)
            self.load.set_phase("auto")
            with Mirror.lock:
                Mirror.latest = self.b
            # Only a check is requested: no install command or install API is used for A -> B.
            require(self.api("/check", "POST").get("check") in ("started", "running"),
                    "check-now was not accepted")
            self.until("automatic B after watch", lambda value: self.stable(value, self.b, "update"), seconds=900)
            # Durable outcomes currently report auto:null. The production worker's
            # explicit acceptance log proves this used its automatic switch path.
            self.logs()
            log = (OUT / "supervisor-worker.log").read_text(encoding="utf-8")
            require(any("automatic switch accepted" in line and self.b in line for line in log.splitlines()),
                    "B switched without the worker's automatic-switch acceptance evidence")
            require(command([self.exe, "--version"]).stdout.strip() == f"open-ferry {self.b}",
                    "automatic switch did not commit the installed B binary")
            self.load.set_phase("rollback")
            command([self.exe, "update", "-rollback", "-yes", "-config", self.config],
                    env=self.env, timeout=900)
            self.until("rolled back A after watch", lambda value: self.stable(value, self.a, "rollback"), seconds=900)
            self.load.healthy()
            counts = self.load.snapshot()["counts"]
            for phase in ("baseline", "auto", "rollback"):
                require(counts.get(f"{phase}.requests", 0) > 0, f"no requests covered {phase}")
            require(command([self.exe, "--version"]).stdout.strip() == f"open-ferry {self.a}",
                    "rollback did not restore the installed binary")
            self.uninstall()
        finally:
            try:
                if self.load:
                    self.load.finish()
            finally:
                self.load = None
                try:
                    self.logs()
                finally:
                    try:
                        if self.installed_service or (self.data / "service.json").exists():
                            command([self.exe, "service", "uninstall"], env=self.env, check=False, timeout=120)
                    finally:
                        server.shutdown()
                        server.server_close()
                        thread.join(timeout=10)
                        write(OUT / "mirror-requests.json", json.dumps(dict(Mirror.requests), indent=2) + "\n")


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    result = {"form": "user", "os": platform.system(), "passed": False}
    try:
        qualification = Qualification()
        result.update(qualification.candidates)
        qualification.run()
        result["passed"] = True
        record("PASS: supervised user service A -> automatic B -> rollback A -> uninstall, zero failed load requests")
    except Exception as error:
        result["error"] = clean(f"{type(error).__name__}: {error}")
        record("FAIL: " + result["error"])
    finally:
        write(OUT / "result.json", json.dumps(result, indent=2) + "\n")
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
