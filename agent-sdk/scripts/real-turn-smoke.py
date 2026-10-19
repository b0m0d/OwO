#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""P0 真实模型 turn 冒烟：验证逐 delta 实时输出（不是整轮缓存后回放）。

指南 §8 P0 第 5 条：使用用户环境中已配置的 Provider，验证收到**多个实时 delta**；
密钥只从环境变量/参数读取，绝不打印。

输入：
  --base      http://127.0.0.1:<port>（真实 Daemon）
  --workspace 会话工作区路径
  --prompt    提示词（默认要求一段较长回答，确保产生多个 delta）
  --timeout   整轮墙钟上限（秒）
  --out       结果 JSON 输出路径（供 mk-smoke.ps1 读取）
  鉴权 token 经环境变量 OWO_SMOKE_TOKEN（命令行不回显，避免进入进程列表）。

输出：一行 JSON 摘要到 stdout，同时写 --out；退出码 0=通过，1=失败。
"""

import argparse
import http.client
import json
import os
import sys
import time
import urllib.parse

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")

DEFAULT_PROMPT = "请用大约 60 个字介绍你自己，并说明你能做什么。"


def parse_base(base):
    url = urllib.parse.urlparse(base)
    host = url.hostname or "127.0.0.1"
    port = url.port or 80
    return host, port


def request_json(conn, method, path, body, token, timeout):
    headers = {"Content-Type": "application/json", "Accept": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    payload = json.dumps(body, ensure_ascii=False).encode("utf-8") if body is not None else b"{}"
    conn.request(method, path, body=payload, headers=headers)
    resp = conn.getresponse()
    raw = resp.read().decode("utf-8", errors="replace")
    if resp.status not in (200, 201, 202):
        raise RuntimeError(f"{method} {path} -> {resp.status}: {raw[:300]}")
    return json.loads(raw) if raw.strip() else {}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default=os.environ.get("OWO_SMOKE_BASE", "http://127.0.0.1:4099"))
    parser.add_argument("--workspace", default=os.environ.get("OWO_SMOKE_WORKSPACE", os.getcwd()))
    parser.add_argument("--prompt", default=DEFAULT_PROMPT)
    parser.add_argument("--timeout", type=float, default=240)
    parser.add_argument("--min-deltas", type=int, default=2)
    parser.add_argument("--out", default=os.environ.get("OWO_SMOKE_OUT", ""))
    args = parser.parse_args()

    token = os.environ.get("OWO_SMOKE_TOKEN", "")
    host, port = parse_base(args.base)
    started = time.time()
    result = {
        "status": "failed",
        "ok": False,
        "delta_count": 0,
        "first_delta_latency_s": None,
        "stream_span_s": None,
        "final_latency_s": None,
        "buffered_suspected": None,
        "text_len": 0,
        "final_received": False,
        "first_delta_before_final": False,
        "model": None,
        "error": None,
    }

    try:
        conn = http.client.HTTPConnection(host, port, timeout=args.timeout)
        session = request_json(conn, "POST", "/session", {"workspace": args.workspace}, token, args.timeout)
        session_id = session.get("id")
        if not session_id:
            raise RuntimeError(f"创建会话未返回 id：{session}")

        headers = {"Content-Type": "application/json", "Accept": "text/event-stream"}
        if token:
            headers["Authorization"] = "Bearer " + token
        body = json.dumps({"prompt": args.prompt}, ensure_ascii=False).encode("utf-8")
        conn.request("POST", f"/session/{session_id}/turn", body=body, headers=headers)
        resp = conn.getresponse()
        if resp.status != 200:
            raw = resp.read().decode("utf-8", errors="replace")
            raise RuntimeError(f"turn -> {resp.status}: {raw[:300]}")

        first_delta_at = None
        final_at = None
        text_parts = []
        final_text = None
        for raw in resp:
            line = raw.decode("utf-8", errors="replace").strip()
            if not line.startswith("data:"):
                continue
            try:
                event = json.loads(line[5:].strip())
            except json.JSONDecodeError:
                continue
            etype = event.get("type")
            now = time.time()
            if etype == "token_delta":
                delta = event.get("delta") or ""
                result["delta_count"] += 1
                if first_delta_at is None:
                    first_delta_at = now
                text_parts.append(delta)
            elif etype == "final":
                final_at = now
                final_text = event.get("text") or ""
                result["final_received"] = True
                break
            elif etype == "progress":
                # 记录一次模型调用开始，不代表文本输出。
                pass

        result["text_len"] = len(final_text if final_text is not None else "".join(text_parts))
        if first_delta_at is not None:
            result["first_delta_latency_s"] = round(first_delta_at - started, 2)
        if first_delta_at is not None and final_at is not None:
            result["stream_span_s"] = round(final_at - first_delta_at, 2)
            result["final_latency_s"] = round(final_at - started, 2)
            result["first_delta_before_final"] = first_delta_at < final_at
            # 真流式的判据：首个 delta 必须明显早于 final（生成期有持续输出），而不是
            # 整轮结束后一次性回放。指南 F-02 的假流式正是"整轮缓存 Vec 成功后回放"，
            # 表现就是首个 delta 与 final 几乎同时到达（span≈0，first≈total）。
            total = result["final_latency_s"] or 0.0
            result["buffered_suspected"] = bool(
                result["first_delta_latency_s"] is not None
                and total > 0
                and (result["stream_span_s"] or 0.0) < 0.05
                and result["first_delta_latency_s"] >= total * 0.9
            )
        result["ok"] = (
            result["delta_count"] >= args.min_deltas
            and result["final_received"]
            and result["first_delta_before_final"]
        )
        result["status"] = "ok" if result["ok"] else "failed"
    except Exception as exc:  # noqa: BLE001 - 冒烟脚本需把任何异常变成结构化失败
        result["error"] = f"{type(exc).__name__}: {exc}"
        result["status"] = "failed"

    out = args.out
    payload = json.dumps(result, ensure_ascii=False)
    if out:
        with open(out, "w", encoding="utf-8") as handle:
            handle.write(payload)
    print(payload, flush=True)
    return 0 if result["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
