// sync-openapi-snapshot.mjs — 把第三路评审闭环契约写入权威快照 clients/ts/openapi.json。
// 快照与 served /openapi.json 路径集合双向一致（route_contract_tests 断言），组件同步。
import { readFileSync, writeFileSync } from "node:fs";

const SNAP = new URL("../openapi.json", import.meta.url);
const snap = JSON.parse(readFileSync(SNAP, "utf8"));

const reviewPath = {
    post: {
        operationId: "artifactSubmitReview",
        parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }],
        requestBody: {
            content: {
                "application/json": {
                    schema: {
                        type: "object",
                        properties: {
                            team_id: { type: "string" },
                            decision: {
                                type: "string",
                                enum: ["approve", "request_changes", "reject"],
                                description: "评审决定（snake_case）",
                            },
                            reviewer: { type: "string", description: "评审者（member_id / user_id / 角色名）" },
                            comment: { type: "string" },
                            expected_version: {
                                type: "integer",
                                description: "乐观并发目标版本；缺省跳过版本校验；不符 → 409",
                            },
                            idempotency_key: { type: "string", description: "幂等键；同键重放零副作用返回既有记录" },
                        },
                        required: ["team_id", "decision", "reviewer", "idempotency_key"],
                    },
                },
            },
        },
        responses: {
            "201": {
                description: "review recorded; body = { replayed: false, review, artifact, approved_head }",
                content: {
                    "application/json": {
                        schema: {
                            type: "object",
                            properties: {
                                replayed: { type: "boolean" },
                                review: { $ref: "#/components/schemas/ArtifactReviewRecord" },
                                artifact: { type: "object", description: "评审后的 Artifact（review_state 已迁移）" },
                                approved_head: approvedHeadSchemaNullable(),
                            },
                            required: ["replayed", "review", "artifact"],
                        },
                    },
                },
            },
            "200": {
                description: "idempotent replay（同幂等键重放，零副作用；replayed: true）",
                content: {
                    "application/json": {
                        schema: {
                            type: "object",
                            properties: {
                                replayed: { type: "boolean", description: "恒为 true（回放既有记录）" },
                                review: { $ref: "#/components/schemas/ArtifactReviewRecord" },
                                artifact: { type: "object", description: "评审后的 Artifact（当前状态）" },
                                approved_head: approvedHeadSchemaNullable(),
                            },
                            required: ["replayed", "review", "artifact"],
                        },
                    },
                },
            },
            "400": { description: "validation failed（未知 decision）" },
            "403": {
                description:
                    "producer self-approve without human policy authorization（需 self_review_allowed）",
            },
            "404": { description: "artifact or team not found" },
            "409": {
                description:
                    "expected_version stale（旧页面提交）/ artifact superseded / idempotency key reused on other artifact",
            },
            "422": { description: "missing required field（Json extractor 语义）" },
        },
    },
};

const historyPath = {
    get: {
        operationId: "artifactReviewHistory",
        parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }],
        responses: {
            "200": {
                description: "review history (asc) + version chain (supersedes/superseded_by) + approved head",
                content: {
                    "application/json": {
                        schema: {
                            type: "object",
                            properties: {
                                artifact_id: { type: "string" },
                                kind: { type: "string" },
                                version: { type: "integer" },
                                producer: { type: "string" },
                                review_state: {
                                    type: "string",
                                    enum: ["draft", "pending_review", "approved", "rejected", "superseded"],
                                },
                                supersedes_artifact_id: { type: "string", nullable: true },
                                superseded_by: { type: "string", nullable: true },
                                reviews: {
                                    type: "array",
                                    items: { $ref: "#/components/schemas/ArtifactReviewRecord" },
                                },
                                approved_head: {
                                    ...approvedHeadSchema(),
                                    nullable: true,
                                },
                            },
                            required: [
                                "artifact_id",
                                "kind",
                                "version",
                                "producer",
                                "review_state",
                                "reviews",
                                "approved_head",
                            ],
                        },
                    },
                },
            },
            "404": { description: "artifact not found" },
        },
    },
};

function approvedHeadSchema() {
    return {
        type: "object",
        description: "(project, kind) 的当前 approved head Artifact（指向真实存在且已批准版本；否则 null）",
        properties: {
            artifact_id: { type: "string" },
            kind: { type: "string" },
            version: { type: "integer" },
            producer: { type: "string" },
            review_state: {
                type: "string",
                enum: ["draft", "pending_review", "approved", "rejected", "superseded"],
            },
            content_ref: { type: "string" },
            supersedes_artifact_id: { type: "string", nullable: true },
            created_at: { type: "string" },
        },
        required: ["artifact_id", "kind", "version", "review_state"],
    };
}

function approvedHeadSchemaNullable() {
    return { ...approvedHeadSchema(), nullable: true };
}

snap.paths["/artifacts/{id}/review"] = reviewPath;
snap.paths["/artifacts/{id}/history"] = historyPath;
snap.components ||= {};
snap.components.schemas ||= {};
snap.components.schemas["ArtifactReviewRecord"] = {
    type: "object",
    description: "不可变评审记录（V1 四期第三路；只增不改，append-only）",
    properties: {
        review_id: { type: "string" },
        artifact_id: { type: "string" },
        artifact_version: { type: "integer", description: "被评审的产物版本" },
        team_id: { type: "string" },
        decision: { type: "string", enum: ["approve", "request_changes", "reject"] },
        reviewer: { type: "string" },
        comment: { type: "string" },
        idempotency_key: { type: "string", description: "唯一约束；同键重放零副作用" },
        content_ref: { type: "string", description: "评审时的产物内容引用（取证锚点）" },
        created_at: { type: "string" },
    },
    required: [
        "review_id",
        "artifact_id",
        "artifact_version",
        "team_id",
        "decision",
        "reviewer",
        "idempotency_key",
        "created_at",
    ],
};

// 保持既有缩进风格（检测原文件第二行缩进宽度）。
writeFileSync(SNAP, JSON.stringify(snap, null, 4) + "\n", "utf8");
console.log(
    "snapshot updated: paths=" + Object.keys(snap.paths).length +
    " schemas=" + Object.keys(snap.components.schemas).length,
);
