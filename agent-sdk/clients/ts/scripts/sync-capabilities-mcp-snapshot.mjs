/**
 * R0 门禁收口：把服务端 /openapi.json 已提供的能力目录与 MCP 运行时路由
 * （crates/owo-agent-server/src/lib.rs openapi_spec，§8.3/任务 10）同步进权威
 * 契约快照 clients/ts/openapi.json。
 *
 * 条目与 lib.rs 902-905 逐字一致；幂等：重复执行不产生变更。
 * 运行：node clients/ts/scripts/sync-capabilities-mcp-snapshot.mjs
 */
import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const snapshotPath = join(here, "..", "openapi.json");

// 与 lib.rs openapi_spec 逐字一致的四条路由定义。
const ENTRIES = {
  "/mcp/health": {
    get: {
      operationId: "mcpHealthSnapshot",
      responses: {
        200: {
          description:
            "per-server MCP health (state machine, circuit breaker, failure counters)",
        },
      },
    },
  },
  "/mcp/reconnect": {
    post: {
      operationId: "mcpReconnect",
      requestBody: {
        content: {
          "application/json": {
            schema: {
              type: "object",
              properties: { name: { type: "string" } },
              required: ["name"],
            },
          },
        },
      },
      responses: {
        200: {
          description:
            "server reconnected from saved config (process-level uninstall + hot connect)",
        },
      },
    },
  },
  "/mcp/enabled": {
    post: {
      operationId: "mcpEnabled",
      requestBody: {
        content: {
          "application/json": {
            schema: {
              type: "object",
              properties: {
                name: { type: "string" },
                enabled: { type: "boolean" },
              },
              required: ["name", "enabled"],
            },
          },
        },
      },
      responses: {
        200: {
          description:
            "tool prefix enable/disable (process-level, model-invisible, not persisted)",
        },
      },
    },
  },
  "/capabilities": {
    get: {
      operationId: "capabilitiesList",
      responses: {
        200: {
          description:
            "capability catalog (single source for UI/CLI/diagnostics/help, §8.3)",
        },
      },
    },
  },
};

const raw = readFileSync(snapshotPath, "utf8");
const spec = JSON.parse(raw);
const paths = spec.paths;

// 重建键序：现有键保持原顺序；新键插到锚点之后（/mcp/add 后放 mcp 三条，
// /automations 组后放 /capabilities），保证与 lib.rs 相邻语义、diff 最小。
function insertAfter(source, anchor, additions) {
  const out = {};
  for (const [key, value] of Object.entries(source)) {
    out[key] = value;
    if (key === anchor) {
      for (const [k, v] of Object.entries(additions)) {
        if (!(k in out)) out[k] = v;
      }
    }
  }
  return out;
}

const mcpAdditions = {};
for (const key of ["/mcp/health", "/mcp/reconnect", "/mcp/enabled"]) {
  if (!(key in paths)) mcpAdditions[key] = ENTRIES[key];
}
const capAdditions = {};
if (!("/capabilities" in paths)) capAdditions["/capabilities"] = ENTRIES["/capabilities"];

let next = paths;
if (Object.keys(mcpAdditions).length) next = insertAfter(next, "/mcp/add", mcpAdditions);
if (Object.keys(capAdditions).length) next = insertAfter(next, "/automations", capAdditions);

if (next === paths) {
  console.log("[sync] 快照已含全部四条路由，无变更（幂等）。");
} else {
  // 兜底：锚点缺失时直接追加尾部（当前快照两条锚点均存在，不应走到）。
  for (const [k, v] of Object.entries(next)) {
    if (!(k in paths)) paths[k] = v;
  }
  const written = JSON.stringify(spec);
  JSON.parse(written); // 写回前再次解析校验
  writeFileSync(snapshotPath, written, "utf8");
  const check = JSON.parse(readFileSync(snapshotPath, "utf8"));
  for (const key of Object.keys(ENTRIES)) {
    if (!(key in check.paths)) throw new Error("写回后校验失败：缺 " + key);
  }
  console.log("[sync] 已写入 4 条路由；paths 总数 = " + Object.keys(check.paths).length);
}
