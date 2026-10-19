// 一次性脚本：把八期第四路的 10 条新路由写入 clients/ts/openapi.json 快照。
// 运行：node scripts/sync-eight-snapshot.mjs（在 agent-sdk 下）
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const p = path.join(here, "..", "clients", "ts", "openapi.json");
const s = JSON.parse(fs.readFileSync(p, "utf8"));

const param = { name: "id", in: "path", required: true, schema: { type: "string" } };
const csRef = {
  type: "object",
  additionalProperties: true,
  required: ["change_set_id", "status"],
  properties: {
    change_set_id: { type: "string" },
    team_id: { type: "string" },
    step_id: { type: "string" },
    role: { type: "string" },
    changed_files: { type: "array", items: { type: "string" } },
    diff_ref: { type: "string", nullable: true },
    status: { type: "string", enum: ["pending_review", "accepted", "rejected", "reverted", "conflicted"] },
    created_at: { type: "string" },
    resolved_at: { type: "string", nullable: true },
  },
};
const csResult = {
  type: "object",
  additionalProperties: true,
  properties: { change_set: csRef, replayed: { type: "boolean" } },
};

const add = {
  "/teams/{id}/change-sets": {
    get: {
      operationId: "teamChangeSets",
      parameters: [param],
      responses: {
        "200": {
          description: "八期（二路）：团队 ChangeSet 列表（写角色执行后自动生成；未接受 ChangeSet 阻断最终 approved head）",
          content: {
            "application/json": {
              schema: {
                type: "object",
                properties: { team_id: { type: "string" }, change_sets: { type: "array", items: csRef } },
                required: ["team_id", "change_sets"],
              },
            },
          },
        },
        "404": { description: "team not found" },
      },
    },
  },
  "/change-sets/{id}": {
    get: {
      operationId: "changeSetDetail",
      parameters: [param],
      responses: {
        "200": { description: "单个 ChangeSet", content: { "application/json": { schema: csRef } } },
        "404": { description: "change set not found" },
      },
    },
  },
  "/change-sets/{id}/accept": {
    post: {
      operationId: "changeSetAccept",
      parameters: [param],
      responses: {
        "200": {
          description: "接受变更（幂等重放 replayed:true 零副作用；解除 approved head 阻断）",
          content: { "application/json": { schema: csResult } },
        },
        "404": { description: "change set not found" },
        "409": { description: "文件被用户再次修改（status=conflicted，不覆盖用户新内容）或已终态跨动作" },
      },
    },
  },
  "/change-sets/{id}/reject": {
    post: {
      operationId: "changeSetReject",
      parameters: [param],
      responses: {
        "200": {
          description: "拒绝变更（仅恢复该 ChangeSet 修改的文件；幂等重放零副作用）",
          content: { "application/json": { schema: csResult } },
        },
        "404": { description: "change set not found" },
        "409": { description: "文件被用户再次修改或已终态跨动作" },
      },
    },
  },
  "/change-sets/{id}/revert": {
    post: {
      operationId: "changeSetRevert",
      parameters: [param],
      responses: {
        "200": {
          description: "安全撤销（恢复前逐文件比较当前哈希：=结果哈希→恢复、=基线哈希→跳过、否则 409 conflicted）",
          content: { "application/json": { schema: csResult } },
        },
        "404": { description: "change set not found" },
        "409": { description: "文件被用户再次修改或已终态跨动作" },
      },
    },
  },
  "/human/inbox": {
    get: {
      operationId: "humanInboxList",
      parameters: [
        { name: "kind", in: "query", required: false, schema: { type: "string", enum: ["human_result", "artifact_review", "change_set", "step_retry"] } },
        { name: "status", in: "query", required: false, schema: { type: "string", enum: ["open", "claimed", "resolved"] } },
        { name: "team_id", in: "query", required: false, schema: { type: "string" } },
        { name: "project_id", in: "query", required: false, schema: { type: "string" } },
      ],
      responses: {
        "200": {
          description: "统一待办列表（四类：human_result/artifact_review/change_set/step_retry；服务重启恢复、已解决不再出现）",
          content: {
            "application/json": {
              schema: {
                type: "object",
                properties: {
                  items: { type: "array", items: { type: "object", additionalProperties: true } },
                  counts: { type: "object", additionalProperties: true },
                },
                required: ["items", "counts"],
              },
            },
          },
        },
      },
    },
  },
  "/human/inbox/{id}": {
    get: {
      operationId: "humanInboxItem",
      parameters: [param],
      responses: {
        "200": {
          description: "单条待办 {item}",
          content: {
            "application/json": {
              schema: { type: "object", additionalProperties: true, properties: { item: { type: "object", additionalProperties: true } } },
            },
          },
        },
        "404": { description: "待办不存在" },
      },
    },
  },
  "/human/inbox/{id}/claim": {
    post: {
      operationId: "humanInboxClaim",
      parameters: [param],
      requestBody: {
        content: { "application/json": { schema: { type: "object", properties: { user: { type: "string" } }, required: ["user"] } } },
      },
      responses: {
        "200": { description: "领取成功（同人重复领取幂等）{item}" },
        "400": { description: "user 为空" },
        "404": { description: "待办不存在" },
        "409": { description: "已被其他用户领取" },
      },
    },
  },
  "/human/inbox/{id}/release": {
    post: {
      operationId: "humanInboxRelease",
      parameters: [param],
      requestBody: {
        content: { "application/json": { schema: { type: "object", properties: { user: { type: "string" } }, required: ["user"] } } },
      },
      responses: {
        "200": { description: "释放成功（仅领取者）{item}" },
        "400": { description: "user 为空" },
        "404": { description: "待办不存在" },
        "409": { description: "已被其他用户领取或状态不允许" },
      },
    },
  },
  "/human/inbox/{id}/resolve": {
    post: {
      operationId: "humanInboxResolve",
      parameters: [param],
      requestBody: {
        content: {
          "application/json": {
            schema: {
              type: "object",
              additionalProperties: true,
              properties: {
                user: { type: "string", description: "处理人（审计/领取校验；review 类缺省兼作 reviewer）" },
                resolution_id: { type: "string", description: "幂等键（兼容别名 idempotency_key）" },
                decision: { type: "string", enum: ["approve", "request_changes", "reject"], description: "artifact_review" },
                action: { type: "string", enum: ["accept", "reject"], description: "change_set" },
                result: { type: "string", description: "human_result 结果文本" },
                reviewer: { type: "string" },
                comment: { type: "string" },
                expected_version: { type: "integer" },
                note: { type: "string" },
              },
            },
          },
        },
      },
      responses: {
        "200": {
          description: "按 kind 分派到既有领域能力（不绕过原权限与幂等检查）{resolved, replayed, item_id, kind, result}",
          content: {
            "application/json": {
              schema: {
                type: "object",
                additionalProperties: true,
                properties: {
                  resolved: { type: "boolean" },
                  replayed: { type: "boolean" },
                  item_id: { type: "string" },
                  kind: { type: "string" },
                  result: { type: "object", additionalProperties: true },
                },
              },
            },
          },
        },
        "400": { description: "请求体与 kind 不匹配" },
        "404": { description: "待办不存在" },
        "409": { description: "未被领取/已被他人领取/领域冲突" },
      },
    },
  },
};

let added = 0;
for (const k of Object.keys(add)) {
  if (s.paths[k]) {
    console.log("SKIP (exists):", k);
    continue;
  }
  s.paths[k] = add[k];
  added += 1;
}
fs.writeFileSync(p, JSON.stringify(s, null, 2) + "\n", "utf8");
console.log("added:", added, " snapshot paths now:", Object.keys(s.paths).length);
