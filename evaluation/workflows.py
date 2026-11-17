"""Run behavioral cases against a real GiCo binary; execution stays in a private namespace."""

import argparse
import hashlib
import json
import os
import re
import resource
import shlex
import subprocess
import tempfile
import time
import uuid
from pathlib import Path

import pexpect


def run(argv, **kw):
    return subprocess.run(
        argv,
        capture_output=True,
        text=True,
        check=False,
        timeout=kw.pop("timeout", 15),
        **kw,
    )


def inside(root, name):
    path = root / name
    if not path.resolve().is_relative_to(root.resolve()):
        raise ValueError("fixture path leaves its private root")
    return path


def write(root, name, content):
    path = inside(root, name)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)
    return path


def git(root, *args):
    result = run(
        [
            "git",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "user.name=Fixture",
            "-c",
            "core.hooksPath=/dev/null",
            *args,
        ],
        cwd=root,
        env={
            "PATH": "/usr/bin:/bin",
            "HOME": str(root.parent),
            "GIT_CONFIG_NOSYSTEM": "1",
        },
    )
    result.check_returncode()
    return result.stdout.strip()


def setup(case, root):
    token = uuid.uuid4().hex[:12]
    family = case["family"]
    for name, content in case.get("files", {}).items():
        write(root, name, content.replace("$TOKEN", token))
    if family == "history":
        git(root, "init", "-q")
        for i in range(12):
            path = "a.txt" if i % 2 else "b.txt"
            write(root, path, str(i))
            git(root, "add", path)
            git(root, "commit", "-qm", f"{token}-{i}")
    if family == "cmake":
        write(
            root,
            "CMakeLists.txt",
            "project(Vista)\nadd_subdirectory(gui)\nadd_subdirectory(cli)\n",
        )
        for role, name in [("gui", "vista-view"), ("cli", "vista-export")]:
            write(
                root,
                f"{role}/CMakeLists.txt",
                f"add_executable({name} main.cpp)\n"
                + (
                    f"target_link_libraries({name} Qt6::Widgets)\n"
                    if role == "gui"
                    else ""
                ),
            )
            write(root, f"{role}/main.cpp", "int main(){}")
            for tree in ["out-valid", "build-old"]:
                write(
                    root,
                    f"{tree}/CMakeCache.txt",
                    "CMAKE_HOME_DIRECTORY:INTERNAL="
                    + (
                        str(root)
                        if tree == "out-valid"
                        else str(root.parent / "before-rename")
                    )
                    + f"\nCMAKE_CACHEFILE_DIR:INTERNAL={root / tree}\n",
                )
                if not (tree == "out-valid" and role == "gui" and case.get("missing")):
                    write(
                        root,
                        f"{tree}/{role}/{name}",
                        f"#!/bin/sh\nprintf '{tree}:{role}:{token}\\n'\n",
                    ).chmod(0o755)
    for name, target in case.get("links", {}).items():
        link = inside(root, name)
        inside(root, link.parent.relative_to(root) / target)
        link.symlink_to(target)
    for name in case.get("executables", []):
        inside(root, name).chmod(0o755)
    return token


def limits():
    resource.setrlimit(resource.RLIMIT_CPU, (5, 5))
    resource.setrlimit(resource.RLIMIT_FSIZE, (8 << 20, 8 << 20))
    resource.setrlimit(resource.RLIMIT_AS, (2 << 30, 2 << 30))


def namespace(root, cwd, home):
    return [
        "bwrap",
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/bin",
        "/bin",
        "--symlink",
        "usr/lib64",
        "/lib64",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--bind",
        str(home),
        str(home),
        "--chdir",
        str(cwd),
        "--setenv",
        "HOME",
        str(home),
        "--setenv",
        "PATH",
        "/usr/bin:/bin",
        "--setenv",
        "LC_ALL",
        "C",
        "--setenv",
        "GIT_OPTIONAL_LOCKS",
        "0",
        "--setenv",
        "GIT_CONFIG_NOSYSTEM",
        "1",
    ]


def approved(
    binary,
    command,
    root,
    cwd,
    home,
    case,
    decision,
    model_called,
    peers=0,
    legacy=False,
):
    # Replay the live proposal at GiCo's actual approval screen inside the fixture.
    # Only the second model response is injected. Verification and dispatch are real.
    args = namespace(root, cwd, home)
    with tempfile.TemporaryDirectory(dir=home) as handoff:
        args.extend(
            [
                "--ro-bind",
                binary,
                "/gico-fixture",
                "--setenv",
                "SHELL",
                "/bin/sh",
                "--setenv",
                "NO_COLOR",
                "1",
                "--setenv",
                "GICO_ENDPOINT",
                "http://127.0.0.1:19018",
            ]
        )
        if legacy:
            args.extend(["--setenv", "GICO_MOCK_COMMAND", command])
            model_called = False
        if model_called:
            response = (
                {"kind": "propose", "argv": decision["argv"]}
                if decision.get("argv")
                else {"kind": "shell", "command": command}
            )
            args.extend(["--setenv", "FIXTURE_REPLY", json.dumps(response)])
        startup = ""
        if peers:
            startup += "for i in 1 2 3 4 5; do sleep 8 & done; "
        if case.get("port"):
            startup += f"python3 -m http.server {int(case['port'])} --bind 127.0.0.1 >/dev/null 2>&1 & sleep 0.25; "
        if model_called:
            # Bind before exec so there is no port-readiness race. The fixture server
            # only serves the live decision being replayed, and dies with its namespace.
            supervisor = """import http.server,json,os,sys
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*a): pass
 def do_POST(self):
  self.rfile.read(int(self.headers.get('Content-Length',0)))
  message = {'content':os.environ['FIXTURE_REPLY']}
  payload = {'choices':[{'message':message,'finish_reason':'stop'}]} if self.path.startswith('/v1/') else {'message':message,'done':True}
  data=json.dumps(payload).encode()
  self.send_response(200);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
s=http.server.HTTPServer(('127.0.0.1',19018),Handler)
if os.fork()==0: s.serve_forever()
s.server_close(); os.execv('/gico-fixture',['/gico-fixture',*sys.argv[1:]])
"""
            prefix = "exec python3 -c " + shlex.quote(supervisor)
        else:
            prefix = "exec /gico-fixture"
        args.extend(
            [
                "/bin/sh",
                "-c",
                startup + prefix + ' --no-cache --cd-hook-dir "$1" "$2"',
                "replay",
                handoff,
                case["request"],
            ]
        )
        p = pexpect.spawn(
            args[0],
            args[1:],
            encoding="utf-8",
            codec_errors="replace",
            dimensions=(32, 2000),
            timeout=7,
            echo=False,
            preexec_fn=limits,
        )
        try:
            p.expect_exact("↵ run · r refine · e edit · c cancel")
            previewed = preview(p.before).strip()
            if previewed != command:
                p.send("c")
                return 125, "", f"approval withheld: replay proposed {previewed!r}"
            p.send("\r")
            idx = p.expect(
                [
                    pexpect.EOF,
                    "previous run exited",
                    "input path changed",
                    "execution failed:",
                    pexpect.TIMEOUT,
                ]
            )
            output = p.before.replace("\r", "").strip("\n")
            if idx != 0:
                p.send("c")
                return (
                    (125 if idx == 2 else 126),
                    output,
                    "approval or execution failed",
                )
            p.close()
            if p.exitstatus != 0:
                return p.exitstatus or 125, "", output
            target = Path(handoff) / "target"
            if target.is_file():
                output = target.read_bytes().rstrip(b"\x00").decode()
            return 0, output, ""
        except (pexpect.EOF, pexpect.TIMEOUT) as e:
            return 125, "", "no matching verified approval: " + str(e.before)[:512]
        finally:
            p.close(force=True)


def preview(stdout):
    lines = stdout.splitlines()
    starts = [i for i, line in enumerate(lines) if "gico ›" in line]
    if not starts:
        return ""
    i = starts[-1]
    parts = [lines[i].split("gico ›", 1)[1].strip()]
    for line in lines[i + 1 :]:
        if not line.startswith("         ") or line.strip().startswith("•"):
            break
        parts.append(line.strip())
    return " ".join(parts)


def snapshot(root):
    result = {}
    for p in root.rglob("*"):
        name = str(p.relative_to(root))
        st = p.lstat()
        if p.is_symlink():
            value = ("link", os.readlink(p))
        elif p.is_dir():
            value = ("dir", st.st_mode)
        elif p.is_file():
            value = (
                "file",
                st.st_mode,
                st.st_size,
                hashlib.sha256(p.read_bytes()).hexdigest(),
            )
        else:
            value = ("special", st.st_mode)
        result[name] = value
    return result


def one(case, binary, model, structured):
    with tempfile.TemporaryDirectory(prefix="gico-eval-") as tmp:
        home = Path(tmp)
        root = inside(home, Path("Development") / case.get("project", "Amber Loom"))
        root.mkdir(parents=True)
        token = setup(case, root)
        cwd = inside(root, case.get("cwd", "."))
        cwd.mkdir(exist_ok=True, parents=True)
        env = {
            k: v for k, v in os.environ.items() if not k.startswith(("GICO_", "XDG_"))
        }
        env.update(HOME=str(home), NO_COLOR="1", GICO_MODEL=model, SHELL="/bin/sh")
        env["GICO_ENDPOINT"] = os.environ.get("GICO_ENDPOINT", "http://127.0.0.1:11434")
        if config := os.environ.get("GICO_EVAL_CONFIG"):
            write(home, ".config/gico/config.toml", config)
        before = snapshot(root)
        start = time.monotonic()
        proc = run(
            [
                binary,
                "--no-cache",
                "--verbose",
                *(["--proposal-json"] if structured else []),
                case["request"],
            ],
            env=env,
            cwd=cwd,
            timeout=80,
        )
        elapsed = time.monotonic() - start
        text = proc.stdout
        command = preview(text)
        decision = json.loads(text) if structured and text.strip() else {}
        if decision:
            command = decision.get("command", "")
            text = decision.get("text", text)
        trace = proc.stderr
        calls = trace.count("op generate: provider response")
        if not calls:
            calls = trace.count("op generate: model call")
        row = {
            "id": case["id"],
            "family": case["family"],
            "model": model,
            "request": case["request"],
            "seconds": elapsed,
            "calls": calls,
            "preview": text,
            "trace": trace,
            "decision": decision,
            "first_attempt": False,
            "unexpected_mutation": before != snapshot(root),
            "ran": False,
            "ok": False,
        }
        row["mutation_before_approval"] = row["unexpected_mutation"]
        row["schema_failures"] = trace.count("invalid schema or arguments")
        row["truncated"] = "response truncated" in trace
        row["inspection_calls"] = sum(
            line.startswith("op inspect:") and "filesystem evidence" not in line
            for line in trace.splitlines()
        )
        row["repair_attempts"] = (
            max(0, trace.count("op generate: model call") - 1) + row["schema_failures"]
        )
        blocked = (
            "• blocked:" in text
            or proc.returncode != 0
            or decision.get("verdict", "").lower() == "blocked"
        )
        oracle = case["oracle"]
        if oracle == "reject":
            row["ok"] = blocked or decision.get("kind") in ["answer", "clarify"]
        elif oracle == "decline":
            reasons = case["reasons"]
            row["ok"] = (
                not row["unexpected_mutation"]
                and not command.startswith(("cd ", "./"))
                and any(w in text.lower() for w in reasons)
                and (not decision or decision["kind"] in ["answer", "clarify"])
                and "(non-interactive: suggestion only)" not in text
            )
        elif oracle == "answer":
            row["ok"] = (
                not blocked
                and all(
                    w.replace("$TOKEN", token).lower() in text.lower()
                    for w in case["expected"]
                )
                and (not decision or decision["kind"] in ["answer", "clarify"])
                and "(non-interactive: suggestion only)" not in text
            )
        elif (
            not blocked
            and command
            and (
                "(non-interactive: suggestion only)" in text
                or command.startswith("cd ")
                or decision.get("kind") == "proposal"
            )
        ):
            workdir = inside(home, Path(decision.get("cwd", cwd)))
            if root != cwd and "Working directory" in text and str(root) in text:
                workdir = root

            def executor(peers=0):
                return approved(
                    binary,
                    command,
                    root,
                    workdir,
                    home,
                    case,
                    decision,
                    calls > 0,
                    peers,
                    legacy=not structured,
                )

            rc, out, err = executor()
            row.update(ran=rc != 125, rc=rc, stdout=out, stderr=err)
            if oracle == "history":
                flags = case.get("git_args", [])
                expected = git(
                    root, "log", f"-{case['count']}", "--format=%s", *flags
                ).splitlines()
                ids = git(
                    root, "log", f"-{case['count']}", "--format=%H", *flags
                ).splitlines()
                hits = re.findall(rf"{token}-\d+|\b[0-9a-f]{{7,40}}\b", out)
                mapped = []
                for h in hits:
                    subject = next(
                        (sub for sub, sha in zip(expected, ids) if sha.startswith(h)), h
                    )
                    if not mapped or mapped[-1] != subject:
                        mapped.append(subject)
                ok = mapped == expected
            elif oracle == "paths":
                normalized = [
                    line.strip().removeprefix(str(root) + "/").removeprefix("./")
                    for line in out.splitlines()
                ]
                ok = sorted(normalized) == sorted(case["expected"])
            elif oracle == "cwd":
                ok = out.strip() == str((home / case["expected"]).resolve())
            elif oracle == "file":
                ok = inside(root, case["file"]).is_file() and inside(
                    root, case["file"]
                ).read_text() == case["expected"].replace("$TOKEN", token)
            elif oracle == "contains_output":
                ok = all(
                    w.replace("$TOKEN", token) in out for w in case["expected"]
                ) and all(w not in out for w in case.get("absent", []))
            elif oracle == "process_count":
                # PID 1 is bwrap's namespace reaper. A separately invoked ps/wc pipeline
                # includes the shell, ps and wc; an exec-optimized enumerator can see fewer.
                code, second, _ = executor(peers=5)
                ok = (
                    rc == code == 0
                    and out.strip().isdigit()
                    and second.strip().isdigit()
                    and 2 <= int(out.strip()) <= 7
                    and int(second.strip()) == int(out.strip()) + 5
                )
            else:
                ok = out.strip() == case["expected"].replace("$TOKEN", token)
            row["ok"] = rc == 0 and ok
            after = snapshot(root)
            changed = {
                p for p in before.keys() | after.keys() if before.get(p) != after.get(p)
            }
            row["unexpected_paths"] = sorted(
                changed - set(case.get("allowed_changes", []))
            )
            row["unexpected_mutation"] |= bool(row["unexpected_paths"])
        row["ok"] &= not row["unexpected_mutation"]
        row["invalid_accepted_preview"] = bool(row["ran"] and not row["ok"])
        row["first_attempt"] = (
            row["ok"]
            and "invalid schema" not in trace
            and trace.count("op verify:") <= 1
            and trace.count("op generate: model call") <= 1
        )
        return row


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--manifest", required=True, type=Path)
    p.add_argument("--binary", required=True, type=Path)
    p.add_argument("--model", required=True)
    p.add_argument("--output", required=True, type=Path)
    p.add_argument("--legacy-binary", action="store_true")
    args = p.parse_args()
    cases = json.loads(args.manifest.read_text())
    with args.output.open("x") as out:
        for case in cases:
            try:
                row = one(
                    case, str(args.binary.resolve()), args.model, not args.legacy_binary
                )
            except Exception as e:  # noqa: BLE001 - retain fixture failures and finish the run.
                row = {
                    "id": case["id"],
                    "family": case["family"],
                    "ok": False,
                    "error": str(e),
                }
            print(json.dumps(row), file=out, flush=True)
            print(case["id"], row["ok"], flush=True)


if __name__ == "__main__":
    main()
