import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const domain = readFileSync(new URL("../app-domain.js", import.meta.url), "utf8");
const helper = domain.match(/function syncMcpFields\(\) \{[\s\S]*?\n\}/);
assert.ok(helper, "MCP transport field controller must exist");

function runFor(transport) {
  const inputs = {
    mcpCommand: { disabled: false, required: false },
    mcpUrl: { disabled: false, required: false },
    mcpTransport: { value: transport },
  };
  const groups = {
    stdio: { hidden: false },
    http: { hidden: false },
  };
  const context = {
    $: (id) => inputs[id],
    document: {
      querySelector(selector) {
        const kind = selector.match(/data-mcp-group="([^"]+)"/)[1];
        return groups[kind];
      },
    },
  };
  runInNewContext(helper[0] + "\nsyncMcpFields();", context);
  return { inputs: { mcpCommand: inputs.mcpCommand, mcpUrl: inputs.mcpUrl }, groups };
}

test("stdio enables and requires only the command field", () => {
  const { inputs, groups } = runFor("stdio");
  assert.deepEqual(inputs.mcpCommand, { disabled: false, required: true });
  assert.deepEqual(inputs.mcpUrl, { disabled: true, required: false });
  assert.deepEqual(groups, { stdio: { hidden: false }, http: { hidden: true } });
});

test("http enables and requires only the URL field", () => {
  const { inputs, groups } = runFor("http");
  assert.deepEqual(inputs.mcpCommand, { disabled: true, required: false });
  assert.deepEqual(inputs.mcpUrl, { disabled: false, required: true });
  assert.deepEqual(groups, { stdio: { hidden: true }, http: { hidden: false } });
});
