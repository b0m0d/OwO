#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""采集指南 §6.5 的固定性能任务（真实 Daemon + 真实 HTTP/SSE）。

该脚本只负责采集证据，不替代 trace-perf.ps1 的统计门。每轮都会把
Daemon stdout/stderr、启动时间、会话创建时间、SSE 事件和回合结果写入
指定 evidence 目录；凭据只从当前进程环境继承，绝不写入结果。
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Tuple


def json_request(
    base: str,
    path: str,
    payload: Optional[Dict[str, Any]],
    token: str,
    timeout: float,
) -> Dict[str, Any]:
    body = None if payload is None else json.dumps(payload, ensure_ascii=False).encode("utf-8")
    request = urllib.request.Request(base + path, data=body, method="POST" if body is not None else "GET")
    request.add_header("Accept", "application/json")
    if body is not None:
        request.add_header("Content-Type", "application/json")
    if token:
        request.add_header("Authorization", "Bearer " + token)
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.loads(response.read().decode("utf-8"))


def permission_allow(base: str, session_id: str, request_id: str, token: str) -> None:
    json_request(base, f"/session/{session_id}/permission/{request_id}", {"allow": True}, token, 30)


def consume_sse(
    base: str,
    session_id: str,
    prompt: str,
    token: str,
    timeout: float,
) -> Tuple[List[Dict[str, Any]], Optional[str]]:
    request = urllib.request.Request(
        base + f"/session/{session_id}/turn",
        data=json.dumps({"prompt": prompt}, ensure_ascii=False).encode("utf-8"),
        method="POST",
    )
    request.add_header("Accept", "text/event-stream")
    request.add_header("Content-Type", "application/json")
    if token:
        request.add_header("Authorization", "Bearer " + token)
    events: List[Dict[str, Any]] = []
    event_name = ""
    data_lines: List[str] = []
    final_text: Optional[str] = None
    with urllib.request.urlopen(request, timeout=timeout) as response:
        for raw in response:
            line = raw.decode("utf-8", errors="replace").rstrip("\r\n")
            if line.startswith("event:"):
                event_name = line[6:].strip()
            elif line.startswith("data:"):
                data_lines.append(line[5:].strip())
            elif not line and data_lines:
                try:
                    payload = json.loads("".join(data_lines))
                except json.JSONDecodeError:
                    event_name = ""
                    data_lines = []
                    continue
                if isinstance(payload, dict):
                    if event_name and "type" not in payload:
                        payload["type"] = event_name
                    events.append(payload)
                    kind = str(payload.get("type", event_name))
                    if kind in {"permission_request", "permission"}:
                        request_id = str(payload.get("request_id", ""))
                        if request_id:
                            permission_allow(base, session_id, request_id, token)
                    elif kind == "final":
                        final_text = payload.get("text")
                event_name = ""
                data_lines = []
    return events, final_text


def wait_core_ready(stdout: Path, process: subprocess.Popen[Any], timeout: float) -> Dict[str, Any]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if stdout.exists():
            for line in stdout.read_text(encoding="utf-8", errors="replace").splitlines():
                if '"event"' in line and '"core_ready"' in line:
                    return json.loads(line)
        if process.poll() is not None:
            break
        time.sleep(0.2)
    raise RuntimeError(f"core_ready 未在 {timeout:.0f}s 内出现（exit={process.poll()}）")


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


def project_root(exe: Path) -> Path:
    for candidate in (exe.parent, *exe.parents):
        if (candidate / "scripts").is_dir():
            return candidate
    return exe.parent


def start_daemon(
    exe: Path,
    run_dir: Path,
    task: str,
    workspace: Path,
    invalid_mcp: bool,
) -> Tuple[subprocess.Popen[Any], str, str, Dict[str, Any]]:
    data = run_dir / "data"
    data.mkdir(parents=True, exist_ok=True)
    workspace.mkdir(parents=True, exist_ok=True)
    if invalid_mcp:
        (data / "mcp-servers.json").write_text(
            json.dumps(
                [
                    {
                        "name": "invalid-perf-mcp",
                        "transport": "stdio",
                        "command": "owo-agent-perf-missing-mcp-command",
                        "args": [],
                    }
                ],
                ensure_ascii=False,
            ),
            encoding="utf-8",
        )
    stdout = run_dir / "serve.stdout.log"
    stderr = run_dir / "serve.stderr.log"
    env = os.environ.copy()
    env["OWO_AGENT_DATA"] = str(data)
    env["OWO_PERF_TASK_ID"] = task
    env.pop("OWO_CLOUD_ENABLED", None)
    if os.name == "nt":
        creationflags = subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.CREATE_NO_WINDOW
    else:
        creationflags = 0
    started = time.monotonic()
    out_handle = stdout.open("w", encoding="utf-8")
    err_handle = stderr.open("w", encoding="utf-8")
    process = subprocess.Popen(
        [str(exe), "serve", "--port", "0", "--output", "jsonl", "--workspace", str(workspace)],
        cwd=str(project_root(exe)),
        env=env,
        stdout=out_handle,
        stderr=err_handle,
        creationflags=creationflags,
    )
    try:
        ready = wait_core_ready(stdout, process, 90)
    finally:
        out_handle.close()
        err_handle.close()
    ready["startup_ms"] = round((time.monotonic() - started) * 1000, 2)
    token_path = data / "auth" / "token"
    deadline = time.monotonic() + 10
    while not token_path.exists() and time.monotonic() < deadline:
        time.sleep(0.1)
    if not token_path.exists():
        stop_process(process)
        raise RuntimeError("Daemon 已 core_ready 但未生成 auth/token")
    token = token_path.read_text(encoding="utf-8").strip()
    return process, f"http://127.0.0.1:{ready['port']}", token, ready


def one_run(
    exe: Path,
    evidence: Path,
    task: str,
    prompt: str,
    index: int,
    restart_each: bool,
    invalid_mcp: bool,
    verify_diff: bool,
    shared: Optional[Tuple[subprocess.Popen[Any], str, str, Dict[str, Any]]],
) -> Tuple[Dict[str, Any], Optional[Tuple[subprocess.Popen[Any], str, str, Dict[str, Any]]]]:
    run_dir = evidence / "runs" / f"run-{index:02d}"
    workspace = run_dir / "workspace"
    run_dir.mkdir(parents=True, exist_ok=True)
    active = shared
    if active is None:
        active = start_daemon(exe, run_dir, task, workspace, invalid_mcp)
    process, base, token, ready = active
    session_started = time.monotonic()
    session = json_request(base, "/session", {"workspace": str(workspace)}, token, 30)
    session_ms = round((time.monotonic() - session_started) * 1000, 2)
    turn_started = time.monotonic()
    events, final_text = consume_sse(base, str(session["id"]), prompt, token, 300)
    diff = None
    if verify_diff:
        diff = json_request(base, f"/session/{session['id']}/diff", None, token, 30)
    result = {
        "run": index,
        "task": task,
        "core_ready": ready,
        "session_create_ms": session_ms,
        "turn_duration_ms": round((time.monotonic() - turn_started) * 1000, 2),
        "session_id": session["id"],
        "events": events,
        "tool_names": sorted({str(event.get("tool")) for event in events if event.get("tool")}),
        "final_received": final_text is not None,
        "final_text_length": len(final_text or ""),
        "diff": diff,
    }
    (run_dir / "result.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    if restart_each:
        stop_process(process)
        active = None
    return result, active


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--exe", required=True)
    parser.add_argument("--evidence", required=True)
    parser.add_argument("--task", required=True)
    parser.add_argument("--prompt", required=True)
    parser.add_argument("--runs", type=int, default=20)
    parser.add_argument("--restart-each", action="store_true")
    parser.add_argument("--invalid-mcp", action="store_true")
    parser.add_argument("--verify-diff", action="store_true")
    args = parser.parse_args()
    if args.runs < 1:
        raise SystemExit("--runs 必须大于 0")

    exe = Path(args.exe).resolve()
    evidence = Path(args.evidence).resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    results: List[Dict[str, Any]] = []
    shared: Optional[Tuple[subprocess.Popen[Any], str, str, Dict[str, Any]]] = None
    try:
        for index in range(1, args.runs + 1):
            result, shared = one_run(
                exe,
                evidence,
                args.task,
                args.prompt,
                index,
                args.restart_each,
                args.invalid_mcp,
                args.verify_diff,
                shared,
            )
            results.append(result)
            print(
                f"run={index}/{args.runs} task={args.task} turn_ms={result['turn_duration_ms']} "
                f"tools={','.join(result['tool_names']) or '-'}",
                flush=True,
            )
    finally:
        if shared is not None:
            stop_process(shared[0])
    summary = {
        "task": args.task,
        "runs": len(results),
        "success": sum(1 for result in results if result["final_received"]),
        "restart_each": bool(args.restart_each),
        "invalid_mcp": bool(args.invalid_mcp),
        "results": results,
    }
    (evidence / "collector-summary.json").write_text(
        json.dumps(summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    return 0 if len(results) == args.runs and summary["success"] == args.runs else 2


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (KeyboardInterrupt, urllib.error.URLError, RuntimeError) as error:
        print(f"collector failed: {error}", file=sys.stderr)
        raise SystemExit(2)
