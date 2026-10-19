const fs = require("node:fs");
const path = require("node:path");

// Only an explicit user-selected path is a project workspace. The Electron main
// process may still use the home directory as a safe startup fallback.
function configuredWorkspacePath(dataRoot) {
  try {
    const file = path.join(dataRoot, "workspace.json");
    const parsed = JSON.parse(fs.readFileSync(file, "utf8"));
    if (parsed && typeof parsed.path === "string" && parsed.path.trim()) return parsed.path;
  } catch (_) {
    // Missing or malformed preferences mean there is no explicitly configured workspace.
  }
  return "";
}

module.exports = { configuredWorkspacePath };
