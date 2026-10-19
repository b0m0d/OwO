#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""模型通道冒烟：建会话 → 最小 turn（“用中文回复：你好”）→ 打印耗时与事件数。"""
import json
import argparse
import os
import sys
import time
import urllib.request

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")


def fetch_token(base):
    """R7 bearer 引导：本地服务除公开端点外一律要 Authorization。"""
    request = urllib.request.Request(base + "/auth/token")
    token = json.loads(urllib.request.urlopen(request, timeout=30).read().decode("utf-8")).get("token")
    if not token:
        raise RuntimeError("GET /auth/token 未返回 token")
    return token


def post(base, path, body=None, timeout=60, token=None):
    data = b"{}" if body is None else json.dumps(body, ensure_ascii=False).encode("utf-8")
    request = urllib.request.Request(base + path, data=data, method="POST")
    request.add_header("Content-Type", "application/json")
    if token:
        request.add_header("Authorization", "Bearer " + token)
    return json.loads(urllib.request.urlopen(request, timeout=timeout).read().decode("utf-8"))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default="http://127.0.0.1:4097")
    parser.add_argument("--prompt", default="用中文回复：你好")
    args = parser.parse_args()
    token = fetch_token(args.base)
    session = post(args.base, "/session", {"workspace": os.getcwd(), "model": None}, token=token)
    print("session:", session.get("id"), flush=True)
    request = urllib.request.Request(
        args.base + "/session/" + session["id"] + "/turn",
        data=json.dumps({"prompt": args.prompt}).encode("utf-8"),
        method="POST",
    )
    request.add_header("Content-Type", "application/json")
    request.add_header("Authorization", "Bearer " + token)
    started = time.time()
    events = 0
    final = None
    with urllib.request.urlopen(request, timeout=240) as response:
        for raw in response:
            line = raw.decode("utf-8", errors="replace").strip()
            if not line.startswith("data:"):
                continue
            events += 1
            try:
                event = json.loads(line[5:].strip())
            except json.JSONDecodeError:
                continue
            if event.get("type") == "final":
                final = event.get("text")
    print("elapsed:", round(time.time() - started, 1), "s events:", events, flush=True)
    print("final:", final, flush=True)
    return 0 if final else 1


if __name__ == "__main__":
    sys.exit(main())
