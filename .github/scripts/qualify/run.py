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
SIGNATURE_ERRORS = {
    "untrusted-key": "SHA256SUMS is signed with a key this build doesn't trust",
    "bad-signature": "SHA256SUMS doesn't match its signature",
}


class UpdateFailure(RuntimeError):
    """The updater's own error, kept distinct from harness failures."""


class QualificationFailure(RuntimeError):
    def __init__(self, failures):
        self.failures = failures
        super().__init__("; ".join(f"{phase}: {type(error).__name__}: {error}"
                                 for phase, error in failures))


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
        if sensitive:
            # May contain init keys: retain privately in WORK, outside the
            # diagnostics allowlist, and never echo it into logs or errors.
            write(WORK / "private-command-failure.txt", result.stdout)
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
        self.phase_started = time.monotonic()
        self.phase_seconds = Counter()
        self.next_source = 2
        self.pool = ThreadPoolExecutor(max_workers=4)
        self.futures = [self.pool.submit(self.loop) for _ in range(4)]

    def connection(self):
        options = {}
        if platform.system() == "Windows":
            # Winsock can choose a source port whose previous identical TCP
            # tuple is still in TIME_WAIT, then fail connect with 10048. Use a
            # new address in the existing 127/8 loopback range per connection:
            # the tuple cannot repeat. No OS settings, retries, connection
            # reuse, abortive closes, rate or timeout changes are involved.
            with self.lock:
                number = self.next_source
                self.next_source += 1
            require(number < 0xFFFFFF, "Windows loopback source-address space exhausted")
            source = f"127.{number >> 16}.{(number >> 8) & 255}.{number & 255}"
            options["source_address"] = (source, 0)
        return http.client.HTTPConnection("127.0.0.1", self.port, timeout=5, **options)

    def loop(self):
        while not self.stop.is_set():
            with self.lock:
                phase = self.phase
            connection = self.connection()
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
            timings = dict(self.phase_seconds)
            timings[self.phase] = timings.get(self.phase, 0) + time.monotonic() - self.phase_started
            result = {"counts": dict(self.counts), "errors": list(self.errors), "phase_seconds": timings}
        write(OUT / "requests.json", json.dumps(result, indent=2) + "\n")
        return result

    def healthy(self):
        result = self.snapshot()
        require(not any(value for name, value in result["counts"].items() if name.endswith(".failures")),
                "request load failed: " + json.dumps(result))

    def set_phase(self, phase):
        self.healthy()
        with self.lock:
            now = time.monotonic()
            self.phase_seconds[self.phase] += now - self.phase_started
            self.phase_started = now
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
        require(self.candidates["fault"] in ("none", *SIGNATURE_ERRORS), "unknown qualification fault")
        self.a, self.b = self.candidates["versions"]
        self.port = free_port()
        self.load = None
        self.installed_service = False
        self.fault_confirmed = False
        self.env = dict(os.environ)
        self.env.pop("OPEN_FERRY_SELF_UPDATE", None)
        if self.os == "Windows":
            # Unexercised since 962f661f: no hosted unelevated interactive logon.
            import ctypes
            require(not ctypes.windll.shell32.IsUserAnAdmin(),
                    "the user qualification needs an unelevated token; the limited task did not provide one")
            self.config = WORK / "config/config.yaml"
            self.exe = Path(os.environ["LOCALAPPDATA"]) / "Programs/open-ferry/open-ferry.exe"
            self.data = Path(os.environ["LOCALAPPDATA"]) / "open-ferry"
        else:
            self.config = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))) / "open-ferry/config.yaml"
            self.exe = Path.home() / ".local/bin/open-ferry"
            self.data = Path(os.environ.get("XDG_DATA_HOME", str(Path.home() / ".local/share"))) / "open-ferry"
        # claude-cli is unrelated user data. Never move, delete or write in it.
        data_clear = (not self.data.exists() or (self.os == "Windows" and self.data.is_dir()
                      and all(entry.name == "claude-cli" and entry.is_dir()
                              and not entry.is_symlink() for entry in self.data.iterdir())))
        require(not self.config.exists() and not self.exe.exists() and data_clear,
                "qualification requires no existing open-ferry install or update records")
        if self.os == "Windows":
            require(not (Path(os.environ["APPDATA"]) / "open-ferry").exists(),
                    "qualification refuses an existing user config directory")

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
                if last.get("last_error"):
                    raise UpdateFailure("update failed: " + str(last["last_error"]))
                if predicate(last):
                    record("Verified " + name)
                    write(OUT / (name.replace(" ", "-") + ".json"), json.dumps(last, indent=2) + "\n")
                    return last
            except (OSError, http.client.HTTPException) as error:
                # Startup/status polls are separate from the zero-tolerance request load.
                last = {"status_poll_error": clean(error)}
            time.sleep(2)
        raise RuntimeError(f"timed out waiting for {name}: {clean(json.dumps(last))}")

    def check_after_flip(self, seconds=90):
        deadline = time.monotonic() + seconds
        attempts = 0
        while time.monotonic() < deadline:
            self.load.healthy()
            answer = self.api("/check", "POST").get("check")
            attempts += 1
            require(answer in ("started", "running"), "check-now was not accepted")
            if answer == "started":
                record(f"Confirmed a check started after latest moved to B ({attempts} attempts)")
                return
            # An existing check may have read A before the flip. Only 'started'
            # confirms a new check; 'running' never satisfies this gate.
            time.sleep(2)
        raise RuntimeError("timed out starting a new check after latest moved to B")

    def verify_signature_control(self, error):
        fault = self.candidates["fault"]
        expected = SIGNATURE_ERRORS[fault]
        require(type(error) is UpdateFailure and str(error) == "update failed: " + expected,
                f"{fault} control did not produce the exact expected updater error")
        status = self.api()
        write(OUT / "signature-rejected.json", json.dumps(status, indent=2) + "\n")
        require(status.get("last_error") == expected and status.get("last_result") == "error"
                and not status.get("checking") and self.stable(status, self.a)
                and not status.get("staged_version")
                and not (status.get("supervisor") or {}).get("last_switch"),
                f"{fault} control did not leave supervised A unchanged")
        self.load.healthy()
        counts = self.load.snapshot()["counts"]
        for phase in ("baseline", "auto"):
            require(counts.get(f"{phase}.requests", 0) > 0, f"no requests covered {phase}")
        suffix = ".zip" if self.os == "Windows" else ".tar.gz"
        archive = f"/download/v{self.b}/open-ferry-{self.b}-{self.candidates['target']}{suffix}"
        with Mirror.lock:
            requests = dict(Mirror.requests)
        require(requests.get("/latest/download/SHA256SUMS.minisig", 0) > 0,
                f"{fault} control never requested a signature")
        require(requests.get(archive, 0) == 0, "untrusted B archive was requested")
        self.fault_confirmed = True
        record(f"Verified exact {fault} rejection: supervised A unchanged, B archive never requested")

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
            # Unexercised since 962f661f: no hosted unelevated interactive logon.
            args = ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                    "-File", str(Path.cwd() / "install.ps1"), "-NoAttestation", "-NoMigrate",
                    "-InstallDir", str(self.exe.parent), "-ConfigPath", str(self.config)]
        else:
            args = ["sh", str(Path.cwd() / "install.sh"), "--no-attestation", "--no-migrate"]
        command(args, env=self.env, sensitive=True, timeout=180)
        try:
            config = yaml.safe_load(self.config.read_text(encoding="utf-8"))
        except yaml.YAMLError:
            # Parser diagnostics include source snippets before keys are registered.
            raise RuntimeError("generated config is not valid YAML") from None
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
            # Shared desktops may have unrelated source-tree tests running.
            result = command(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                              "Get-CimInstance Win32_Process -Filter \"Name='open-ferry.exe'\" | "
                              "Select-Object -ExpandProperty ExecutablePath"])
            return any(Path(path.strip()).is_relative_to(root)
                       for path in result.stdout.splitlines() if path.strip()
                       for root in (self.exe.parent, self.data))
        result = command(["pgrep", "-x", "open-ferry"], check=False)
        require(result.returncode in (0, 1), "could not query remaining processes")
        return result.returncode == 0

    def uninstall(self):
        if self.load:
            load = self.load
            self.load = None
            load.finish()
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
        require(server.server_port != 8317, "refusing the reserved development port for the mirror")
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        failures = []
        try:
            self.install(f"http://127.0.0.1:{server.server_port}")
            self.load = Load(self.port, self.client)
            time.sleep(3)
            self.load.set_phase("auto")
            with Mirror.lock:
                Mirror.latest = self.b
            # Only a check is requested: no install command or install API is used for A -> B.
            self.check_after_flip()
            try:
                self.until("automatic B after watch", lambda value: self.stable(value, self.b, "update"), seconds=900)
            except UpdateFailure as error:
                if self.candidates["fault"] not in SIGNATURE_ERRORS:
                    raise
                self.verify_signature_control(error)
            if self.candidates["fault"] in SIGNATURE_ERRORS:
                require(self.fault_confirmed, f"{self.candidates['fault']} control unexpectedly switched to B")
                self.uninstall()
                return
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
        except Exception as error:
            # Preserve the first error before cleanup can raise another one.
            failures.append(("lifecycle", error))
            record(f"Lifecycle failed: {type(error).__name__}: {error}")
            if self.os == "Windows":
                try:
                    load = command(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                                    "Get-CimInstance Win32_Processor | "
                                    "Select-Object Name,LoadPercentage,NumberOfLogicalProcessors | ConvertTo-Json"],
                                   sensitive=True)
                    write(OUT / "system-load.json", load.stdout)
                except Exception as load_error:
                    record(f"Could not capture system load: {type(load_error).__name__}: {load_error}")
        finally:
            def finish_load():
                if self.load:
                    load = self.load
                    self.load = None
                    load.finish()

            def remove_service():
                if self.installed_service or (self.data / "service.json").exists():
                    self.uninstall()

            def stop_mirror():
                server.shutdown()
                server.server_close()
                thread.join(timeout=10)
                with Mirror.lock:
                    requests = dict(Mirror.requests)
                write(OUT / "mirror-requests.json", json.dumps(requests, indent=2) + "\n")
                if self.fault_confirmed:
                    require(not any(path.startswith(f"/download/v{self.b}/") and count
                                    for path, count in requests.items()),
                            f"{self.candidates['fault']} control requested a B asset before mirror shutdown")

            for phase, cleanup in (("load cleanup", finish_load), ("diagnostics", self.logs),
                                   ("service cleanup", remove_service), ("mirror cleanup", stop_mirror)):
                try:
                    cleanup()
                except Exception as error:
                    failures.append((phase, error))
                    record(f"{phase} failed: {type(error).__name__}: {error}")
            if failures:
                raise QualificationFailure(failures) from failures[0][1]


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    result = {"form": "user", "os": platform.system(), "passed": False}
    try:
        qualification = Qualification()
        result.update(qualification.candidates)
        qualification.run()
        result["passed"] = True
        if qualification.candidates["fault"] in SIGNATURE_ERRORS:
            result["expected_failure"] = SIGNATURE_ERRORS[qualification.candidates["fault"]]
            result["fault_confirmed"] = qualification.fault_confirmed
            record(f"PASS: exact {qualification.candidates['fault']} rejection, B archive never requested, "
                   "zero failed load requests, uninstalled")
        else:
            record("PASS: supervised user service A -> automatic B -> rollback A -> uninstall, zero failed load requests")
    except Exception as error:
        failures = error.failures if isinstance(error, QualificationFailure) else [("setup", error)]
        result["errors"] = [{"phase": phase, "error": clean(f"{type(failure).__name__}: {failure}")}
                            for phase, failure in failures]
        result["error"] = result["errors"][0]["error"]
        record("FAIL: " + result["error"])
    finally:
        write(OUT / "result.json", json.dumps(result, indent=2) + "\n")
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
