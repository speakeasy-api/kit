#!/usr/bin/env python3
"""Opt-in Kit native AHP/compose live smoke in a NEW named HerdR pane only.

Build first: mise run test -- --test ahp_herdr --no-run
Uses the real local work/.agents/hooks.json discovery path and isolates HOME for
only the test process. The named HerdR server retains its real HOME.
No compilation or manifest changes are performed by this script.
"""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import time
import uuid


def main():
    repo = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test-executable", type=Path)
    parser.add_argument("--bridge", type=Path, default=repo.parent / "herdr-ahp/target/release/herdr-ahp")
    args = parser.parse_args()
    if os.environ.get("HERDR_ENV") != "1":
        raise RuntimeError("Requires inherited HerdR detection; no focused-session fallback")
    herdr = shutil.which("herdr")
    if not herdr:
        raise RuntimeError("herdr is not on PATH")
    executable = args.test_executable
    if executable is None:
        candidates = [p for p in (repo / "target/debug/deps").glob("ahp_herdr-*") if p.is_file() and os.access(p, os.X_OK) and p.suffix != ".d"]
        if not candidates:
            raise RuntimeError("Compile tests/ahp_herdr.rs with mise run test -- --test ahp_herdr --no-run first")
        executable = max(candidates, key=lambda p: p.stat().st_mtime)
    executable = executable.resolve()
    bridge = args.bridge.resolve()
    if not executable.is_file() or not bridge.is_file():
        raise RuntimeError("Test executable and built generic bridge must exist")
    case = "native_ahp_herdr_real_compose"
    listed = subprocess.run([str(executable), "--list"], capture_output=True, text=True, timeout=10)
    if listed.returncode or case + ": test" not in listed.stdout:
        raise RuntimeError("Executable does not expose the expected native Kit smoke case")
    name = "kit-ahp-e2e-" + uuid.uuid4().hex[:10]
    root = Path(tempfile.mkdtemp(prefix="kit-ahp-herdr-live-"))
    gates = root / "gates"
    gates.mkdir(mode=0o700)
    isolated_home = root / "isolated-home"
    isolated_home.mkdir(mode=0o700)
    work = gates / "work"
    (work / ".agents").mkdir(parents=True, mode=0o700)
    env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_")}
    prefix = [herdr, "--session", name]

    def cli(*words):
        result = subprocess.run(prefix + list(words), env=env, capture_output=True, text=True, timeout=15)
        if result.returncode:
            raise RuntimeError(result.stderr[:1500])
        return json.loads(result.stdout) if result.stdout.strip() else None

    def wait(check, message):
        deadline = time.monotonic() + 40
        while time.monotonic() < deadline:
            value = check()
            if value:
                return value
            if (root / "test-exit").exists():
                stderr = (root / "test.stderr").read_text()[-1500:]
                stdout = (root / "test.stdout").read_text()[-1500:]
                raise RuntimeError(message + ": test exited\n" + stdout + stderr)
            time.sleep(0.05)
        raise RuntimeError(message)

    status = cli("status", "--json")["server"]
    socket = status["socket"]
    if status["running"] or socket == os.environ.get("HERDR_SOCKET_PATH") or ("/sessions/" + name + "/") not in socket:
        raise RuntimeError("Refusing non-isolated or running session")
    if Path(socket).parent.exists():
        raise RuntimeError("Refusing existing stopped-session directory")
    evidence = {"session": name, "socket_is_isolated": True, "test_executable": str(executable), "bridge": str(bridge), "checks": []}
    started = False
    try:
        with (root / "server.log").open("w") as log:
            server = subprocess.Popen(prefix + ["server"], env=env, stdout=log, stderr=log, start_new_session=True)
        started = True
        wait(lambda: cli("status", "--json")["server"]["running"], "Named headless server did not start")
        evidence["version"] = cli("status", "--json")["server"]["version"]
        created = cli("workspace", "create", "--cwd", str(repo), "--label", "kit-native-ahp-isolated-smoke", "--no-focus")
        pane_id = created["result"]["root_pane"]["pane_id"]
        evidence["pane"] = pane_id
        (gates / "scope.json").write_text(json.dumps({"session": name, "socket": socket, "pane": pane_id, "home": str(isolated_home)}))

        def pane():
            return cli("pane", "get", pane_id)["result"]["pane"]

        context_path = root / "bridge-context.json"
        context_path.write_text(json.dumps({"source": "org.example.kit.native-herdr-smoke", "agent": "kit-smoke",
            "sequence_file": str(root / "sequence"), "resume_argv": ["sh", "-c", "printf isolated-kit-smoke-{session_id}"], "release_on_eof": False}))
        config_path = work / ".agents/hooks.json"
        names = ["session.start", "turn.start", "turn.end"]
        config_path.write_text(json.dumps({"protocolVersion": "draft", "hooks": [{
            "id": "org.example.kit.native-herdr-smoke", "transport": {"type": "stdio", "command": str(bridge),
                "args": ["--config", str(context_path)], "lifecycle": "persistent"},
            "subscriptions": [{"events": names, "mode": "observe", "content": {"default": "metadata"}}]}]}))
        evidence["canonical_hooks_file"] = str(config_path)
        evidence["test_home"] = str(isolated_home)
        q = shlex.quote
        command = (f"env HOME={q(str(isolated_home))} KIT_AHP_HERDR_SMOKE=1 KIT_AHP_HERDR_GATE_DIR={q(str(gates))} "
                   f"{q(str(executable))} --ignored --exact {case} --nocapture --test-threads=1 "
                   f"> {q(str(root / 'test.stdout'))} 2> {q(str(root / 'test.stderr'))}; test_rc=$?; "
                   f"{q(str(bridge))} --config {q(str(context_path))} --release; release_rc=$?; "
                   f"printf '%s %s\\n' \"$test_rc\" \"$release_rc\" > {q(str(root / 'test-exit'))}; sleep 45")
        cli("pane", "run", pane_id, "sh -c " + q(command))

        def assert_state(stage, expected):
            wait(lambda: (gates / stage).exists(), "Missing genuine model/driver boundary " + stage)
            def accepted():
                current = pane()
                return current if current.get("agent") == "kit-smoke" and current.get("agent_status") in ({"idle", "done"} if expected == "idle" else {expected}) else None
            current = wait(accepted, "Native event did not report " + expected)
            evidence["checks"].append({"boundary": stage, "agent": current["agent"], "state": current["agent_status"]})

        assert_state("session-initialized", "idle")
        (gates / "allow-turn").touch()
        assert_state("turn-working", "working")
        (gates / "allow-compose").touch()
        wait(lambda: (gates / "child-verified").exists(), "Real compose/edit result missing")
        evidence["child"] = json.loads((gates / "child-verified").read_text())
        effect = gates / "work/actual-compose-edit.txt"
        assert effect.read_text() == "real compose child\n", "Actual native edit effect missing"
        assert evidence["child"]["transcript_result_verified"] is True
        assert_state("turn-finished", "idle")
        evidence["final"] = json.loads((gates / "turn-finished").read_text())
        assert evidence["final"]["final_output"] == "Kit native AHP HerdR smoke complete"
        # Do not let SDK shutdown kill a pending last-state report: idle was
        # independently observed before close/shutdown is admitted.
        (gates / "allow-shutdown").touch()
        wait(lambda: (root / "test-exit").exists(), "Native test close/exit missing")
        assert (root / "test-exit").read_text().strip() == "0 0", "Test or explicit release failed"
        assert (gates / "closed").exists(), "Native driver/SDK did not close"
        released = pane()
        assert not released.get("agent"), "Explicit exit release did not clear source ownership"
        info = cli("pane", "process-info", "--pane", pane_id)["result"]["process_info"]
        assert any(process["name"] == "sleep" for process in info.get("foreground_processes", [])), "Idle-shell fallback could mask release"
        evidence["release_foreground_processes"] = [process["name"] for process in info["foreground_processes"]]
        evidence["released_state"] = released["agent_status"]
        evidence["passed"] = True
    except Exception as error:
        evidence["failure"] = str(error)
        raise
    finally:
        cleanup_failed = False
        if started:
            for operation in ["stop", "delete"]:
                try:
                    result = subprocess.run(prefix + ["session", operation, name, "--json"], env=env, capture_output=True, text=True, timeout=15)
                    evidence["cleanup_" + operation] = {"exit": result.returncode, "stdout": result.stdout.strip(), "stderr": result.stderr.strip()}
                    cleanup_failed = cleanup_failed or result.returncode != 0
                except Exception as error:
                    evidence["cleanup_" + operation] = {"exit": None, "error": str(error)}
                    cleanup_failed = True
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                evidence["server_reap"] = "Owned named server did not exit; no process was killed directly"
                cleanup_failed = True
        if cleanup_failed:
            evidence["passed"] = False
        (root / "evidence.json").write_text(json.dumps(evidence, indent=2))
        print(json.dumps({"session": name, "evidence": str(root / "evidence.json"), "passed": evidence.get("passed", False),
            "checks": evidence["checks"], "cleanup": {k: v["exit"] for k, v in evidence.items() if k.startswith("cleanup_")}}, indent=2))
        if cleanup_failed:
            raise RuntimeError("Named test-resource cleanup incomplete; inspect evidence")


if __name__ == "__main__":
    main()
