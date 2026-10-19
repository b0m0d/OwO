/**
 * 七期第四路：openapi.json 快照同步（增量、行级拼接，保留既有格式与 CRLF）。
 * 同步 /teams/{id}（worker_profiles / write_lease / changes）与
 * /projects/{id}/artifacts（validation / sha256 / size_bytes / evidence_refs）
 * 到 crates/owo-agent-server/src/lib.rs openapi_spec（七期 additive wire 契约）。
 * 一次性脚本；执行后 openapi.json 解析校验通过才写回。
 * 已执行（七期第四路，快照增量落盘）；重跑会因 expect 前置断言行号漂移而失败，属预期保护。
 * 后续三路 wire 后应以 sync-openapi-snapshot.mjs 从服务端重新同步整体快照。
 */
import { readFileSync, writeFileSync } from "node:fs";

const path = "T:/创新创业/OwO-master/agent-sdk/clients/ts/openapi.json";
const raw = readFileSync(path, "utf8");
if (raw.replace(/\r\n/g, "").includes("\n")) throw new Error("存在非 CRLF 换行");
const lines = raw.split("\r\n");

function expect(n, frag) {
  const L = lines[n - 1];
  if (!L.includes(frag)) throw new Error(`line ${n} mismatch: ${L.slice(0, 80)}`);
}
expect(5481, '"version":  {');
expect(5482, '"type":  "integer"');
expect(5483, "}");
expect(5484, "},");
expect(7121, '"team":  {');
expect(7122, '"description":  "TeamRun');
expect(7124, "}");
expect(7136, '"team + task view + audit tail; R2 additive: interrupted"');

const P = (n, s = "") => s.padEnd(n, " ");

// ---- /projects/{id}/artifacts：items 追加七期（三路）字段（插在 version 之后；末行无逗号） ----
const arts = [
  P(219, '"evidence_refs":  { "description":  "七期（三路）additive：证据引用", "items":  { "type":  "string" }, "type":  "array" },'),
  P(219, '"sha256":  { "description":  "七期（三路）additive：内容 SHA256（hex）", "type":  "string" },'),
  P(219, '"size_bytes":  { "description":  "七期（三路）additive：内容字节数", "type":  "integer" },'),
  P(219, '"validation":  {'),
  P(235, '"description":  "七期（三路）additive：格式校验 {format, valid, reason?}",'),
  P(235, '"properties":  {'),
  P(251, '"format":  { "type":  "string" },'),
  P(251, '"reason":  { "nullable":  true, "type":  "string" },'),
  P(251, '"valid":  { "type":  "boolean" }'),
  P(235, "},"),
  P(235, '"type":  "object"'),
  P(215, "}"),
];

// ---- /teams/{id}：追加 worker_profiles / write_lease / changes（插在 team 之后；末行无逗号） ----
const teams = [
  P(151, '"changes":  {'),
  P(168, '"description":  "七期（二路 wire）additive：文件变更列表（与既有 workspace git-status 路由可互用；diff 预览容错读 diff / diff_content 双键）",'),
  P(168, '"items":  {'),
  P(184, '"properties":  {'),
  P(200, '"added_lines":  { "nullable":  true, "type":  "integer" },'),
  P(200, '"deleted_lines":  { "nullable":  true, "type":  "integer" },'),
  P(200, '"diff":  { "description":  "可选：该文件的 unified diff（UI 容错读 diff / diff_content 双键）", "nullable":  true, "type":  "string" },'),
  P(200, '"path":  { "type":  "string" },'),
  P(200, '"state":  { "enum":  ["added", "modified", "deleted"], "type":  "string" }'),
  P(184, "},"),
  P(184, '"required":  ["path", "state"],'),
  P(184, '"type":  "object"'),
  P(168, "},"),
  P(168, '"nullable":  true,'),
  P(168, '"type":  "array"'),
  P(151, "},"),
  P(151, '"write_lease":  {'),
  P(168, '"description":  "七期（二路 wire）additive：单一写租约（null = 未持有；取消中团队状态 stopping/stopped 渲染为 正在停止/已停止）",'),
  P(168, '"nullable":  true,'),
  P(168, '"properties":  {'),
  P(184, '"acquired_at_ms":  { "type":  "integer" },'),
  P(184, '"holder_role":  { "type":  "string" },'),
  P(184, '"holder_step_id":  { "type":  "string" },'),
  P(184, '"released_at_ms":  { "nullable":  true, "type":  "integer" }'),
  P(168, "},"),
  P(168, '"required":  ["holder_role", "holder_step_id", "acquired_at_ms"],'),
  P(168, '"type":  "object"'),
  P(151, "},"),
  P(151, '"worker_profiles":  {'),
  P(168, '"description":  "七期（二路 wire）additive：按角色 WorkerProfile（工具权限 + 调用预算）；旧记录缺失 → UI 缺省空/false/null",'),
  P(168, '"items":  {'),
  P(184, '"properties":  {'),
  P(200, '"can_run_command":  { "type":  "boolean" },'),
  P(200, '"can_use_browser":  { "type":  "boolean" },'),
  P(200, '"max_turns":  { "nullable":  true, "type":  "integer" },'),
  P(200, '"read_only":  { "type":  "boolean" },'),
  P(200, '"role":  { "type":  "string" },'),
  P(200, '"visible_tools":  { "items":  { "type":  "string" }, "type":  "array" },'),
  P(200, '"write_allowed_paths":  { "items":  { "type":  "string" }, "nullable":  true, "type":  "array" }'),
  P(184, "},"),
  P(184, '"required":  ["role", "visible_tools", "read_only", "can_use_browser", "can_run_command"],'),
  P(184, '"type":  "object"'),
  P(168, "},"),
  P(168, '"nullable":  true,'),
  P(168, '"type":  "array"'),
  P(151, "}"),
];

// 自底向上：先 7136 描述，再 7124 后插入，再 7122 描述，最后 5483 后插入
lines[7135] = lines[7135].replace(
  '"team + task view + audit tail; R2 additive: interrupted"',
  '"team + task view + audit tail; R2 additive: interrupted; 七期 additive（二/三路 wire）: worker_profiles / write_lease / changes（字段可能位于响应顶层或 team 对象内，UI 双路径容错读取）"',
);
lines.splice(7124, 0, ...teams);
lines[7123] += ","; // team 由末位属性变为非末位：补逗号
lines[7121] = lines[7121].replace(
  /"description":  "[^"]*"/,
  '"description":  "TeamRun（透传）；七期 additive 字段（worker_profiles/write_lease/changes，见顶层同名属性）亦可能位于 team 对象内"',
);
lines.splice(5483, 0, ...arts);
lines[5482] += ","; // version 由末位属性变为非末位：补逗号

// 校验 + 结构断言
const joined = lines.join("\r\n");
let json;
try {
  json = JSON.parse(joined); // 失败即抛
} catch (e) {
  const m = /position (\d+)/.exec(String(e.message));
  const pos = m ? +m[1] : 0;
  const ctx = joined.slice(0, pos).split("\r\n").slice(-10).map((l) => JSON.stringify(l.trim().slice(0, 200)));
  console.error("parse failed; context lines:\n" + ctx.join("\n"));
  writeFileSync(path + ".debug", joined, "utf8");
  console.error("mutated content written to " + path + ".debug");
  throw e;
}
const team200 = json.paths["/teams/{id}"].get.responses["200"].content["application/json"].schema.properties;
for (const k of ["worker_profiles", "write_lease", "changes"]) {
  if (!team200[k]) throw new Error(`teams/{id} 缺少 ${k}`);
}
const artItems =
  json.paths["/projects/{id}/artifacts"].get.responses["200"].content["application/json"].schema.properties.artifacts.items.properties;
for (const k of ["validation", "sha256", "size_bytes", "evidence_refs"]) {
  if (!artItems[k]) throw new Error(`artifacts items 缺少 ${k}`);
}
writeFileSync(path, joined, "utf8");
console.log(
  "ok: teams/{id} properties =",
  Object.keys(team200).join(","),
  "; artifacts items =",
  Object.keys(artItems).join(","),
);
