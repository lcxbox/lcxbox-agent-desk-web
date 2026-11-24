"""Exercise local browser and PTY recovery on a disposable Unix signaling host."""
import argparse
import contextlib
import http.client
import json
import os
from pathlib import Path
import pty
import re
import secrets
import select
import signal
import socket
import subprocess
import tempfile
import time


def change_in_terminal(binary, config, first, second):
    pid, tty = pty.fork()
    if pid == 0:
        os.execv(binary, [binary, "reset-password", "--config-file-path", str(config)])
    output = b""
    try:
        for prompt, value in [(b"New password:", first), (b"Confirm new password:", second)]:
            while prompt not in output:
                assert select.select([tty], [], [], 15)[0], "CLI prompt timed out"
                data = os.read(tty, 2048)
                assert data, "CLI exited before prompting"
                output += data
            os.write(tty, value.encode() + b"\n")
        while select.select([tty], [], [], 30)[0]:
            try:
                data = os.read(tty, 2048)
            except OSError:
                break
            if not data:
                break
            output += data
        assert first.encode() not in output and second.encode() not in output, "TTY echoed a password"
        assert (b"Password reset successfully." in output) == (first == second)
    finally:
        os.close(tty)
        os.waitpid(pid, 0)


def run(binary, root):
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    config = root / "config.toml"
    config.write_text(f'[system]\nport={port}\nlisten_addr_ipv4="127.0.0.1"\nenable_ipv6=false\nmanager_enabled=false\ntelemetry_consent=false\n[turn]\nenable_turn=false\n')
    config.chmod(0o600)
    command = [binary, "--startup-mode", "signaling", "--config-file-path", str(config), "--hidden"]
    origin = f"http://127.0.0.1:{port}"
    bootstrap = secrets.token_urlsafe(32)
    environment = {**os.environ, "LAD_BOOTSTRAP_TOKEN": bootstrap, "LAD_COOKIE_SECURE": "false"}
    passwords = [secrets.token_urlsafe(30) for _ in range(3)]
    recovery = "/api/auth/local-recovery/"

    def call(path, data=None, cookie="", headers=None):
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
        try:
            conn.request("GET" if data is None else "POST", path,
                         None if data is None else json.dumps(data),
                         {"Origin": origin, "Content-Type": "application/json", "Cookie": cookie, **(headers or {})})
            response = conn.getresponse()
            return json.loads(response.read()), (response.getheader("Set-Cookie") or "").split(";")[0]
        finally:
            conn.close()

    def assert_stale(cookie):
        assert not call("/api/auth/me", cookie=cookie)[0]["success"]
        for path in ["/api/desk/signaling", "/api/desk/exec-pty", "/api/desk/terminal/test-device"]:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
            try:
                conn.request("GET", path, headers={"Origin": origin, "Cookie": cookie})
                assert conn.getresponse().status == 401, path
            finally:
                conn.close()

    def login(pwd):
        return call("/api/auth/login", {"username": "owner", "password": pwd})

    def field(name):
        return re.search(rf'^{name} = "([^"]+)"$', config.read_text(), re.MULTILINE)[1]

    @contextlib.contextmanager
    def running():
        with (root/"host.log").open("ab") as log:
            proc = subprocess.Popen(command, env=environment, cwd=root, stdout=log, stderr=log)
            try:
                for _ in range(120):
                    assert proc.poll() is None, "fixture host exited"
                    try:
                        call("/api/init/requirements")
                        break
                    except OSError:
                        time.sleep(.25)
                else:
                    raise AssertionError("fixture startup timed out")
                yield
                assert proc.poll() is None, "host exited during reset"
            finally:
                proc.send_signal(signal.SIGTERM)
                try:
                    proc.wait(timeout=6)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()

    with running():
        args = dict(username="owner", password=passwords[0], telemetry_consent=False,
                    host_access_indicator_enabled=True, bootstrap_token=bootstrap)
        assert call("/api/init", args)[0]["success"]
        first_hash = field("login_password_hash")
        assert first_hash.startswith("$argon2id$")
        logged_in, owner = login(passwords[0])
        assert logged_in["success"] and call("/api/auth/me", cookie=owner)[0]["success"]
        for bad_header in [{"Origin": "https://example.invalid"}, {"X-Forwarded-For": "127.0.0.1"}]:
            assert not call(recovery+"start", {}, headers=bad_header)[0]["success"]
        issued, browser = call(recovery+"start", {})
        assert issued["success"], issued.get("message")
        assert Path(issued["data"]["config_file"]) == config.resolve()
        code = field("password_reset_code")
        assert code.isdigit() and len(code) == 8 and code not in json.dumps(issued)
        finish = dict(code=code, new_password=passwords[1])
        assert not call(recovery+"finish", finish)[0]["success"]
        assert call(recovery+"finish", finish, browser)[0]["success"]
        assert not call(recovery+"finish", finish, browser)[0]["success"]
        assert_stale(owner)
        assert not login(passwords[0])[0]["success"]
        assert "password_reset_code" not in config.read_text()
        assert field("login_password_hash").split("$")[4] != first_hash.split("$")[4]
    with running():
        logged_in, owner = login(passwords[1])
        assert logged_in["success"]
        issued, browser = call(recovery+"start", {})
        assert issued["success"]
        finish = dict(code=field("password_reset_code"), new_password=passwords[0])
        no_tty = subprocess.run([binary, "reset-password", "--config-file-path", str(config)],
                                stdin=subprocess.DEVNULL, capture_output=True, timeout=3)
        assert no_tty.returncode and b"interactive terminal" in no_tty.stderr
        change_in_terminal(binary, config, passwords[2], passwords[1])
        assert call("/api/auth/me", cookie=owner)[0]["success"]
        change_in_terminal(binary, config, passwords[2], passwords[2])
        assert not call(recovery+"finish", finish, browser)[0]["success"]
        assert_stale(owner)
        assert not login(passwords[1])[0]["success"] and login(passwords[2])[0]["success"]
        assert "password_reset_code" not in config.read_text()
    with running():
        assert login(passwords[2])[0]["success"]
    for pwd in passwords:
        assert pwd not in config.read_text() and pwd not in (root/"host.log").read_text()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="desk-reset-", dir="/tmp") as directory:
        run(str(args.binary.resolve(strict=True)), Path(directory).resolve())
    print("Browser recovery, stale cookies, CLI reset, and restart passed.")
