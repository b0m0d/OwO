import test from "node:test";
import assert from "node:assert/strict";
import { createClient, type TurnEvent } from "../src/index.js";
import type { components, operations } from "../src/schema.js";

test("runTurn 复用自定义请求头并处理无换行的最后一个 SSE 事件", async () => {
  const originalFetch = globalThis.fetch;
  let requestUrl = "";
  let requestHeaders: Headers | undefined;

  globalThis.fetch = async (input, init) => {
    requestUrl = String(input);
    requestHeaders = new Headers(init?.headers);
    return new Response('data: {"type":"final","text":"完成"}', {
      status: 200,
      headers: { "Content-Type": "text/event-stream" },
    });
  };

  try {
    const events: TurnEvent[] = [];
    const client = createClient({
      baseUrl: "http://127.0.0.1:4096/",
      headers: { Authorization: "Bearer test-token", "X-Client": "unit" },
    });

    await client.runTurn(
      { id: "session/1", prompt: "测试" },
      { onEvent: (event) => events.push(event) },
    );

    assert.equal(
      requestUrl,
      "http://127.0.0.1:4096/session/session%2F1/turn",
    );
    assert.equal(requestHeaders?.get("Authorization"), "Bearer test-token");
    assert.equal(requestHeaders?.get("X-Client"), "unit");
    assert.equal(requestHeaders?.get("Content-Type"), "application/json");
    assert.deepEqual(events, [{ type: "final", text: "完成" }]);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("runTurn 在开始前已取消时透传 AbortSignal", async () => {
  const originalFetch = globalThis.fetch;
  let signalAborted = false;
  const requestedUrls: string[] = [];

  globalThis.fetch = async (input, init) => {
    requestedUrls.push(String(input));
    signalAborted = init?.signal?.aborted ?? false;
    return new Response("", { status: 499 });
  };

  try {
    const controller = new AbortController();
    controller.abort();
    const client = createClient({ baseUrl: "http://127.0.0.1:4096" });

    await assert.rejects(
      client.runTurn(
        { id: "session", prompt: "取消" },
        { onEvent: () => undefined, signal: controller.signal },
      ),
      /HTTP 499/,
    );
    assert.equal(signalAborted, true);
    assert.ok(requestedUrls.some((url) => url.endsWith("/session/session/abort")));
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("runTurn 在 SSE 没有 final 事件时失败", async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () =>
    new Response('data: {"type":"progress","message":"处理中"}', {
      status: 200,
      headers: { "Content-Type": "text/event-stream" },
    });

  try {
    const client = createClient({ baseUrl: "http://127.0.0.1:4096" });
    await assert.rejects(
      client.runTurn(
        { id: "session", prompt: "截断" },
        { onEvent: () => undefined },
      ),
      /未收到 final 事件/,
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

// ---------------------------------------------------------------------------
// R0 契约同步（第一路）：候选治理字段 + retry 冻结契约的 TS 类型级断言。
// 本用例不发起网络请求——任何字段漂移都会先在 tsc 编译期失败。
// ---------------------------------------------------------------------------

test("ModelCandidate 治理字段契约（provider_ref / provider / calibration_summary / gates）", () => {
  type Candidate = components["schemas"]["ModelCandidate"];
  type ProviderRef = components["schemas"]["CandidateProviderRef"];
  type Calibration = components["schemas"]["CalibrationReport"];
  type RegisterBody = NonNullable<
    operations["modelCandidateRegister"]["requestBody"]
  >["content"]["application/json"];
  type PromoteResponse = operations["modelCandidatePromote"]["responses"][200]["content"]["application/json"];

  // 注册请求侧：provider_ref（声明 ≠ 接线；external 与 metadata_only 双形态）。
  const registerBody: RegisterBody = {
    model_id: "wm-rule",
    model_version: "1.0.0",
    provider_ref: { type: "external", kind: "wm1-http", locator: "http://127.0.0.1:9/v1" },
  };
  assert.equal(registerBody.provider_ref?.type, "external");
  const metadataOnly: NonNullable<RegisterBody["provider_ref"]> = { type: "metadata_only" };
  assert.equal(metadataOnly.type, "metadata_only");

  // 候选响应侧：wire 字段名是 provider（非 provider_ref）+ calibration_summary（可空）。
  const candidate: Candidate = {
    candidate_id: "c-1",
    model_id: "wm-rule",
    model_version: "1.0.0",
    source: "手动注册（shadow 起步）",
    status: "shadow",
    created_at: "2026-08-27T00:00:00Z",
    promoted_at: null,
    promote_reason: null,
    provider: { type: "metadata_only" },
    calibration_summary: null,
  };
  const wired: Extract<ProviderRef, { type: "external" }> = {
    type: "external",
    kind: "local-onnx",
    locator: "file:///models/wm.onnx",
  };
  candidate.provider = wired;
  const calibration: Calibration = {
    samples: 16,
    success_hit_rate: 0.9,
    mean_calibration_error: 0.1,
    mean_delta_jaccard: 0.2,
    uncertainty_buckets: [{ label: "0.0-0.25", samples: 4, hit_rate: 0.75 }],
  };
  candidate.status = "active";
  candidate.calibration_summary = calibration;

  // 晋升响应侧：gates 明细（min_shadow_samples / provider_wired / calibration_summary / regression_check）。
  const promoted: PromoteResponse = {
    candidate,
    active: "c-1",
    previous_active: null,
    samples: 16,
    gates: {
      min_shadow_samples: 16,
      provider_wired: { kind: "wm1-http", locator: "http://127.0.0.1:9/v1" },
      calibration_summary: calibration,
      regression_check: null,
    },
  };
  assert.equal(promoted.gates.min_shadow_samples, 16);
  assert.equal(promoted.gates.calibration_summary.samples, 16);
});

test("steer retry 冻结契约（command 枚举含 retry，step_id + note）", () => {
  type SteerBody = NonNullable<
    operations["workswarmSteerTeam"]["requestBody"]
  >["content"]["application/json"];
  const retryBody: SteerBody = {
    command: "retry",
    step_id: "builder",
    note: "修复输入后重试",
  };
  assert.equal(retryBody.command, "retry");
  assert.equal(retryBody.step_id, "builder");
  const continueBody: SteerBody = { command: "continue" };
  assert.equal(continueBody.command, "continue");
});

test("R2 additive：interrupted 布尔位出现在 teams 列表 / 详情 / steer / events 快照", () => {
  type SteerResponse =
    operations["workswarmSteerTeam"]["responses"][200]["content"]["application/json"];
  type TeamDetail =
    operations["workswarmGetTeam"]["responses"][200]["content"]["application/json"];
  type TeamList =
    operations["workswarmListTeams"]["responses"][200]["content"]["application/json"];
  type EventsSnapshot =
    operations["workswarmTeamEvents"]["responses"][200]["content"]["application/json"];

  const steer: SteerResponse = { team_id: "t", status: "Running", interrupted: false };
  assert.equal(typeof steer.interrupted, "boolean");

  const detail: TeamDetail = {
    team: {},
    interrupted: true,
    tasks: {},
    audit_tail: [{ ts: "2026-08-27T00:00:00Z", event: "team.interrupted", detail: "进程重启中断识别" }],
  };
  assert.equal(detail.interrupted, true);

  const list: TeamList = { teams: [{ active: false, interrupted: false }] };
  assert.equal(typeof list.teams[0].interrupted, "boolean");

  const snapshot: EventsSnapshot = {
    team_id: "t",
    status: "Running",
    active: false,
    interrupted: true,
    audit: [],
  };
  assert.equal(snapshot.interrupted, true);
});

// ---------------------------------------------------------------------------
// V1 三日 ProductEval 冻结契约（第四路）：四路由 + 六态 + 拓扑枚举的类型级断言。
// 本用例不发起网络请求——任何字段漂移都会先在 tsc 编译期失败。
// ---------------------------------------------------------------------------

test("ProductEval 冻结契约（创建/列表/详情/取消 + 六态 + workswarm≡multi 口径）", () => {
  type CreateBody = NonNullable<
    operations["createProductEvalRun"]["requestBody"]
  >["content"]["application/json"];
  type Accepted = operations["createProductEvalRun"]["responses"][202]["content"]["application/json"];
  type RunSummary = components["schemas"]["ProductEvalRunSummary"];
  type Detail = operations["getProductEvalRun"]["responses"][200]["content"]["application/json"];
  type Cancelled = operations["cancelProductEvalRun"]["responses"][200]["content"]["application/json"];

  // 冻结请求体：suite 仅 v1；execution reference|live；modes 单拓扑子集；repetitions 1..=20。
  const body: CreateBody = {
    suite: "v1",
    execution: "reference",
    modes: ["single", "workswarm"],
    repetitions: 1,
    category: null,
    only: null,
  };
  assert.equal(body.suite, "v1");
  assert.equal(body.execution, "reference");
  assert.deepEqual(body.modes, ["single", "workswarm"]);

  // 202 受理：eval-… run_id + queued。
  const accepted: Accepted = { run_id: "eval-abc123", status: "queued" };
  assert.match(accepted.run_id, /^eval-/);
  assert.equal(accepted.status, "queued");

  // 运行摘要：六态 + 进度 {done,total}；modes 保留请求字面量 single/workswarm。
  const summary: RunSummary = {
    run_id: accepted.run_id,
    suite: "v1",
    execution: "reference",
    modes: ["single", "workswarm"],
    repetitions: 1,
    status: "completed",
    created_at: "2026-08-27T09:00:00Z",
    planned_total: 4,
    progress: { done: 4, total: 4 },
  };
  assert.equal(summary.status, "completed");
  assert.equal(summary.progress.done, summary.progress.total);

  // 详情：summary 基底 + report（ProductEvalReport 原样，可为 null）。
  const detail: Detail = {
    ...summary,
    report: {
      schema_version: 1,
      suite_name: "v1-r1-product-suite",
      suite_hash: "sha256:deadbeef",
      execution: "reference",
      model: null,
      generated_at: "2026-08-27T09:01:00Z",
      runs: [
        {
          key: { case_id: "document-draft-note", agent_mode: "multi", repetition: 1 },
          category: "document",
          status: "passed",
          wall_ms: 12,
          model_calls: 0,
          prompt_tokens: null,
          completion_tokens: null,
          total_tokens: null,
          cost_usd: null,
          failed_steps: [],
          retries: 0,
          cancellations: 0,
          artifact_refs: ["artifacts/note.md"],
          tool_log: [],
          model: null,
          started_at: "2026-08-27T09:00:30Z",
          finished_at: "2026-08-27T09:00:31Z",
          error: null,
        },
      ],
      pending: [],
      metrics: {
        runs_total: 4,
        passed: 2,
        failed: 2,
        errors: 0,
        timeouts: 0,
        cancelled: 0,
        success_rate: 0.5,
        mean_wall_ms: 12,
        total_model_calls: 0,
        total_tokens: null,
        estimated_cost_usd: null,
      },
      per_case: [
        {
          case_id: "document-draft-note",
          category: "document",
          agent_mode: "multi",
          runs_total: 1,
          passed: 1,
          success_rate: 1,
          mean_wall_ms: 12,
          mean_model_calls: 0,
          total_tokens: null,
        },
      ],
    },
  };
  assert.equal(detail.report?.metrics.runs_total, 4);
  assert.deepEqual(detail.report?.runs[0].key, {
    case_id: "document-draft-note",
    agent_mode: "multi",
    repetition: 1,
  });
  const detailWithoutReport: Detail = { ...summary, report: null };
  assert.equal(detailWithoutReport.report, null);

  // 取消：幂等返回六态之一。
  const cancelled: Cancelled = { run_id: accepted.run_id, status: "cancelled" };
  assert.equal(cancelled.status, "cancelled");
  const completedNoOp: Cancelled = { run_id: accepted.run_id, status: "completed" };
  assert.equal(completedNoOp.status, "completed");

  // workswarm ≡ multi：结果 wire 的 agent_mode 用核心小写词（single/multi）。
  type ReportRun = NonNullable<Detail["report"]>["runs"][number];
  const multiMode: ReportRun["key"]["agent_mode"] = "multi";
  assert.equal(multiMode, "multi");
  const singleMode: ReportRun["key"]["agent_mode"] = "single";
  assert.equal(singleMode, "single");
});
