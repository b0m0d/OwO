#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""长程多轮任务 E2E：真实模型 + 自动审批应答 + 工具调用闭环。

验证目标（用户可见行为，而非接口 200）：
* 同一会话连续两轮自然语言任务都能走到 final；
* 中间真实触发工具调用与审批，且审批后回合继续；
* 工作区留下第一轮声明的文件，第二轮能读改同一文件；
* 没有 turn_failed / 审批超时。

用法：python long-task-e2e.py --base http://127.0.0.1:4097 --workspace <path>
退出码：0 = 全部断言通过；非 0 = 有断言失败。
"""
import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")


class ApiError(RuntimeError):
    def __init__(self, status, body):
        super().__init__("HTTP %s %s" % (status, body[:500]))
        self.status = status
        self.body = body


def request_json(base, path, body=None, method=None, timeout=60, token=None):
    data = None if body is None else json.dumps(body, ensure_ascii=False).encode("utf-8")
    request = urllib.request.Request(base + path, data=data, method=method or ("POST" if data is not None else "GET"))
    if data is not None:
        request.add_header("Content-Type", "application/json")
    if token:
        request.add_header("Authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as error:
        raise ApiError(error.code, error.read().decode("utf-8", errors="replace")) from error


def acquire_token(api):
    payload = request_json(api, "/auth/token", timeout=30)
    token = payload.get("token")
    if not token:
        raise RuntimeError("GET /auth/token 没有返回 token：%s" % payload)
    return token


def approve(api, session_id, request_id, token):
    return request_json(
        api,
        "/session/%s/permission/%s" % (session_id, request_id),
        {"allow": True},
        token=token,
    )


class TurnResult(object):
    def __init__(self):
        self.final = None
        self.tool_uses = []
        self.tool_results = []
        self.permission_requests = []
        self.user_questions = []
        self.turn_failed = []
        self.events = 0
        self.token_deltas = 0
        self.reasoning_deltas = 0
        self.elapsed = 0.0


def run_turn(api, session_id, prompt, timeout=900, token=None):
    result = TurnResult()
    request = urllib.request.Request(
        api + "/session/" + session_id + "/turn",
        data=json.dumps({"prompt": prompt}, ensure_ascii=False).encode("utf-8"),
        method="POST",
    )
    request.add_header("Content-Type", "application/json")
    if token:
        request.add_header("Authorization", "Bearer " + token)
    started = time.time()
    with urllib.request.urlopen(request, timeout=timeout) as response:
        for raw in response:
            line = raw.decode("utf-8", errors="replace").strip()
            if not line.startswith("data:"):
                continue
            payload = line[5:].strip()
            try:
                event = json.loads(payload)
            except json.JSONDecodeError:
                continue
            result.events += 1
            etype = event.get("type")
            if etype == "token_delta":
                result.token_deltas += 1
                if result.token_deltas % 50 == 0:
                    print("  [stream] %d deltas, %ds" % (result.token_deltas, int(time.time() - started)), flush=True)
                continue
            if etype == "reasoning_delta":
                result.reasoning_deltas += 1
                if result.reasoning_deltas % 100 == 0:
                    print("  [thinking] %d reasoning deltas, %ds" % (result.reasoning_deltas, int(time.time() - started)), flush=True)
                continue
            if etype == "permission_request":
                request_id = event.get("request_id")
                tool = event.get("tool")
                level = event.get("level")
                print("  [approval] %s level=%s tool=%s" % (request_id, level, tool), flush=True)
                result.permission_requests.append(event)
                try:
                    decision = approve(api, session_id, request_id, token)
                    print("  [approval] resolved=%s" % (decision.get("request_id", request_id),), flush=True)
                except ApiError as error:
                    print("  [approval] FAILED %s" % (error,), flush=True)
                    raise
            elif etype == "user_question":
                result.user_questions.append(event)
                question_id = event.get("question_id")
                print("  [question] %s" % event.get("question"), flush=True)
                try:
                    request_json(
                        api,
                        "/session/%s/answer/%s" % (session_id, question_id),
                        {
                            "question_id": question_id,
                            "answer": "请按原任务要求继续，不要提问；缺少的信息按最合理假设处理并在总结中说明。",
                        },
                        token=token,
                    )
                    print("  [question] answered", flush=True)
                except ApiError as error:
                    print("  [question] answer FAILED %s" % (error,), flush=True)
                continue
            elif etype == "tool_use":
                result.tool_uses.append(event)
                print("  [tool] use %s" % event.get("tool"), flush=True)
            elif etype == "tool_result":
                result.tool_results.append(event)
                status = "ok" if event.get("ok") else ("error: %s" % event.get("error"))
                print("  [tool] result %s -> %s" % (event.get("tool"), status), flush=True)
            elif etype == "final":
                result.final = event.get("text")
                print("  [final] %r" % (result.final[:120] if result.final else "",), flush=True)
            elif etype == "turn_failed":
                result.turn_failed.append(event)
                print("  [turn_failed] %s" % (event,), flush=True)
    result.elapsed = round(time.time() - started, 1)
    return result


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default="http://127.0.0.1:4097")
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--turn-timeout", type=int, default=900)
    args = parser.parse_args()
    api = args.base.rstrip("/")
    workspace = os.path.abspath(args.workspace)

    health = request_json(api, "/health")
    print("health: %s" % health, flush=True)
    require(health.get("healthy") is True, "core 不健康：%s" % health)
    token = acquire_token(api)
    print("auth: token acquired", flush=True)

    session = request_json(api, "/session", {"workspace": workspace, "model": None}, token=token)
    session_id = session["id"]
    print("session: %s" % session_id, flush=True)

    turn1_prompt = (
        "请完成一个多步骤文件任务，不要向我提问，全程用工具执行。\n"
        "1) 先调用 verification_plan 登记验收：用 workspace-file-exists-v1 检查 long-task/plan.md 存在，"
        "用 workspace-file-exists-v1 检查 long-task/summary.txt 存在，"
        "用 workspace-file-contains-v1 检查 long-task/data.txt 包含 gamma；"
        "covers_requirement_ids 用本消息原文中的精确片段。\n"
        "2) 创建 long-task/plan.md，写标题和三条任务清单。\n"
        "3) 创建 long-task/data.txt，写入三行：alpha、beta、gamma。\n"
        "4) 读取 long-task/data.txt，确认正好三行。\n"
        "5) 创建 long-task/summary.txt，中文写明创建了哪些文件以及验证方式。\n"
        "6) 最后用中文总结，不要运行命令验证。"
    )
    print("== turn 1 ==", flush=True)
    turn1 = run_turn(api, session_id, turn1_prompt, timeout=args.turn_timeout, token=token)
    print(
        "turn1 elapsed=%ss final=%s tools=%s approvals=%s"
        % (
            turn1.elapsed,
            bool(turn1.final),
            [item.get("tool") for item in turn1.tool_uses],
            len(turn1.permission_requests),
        ),
        flush=True,
    )
    require(not turn1.turn_failed, "第一轮出现 turn_failed：%s" % turn1.turn_failed)
    require(turn1.final, "第一轮没有 final 文本")
    require(len(turn1.tool_uses) >= 3, "第一轮工具调用不足 3 次：%s" % [item.get("tool") for item in turn1.tool_uses])
    require(
        any(item.get("tool") == "verification_plan" for item in turn1.tool_uses),
        "第一轮没有登记宿主验收计划：%s" % [item.get("tool") for item in turn1.tool_uses],
    )
    require(len(turn1.permission_requests) >= 2, "第一轮审批请求不足 2 次（写入至少两次）")

    plan_path = os.path.join(workspace, "long-task", "plan.md")
    data_path = os.path.join(workspace, "long-task", "data.txt")
    summary_path = os.path.join(workspace, "long-task", "summary.txt")
    require(os.path.isfile(plan_path), "第一轮未创建 plan.md")
    require(os.path.isfile(data_path), "第一轮未创建 data.txt")
    require(os.path.isfile(summary_path), "第一轮未创建 summary.txt")
    data_text = open(data_path, "r", encoding="utf-8", errors="replace").read()
    for needle in ("alpha", "beta", "gamma"):
        require(needle in data_text, "data.txt 缺少 %s：%r" % (needle, data_text))

    turn2_prompt = (
        "接着上一轮的工作继续，不要向我提问。\n"
        "1) 用写入/编辑工具把 long-task/data.txt 追加一行 delta（保留原有三行）。\n"
        "2) 读回 long-task/data.txt，确认现在一共四行。\n"
        "3) 更新 long-task/summary.txt，写上当前总行数与新增内容。\n"
        "4) 最后用中文报告：data.txt 现在的内容与总行数。不要运行命令。"
    )
    print("== turn 2 ==", flush=True)
    turn2 = run_turn(api, session_id, turn2_prompt, timeout=args.turn_timeout, token=token)
    print(
        "turn2 elapsed=%ss final=%s tools=%s approvals=%s"
        % (
            turn2.elapsed,
            bool(turn2.final),
            [item.get("tool") for item in turn2.tool_uses],
            len(turn2.permission_requests),
        ),
        flush=True,
    )
    require(not turn2.turn_failed, "第二轮出现 turn_failed：%s" % turn2.turn_failed)
    require(turn2.final, "第二轮没有 final 文本")
    require(len(turn2.tool_uses) >= 2, "第二轮工具调用不足 2 次")
    data_text = open(data_path, "r", encoding="utf-8", errors="replace").read()
    require("delta" in data_text, "第二轮没有追加 delta：%r" % data_text)
    require(len(data_text.strip().splitlines()) >= 4, "data.txt 不足四行：%r" % data_text)

    detail = request_json(api, "/session/" + session_id, timeout=30, token=token)
    messages = detail.get("messages") or detail.get("session", {}).get("messages") or []
    require(len(messages) >= 4, "会话历史没有持久化两轮问答：%s" % len(messages))

    print(
        "E2E_PASS turns=2 tools=%d approvals=%d files=%s"
        % (
            len(turn1.tool_uses) + len(turn2.tool_uses),
            len(turn1.permission_requests) + len(turn2.permission_requests),
            sorted(os.listdir(os.path.join(workspace, "long-task"))),
        ),
        flush=True,
    )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except AssertionError as failure:
        print("E2E_FAIL %s" % failure, flush=True)
        sys.exit(2)
    except Exception as error:  # noqa: BLE001 - E2E 需要明确失败原因
        import traceback

        traceback.print_exc()
        print("E2E_ERROR %r" % (error,), flush=True)
        sys.exit(3)
