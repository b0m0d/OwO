#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""采集真实 SSE 断开、durable events 重连和 turn abort 证据。"""

from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import threading
import time
import urllib.request
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple


def request_json(
    base: str,
    path: str,
    token: str,
    payload: Optional[Dict[str, Any]] = None,
    timeout: float = 30,
) -> Tuple[int, Dict[str, Any]]:
    body = None if payload is None else json.dumps(payload, ensure_ascii=False).encode("utf-8")
    request = urllib.request.Request(
        base + path,
        data=body,
        method="POST" if body is not None else "GET",
    )
    request.add_header("Accept", "application/json")
    if body is not None:
        request.add_header("Content-Type", "application/json")
    request.add_header("Authorization", "Bearer " + token)
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return response.status, json.loads(response.read().decode("utf-8"))


def project_root(exe: Path) -> Path:
    for candidate in (exe.parent, *exe.parents):
        if (candidate / "scripts").is_dir():
            return candidate
    return exe.parent


def stop_process(process: Optional[subprocess.Popen[Any]]) -> None:
    if process is None or process.poll() is not None:
        return
    if os.name == "nt":
        process.send_signal(signal.CTRL_BREAK_EVENT)
    else:
        process.terminate()
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=10)


def start_daemon(exe: Path, run_dir: Path, task: str) -> Tuple[subprocess.Popen[Any], str, str, Dict[str, Any]]:
    data = run_dir / "data"
    workspace = run_dir / "workspace"
    data.mkdir(parents=True, exist_ok=True)
    workspace.mkdir(parents=True, exist_ok=True)
    stdout = run_dir / "serve.stdout.log"
    stderr = run_dir / "serve.stderr.log"
    env = os.environ.copy()
    env["OWO_AGENT_DATA"] = str(data)
    env["OWO_PERF_TASK_ID"] = task
    env.pop("OWO_CLOUD_ENABLED", None)
    flags = subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0
    out_handle = stdout.open("w", encoding="utf-8")
    err_handle = stderr.open("w", encoding="utf-8")
    process = subprocess.Popen(
        [str(exe), "serve", "--port", "0", "--output", "jsonl", "--workspace", str(workspace)],
        cwd=str(project_root(exe)),
        env=env,
        stdout=out_handle,
        stderr=err_handle,
        creationflags=flags,
    )
    try:
        deadline = time.monotonic() + 90
        ready: Optional[Dict[str, Any]] = None
        while time.monotonic() < deadline:
            if stdout.exists():
                for line in stdout.read_text(encoding="utf-8", errors="replace").splitlines():
                    if '"event"' in line and '"core_ready"' in line:
                        ready = json.loads(line)
                        break
            if ready is not None:
                break
            if process.poll() is not None:
                break
            time.sleep(0.2)
        if ready is None:
            raise RuntimeError(f"core_ready 未出现（exit={process.poll()}）")
    finally:
        out_handle.close()
        err_handle.close()
    ready["startup_ms"] = round((time.monotonic() - (deadline - 90)) * 1000, 2)
    token_path = data / "auth" / "token"
    token_deadline = time.monotonic() + 10
    while not token_path.exists() and time.monotonic() < token_deadline:
        time.sleep(0.1)
    if not token_path.exists():
        stop_process(process)
        raise RuntimeError("Daemon 已 core_ready 但未生成 auth/token")
    return process, f"http://127.0.0.1:{ready['port']}", token_path.read_text(encoding="utf-8").strip(), ready


def read_one_sse_event(response: Any, timeout_event: threading.Event) -> Dict[str, Any]:
    event_name = ""
    data_lines: List[str] = []
    for raw in response:
        line = raw.decode("utf-8", errors="replace").rstrip("\r\n")
        if line.startswith("event:"):
            event_name = line[6:].strip()
        elif line.startswith("data:"):
            data_lines.append(line[5:].strip())
        elif not line and data_lines:
            payload = json.loads("".join(data_lines))
            if isinstance(payload, dict) and event_name and "type" not in payload:
                payload["type"] = event_name
            timeout_event.set()
            return payload
    raise RuntimeError("断开前未收到任何 SSE 事件")


def run_one(exe: Path, evidence: Path, index: int, prompt: str) -> Dict[str, Any]:
    run_dir = evidence / "runs" / f"run-{index:02d}"
    run_dir.mkdir(parents=True, exist_ok=True)
    process, base, token, ready = start_daemon(exe, run_dir, "disconnect_reconnect_cancel")
    try:
        session_started = time.monotonic()
        session_status, session = request_json(base, "/session", token, {"workspace": str(run_dir / "workspace")})
        session_ms = round((time.monotonic() - session_started) * 1000, 2)
        session_id = str(session["id"])
        first_event = threading.Event()
        turn_info: Dict[str, Any] = {"status": None, "turn_id": None, "event": None, "error": None}

        def disconnect_reader() -> None:
            request = urllib.request.Request(
                base + f"/session/{session_id}/turn",
                data=json.dumps({"prompt": prompt}, ensure_ascii=False).encode("utf-8"),
                method="POST",
            )
            request.add_header("Accept", "text/event-stream")
            request.add_header("Content-Type", "application/json")
            request.add_header("Authorization", "Bearer " + token)
            try:
                with urllib.request.urlopen(request, timeout=300) as response:
                    turn_info["status"] = response.status
                    turn_info["turn_id"] = response.headers.get("x-owo-turn-id")
                    turn_info["event"] = read_one_sse_event(response, first_event)
                    # 退出 with 块即模拟客户端断开；服务端应记录 disconnect 并停止向无主流推送。
            except Exception as error:  # 断开后的 HTTP 读错误是预期的，保留为证据。
                turn_info["error"] = str(error)

        reader = threading.Thread(target=disconnect_reader, daemon=True)
        turn_started = time.monotonic()
        reader.start()
        if not first_event.wait(timeout=60):
            raise RuntimeError("60s 内未收到首个回合事件")
        disconnect_at = time.monotonic()
        reader.join(timeout=5)
        if not turn_info["turn_id"]:
            raise RuntimeError("turn 响应缺少 x-owo-turn-id")

        abort_started = time.monotonic()
        abort_status, abort_body = request_json(base, f"/session/{session_id}/abort", token, {}, 30)
        after_seq = 0
        replay: List[Dict[str, Any]] = []
        state: Optional[str] = None
        active = True
        reconnect_status = 0
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            reconnect_status, durable = request_json(
                base,
                f"/session/{session_id}/turn/events?turn_id={turn_info['turn_id']}&after_seq={after_seq}&limit=256",
                token,
                None,
                30,
            )
            replay.extend(durable.get("events", []))
            after_seq = int(durable.get("next_after_seq", after_seq))
            state = durable.get("state")
            active = bool(durable.get("active"))
            if not active:
                break
            time.sleep(0.25)
        stop_ms = round((time.monotonic() - abort_started) * 1000, 2)
        result = {
            "run": index,
            "task": "disconnect_reconnect_cancel",
            "core_ready": ready,
            "session_status": session_status,
            "session_create_ms": session_ms,
            "turn_duration_ms": round((time.monotonic() - turn_started) * 1000, 2),
            "disconnect_after_ms": round((disconnect_at - turn_started) * 1000, 2),
            "session_id": session_id,
            "turn_id": turn_info["turn_id"],
            "first_event": turn_info["event"],
            "reader_error": turn_info["error"],
            "abort_status": abort_status,
            "abort_body": abort_body,
            "reconnect_status": reconnect_status,
            "replayed_event_count": len(replay),
            "reconnect_state": state,
            "reconnect_active": active,
            "cancel_to_stop_ms": stop_ms if not active else None,
            "success": abort_status == 200 and reconnect_status == 200 and not active,
            "replayed_events": replay,
        }
        (run_dir / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        return result
    finally:
        stop_process(process)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--exe", required=True)
    parser.add_argument("--evidence", required=True)
    parser.add_argument("--prompt", required=True)
    parser.add_argument("--runs", type=int, default=20)
    args = parser.parse_args()
    exe = Path(args.exe).resolve()
    evidence = Path(args.evidence).resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    results: List[Dict[str, Any]] = []
    try:
        for index in range(1, args.runs + 1):
            result = run_one(exe, evidence, index, args.prompt)
            results.append(result)
            print(
                f"run={index}/{args.runs} state={result['reconnect_state']} "
                f"cancel_to_stop_ms={result['cancel_to_stop_ms']} replayed={result['replayed_event_count']}",
                flush=True,
            )
    finally:
        summary = {
            "task": "disconnect_reconnect_cancel",
            "runs": len(results),
            "success": sum(1 for result in results if result.get("success")),
            "results": results,
        }
        (evidence / "collector-summary.json").write_text(
            json.dumps(summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
    return 0 if len(results) == args.runs and all(result.get("success") for result in results) else 2


if __name__ == "__main__":
    raise SystemExit(main())
