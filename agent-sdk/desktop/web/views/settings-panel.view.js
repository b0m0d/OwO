/* R10 模型设置面板：模型切换 + 自定义接口地址（base_url）+ 自定义模型名。
 *
 * 用户反馈（2026-09-22）："我没找到模型切换功能，自定义模型地址和名称"。
 * 事实是这些能力早就在壳里（`set_provider` 命令接受 base_url/model，provider.rs
 * 落盘 `%LOCALAPPDATA%\OwO\Agent\provider.json`，重启核心时经 OPENAI_BASE_URL /
 * OPENAI_MODEL 注入），但入口藏在两处几乎看不见的地方：
 *   1. 侧栏底部「显示工具与设置」→「设置与诊断」→ 第一个 select；
 *   2. 首次启动引导页里的提供商卡（且 base_url/model 输入框**只在选中"云端"
 *      单选框时才显示**，默认不显示 → 视觉上就是"没有自定义地址这一项"）。
 * 本模块把入口提到一级导航（左栏「模型」）并显式暴露端点与模型名两个字段，
 * 三条保存路径（引导页 / 设置页 / 状态条"模型"段）共用同一个壳命令与口径。
 */
(function (global) {
  "use strict";

  // 常用服务提供方一键预设：唯一事实源在 config/provider-presets.js（与引导页共用），
  // 只填地址与默认模型名，不改密钥（密钥始终只读环境变量）。
  function presetList() {
    if (global.OwoProviderPresets && typeof global.OwoProviderPresets.presets === "function") {
      return global.OwoProviderPresets.presets();
    }
    return [];
  }

  function ollamaBaseUrl() {
    if (global.OwoProviderPresets && typeof global.OwoProviderPresets.ollamaBaseUrl === "function") {
      return global.OwoProviderPresets.ollamaBaseUrl();
    }
    return "";
  }

  function esc(value) {
    return String(value == null ? "" : value).replace(/[&<>"']/g, function (ch) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[ch];
    });
  }

  function invokeOwner() {
    var internal = global.__TAURI_INTERNALS__;
    var publicCore = global.__TAURI__ && global.__TAURI__.core;
    var owner = publicCore && typeof publicCore.invoke === "function" ? publicCore : internal;
    return owner && typeof owner.invoke === "function" ? owner : null;
  }

  function invoke(command, args) {
    var owner = invokeOwner();
    if (!owner) return Promise.reject(new Error("非桌面环境"));
    return Promise.resolve(owner.invoke.call(owner, command, args || {}));
  }

  function $(id) {
    return document.getElementById(id);
  }

  function renderPresets() {
    var root = $("modelPresets");
    if (!root) return;
    root.replaceChildren();
    presetList().forEach(function (preset) {
      var button = document.createElement("button");
      button.type = "button";
      button.className = "model-preset";
      button.textContent = preset.label;
      button.title = preset.baseUrl ? preset.label + " · " + preset.baseUrl : "填入你自己的 OpenAI 兼容地址";
      button.addEventListener("click", function () {
        if (preset.baseUrl) {
          $("settingsBaseUrl").value = preset.baseUrl;
        } else {
          $("settingsBaseUrl").value = "";
          $("settingsBaseUrl").focus();
        }
        if (preset.model) $("settingsModelName").value = preset.model;
        // 本地 Ollama 不需要密钥：自动切到本地模式，避免用户在"云端无密钥"上卡住。
        $("settingsProvider").value = preset.id === "ollama" ? "ollama" : "cloud";
        if (preset.model) rememberModelName(preset.model);
        setHint(
          "已填入 " + preset.label + (preset.model ? " · " + preset.model : "") +
            (preset.keyEnv ? "；密钥请放在环境变量 " + preset.keyEnv + "（本应用不保存密钥）" : "；本地端点不需要密钥"),
          false
        );
      });
      root.appendChild(button);
    });
  }

  var MODEL_NAME_KEY = "owo.model-names";

  function knownModelNames() {
    try {
      var parsed = JSON.parse(localStorage.getItem(MODEL_NAME_KEY) || "[]");
      return Array.isArray(parsed) ? parsed.filter(function (name) {
        return typeof name === "string" && name.trim();
      }) : [];
    } catch (_) {
      return [];
    }
  }

  function rememberModelName(name) {
    if (!name) return;
    var names = knownModelNames().filter(function (existing) {
      return existing !== name;
    });
    names.unshift(name);
    try {
      localStorage.setItem(MODEL_NAME_KEY, JSON.stringify(names.slice(0, 12)));
    } catch (_) {
      /* 隐私模式等写不进去：不影响保存模型本身 */
    }
    renderModelNameCandidates();
  }

  function renderModelNameCandidates() {
    var list = $("modelNameCandidates");
    if (!list) return;
    list.replaceChildren();
    knownModelNames().forEach(function (name) {
      var option = document.createElement("option");
      option.value = name;
      list.appendChild(option);
    });
  }

  function setHint(text, ok) {
    var hint = $("modelHint");
    if (!hint) return;
    hint.textContent = text || "";
    hint.classList.toggle("ok", !!ok);
    hint.classList.toggle("bad", ok === false && !!text);
  }

  function syncProviderFields() {
    var provider = $("settingsProvider").value;
    var editable = provider !== "unset";
    $("settingsBaseUrl").disabled = !editable;
    $("settingsModelName").disabled = !editable;
    // 本地 Ollama 不需要密钥：禁用密钥输入并说明原因，避免用户对着空字段发愁。
    var local = provider === "ollama";
    if ($("settingsApiKey")) $("settingsApiKey").disabled = local;
    if ($("settingsApiKeyEnv")) $("settingsApiKeyEnv").disabled = local || !editable;
    if ($("modelClearKeyBtn")) $("modelClearKeyBtn").disabled = local;
    if (local && !$("settingsBaseUrl").value) {
      $("settingsBaseUrl").value = ollamaBaseUrl();
      if (!$("settingsModelName").value) $("settingsModelName").value = "local";
    }
    if (!editable) {
      setHint("未选择服务提供方：核心仍会启动（诊断/设置可用），但模型调用不可用。", false);
    }
  }

  /// 读取壳侧权威状态（IPC，零 HTTP）并回灌到表单与只读展示。
  function refresh() {
    if (!invokeOwner()) {
      setHint("当前不是桌面环境：模型设置请在 OwO Agent 桌面应用中修改。", false);
      return Promise.resolve(null);
    }
    return invoke("get_provider_status").then(function (state) {
      if (!state) return null;
      if ($("settingsProvider")) $("settingsProvider").value = state.provider || "unset";
      if ($("settingsBaseUrl")) $("settingsBaseUrl").value = state.baseUrl || "";
      if ($("settingsModelName")) $("settingsModelName").value = state.model || "";
      // 密钥输入框永远不回填真实值（壳只回掩码）：留空 = 保持文件里已有的密钥，
      // 这样"改个模型名"不会顺手把密钥清掉。掩码放在 placeholder 里提示已存有。
      var keyInput = $("settingsApiKey");
      if (keyInput) {
        keyInput.value = "";
        keyInput.placeholder = state.keyConfigured && state.keySource === "config_file"
          ? "已保存：" + (state.keyMasked || "****") + "（留空则不修改）"
          : "留空则读环境变量 " + (state.keyEnv || "OPENAI_API_KEY");
      }
      if ($("settingsApiKeyEnv")) $("settingsApiKeyEnv").value = state.keyEnv || "";
      // R13：可调参数回填（空 = 用核心默认，界面显示为空而不是编造一个数字）。
      if ($("settingsContextWindow")) $("settingsContextWindow").value = state.contextWindow || "";
      if ($("settingsMaxOutput")) $("settingsMaxOutput").value = state.maxOutputTokens || "";
      if ($("settingsTemperature")) $("settingsTemperature").value = state.temperature != null ? state.temperature : "";
      if ($("settingsTimeout")) $("settingsTimeout").value = state.timeoutSecs || "";
      if ($("settingsKeepRecent")) $("settingsKeepRecent").value = state.keepRecent || "";
      if ($("settingsCompaction")) {
        $("settingsCompaction").value = state.compaction === true ? "1" : state.compaction === false ? "0" : "";
      }
      // 模型清单：来自配置文件 model.models（用户可维护）+ 历史用过的 + 当前生效的。
      if (Array.isArray(state.models)) state.models.forEach(rememberModelName);
      if (state.model) rememberModelName(state.model);
      if ($("modelConfigPath")) $("modelConfigPath").textContent = state.configPath || "%LOCALAPPDATA%\\OwO\\Agent\\config.json";
      if ($("runtimeProvider")) {
        $("runtimeProvider").textContent = providerLabel(state.provider);
      }
      if ($("runtimeEndpoint")) {
        $("runtimeEndpoint").textContent = state.baseUrl || "将使用内置默认端点";
      }
      if ($("runtimeCredential")) {
        $("runtimeCredential").textContent = state.keyConfigured
          ? "已配置 · 来源：" + sourceLabel(state.keySource) + (state.keyMasked ? " · " + state.keyMasked : "")
          : state.provider === "ollama"
            ? "本地端点不需要密钥"
            : "未配置（可在此填写，或设置环境变量 " + (state.keyEnv || "OPENAI_API_KEY") + "）";
      }
      // 实际生效模型下拉：只有一处模型名，避免"下拉显示 A、输入框显示 B"的双真相。
      var select = $("settingsModel");
      if (select) {
        var model = state.model || "";
        select.replaceChildren(new Option(model || "未配置", model));
        select.value = model;
      }
      syncProviderFields();
      if (!state.ready) {
        setHint("当前不可发起模型调用：请确认接口地址、模型名与凭据" +
          (state.provider === "ollama" ? "（本地端点无需密钥）" : "（配置文件 model.api_key 或环境变量）") + "。", false);
      } else {
        setHint("已就绪：" + (state.baseUrl || "内置端点") + " · " + (state.model || "默认模型"), true);
      }
      return state;
    }).catch(function (error) {
      setHint("读取模型状态失败：" + String((error && error.message) || error), false);
      return null;
    });
  }

  function providerLabel(id) {
    var map = {
      bigmodel: "智谱 BigModel", openai: "OpenAI", deepseek: "DeepSeek",
      dashscope: "阿里 DashScope 兼容", ollama: "本地 Ollama",
      custom: "自定义（OpenAI 兼容）", unset: "未显式选择",
    };
    return map[id] || id || "未知";
  }

  function sourceLabel(id) {
    var map = {
      config_file: "配置文件 model.api_key",
      config_env: "配置指定的环境变量",
      environment: "环境变量 OPENAI_API_KEY",
      legacy_env: "历史环境变量 DASHSCOPE_API_KEY",
      none: "无",
    };
    return map[id] || id || "未知";
  }

  function apply() {
    var provider = $("settingsProvider").value;
    var baseUrl = $("settingsBaseUrl").value.trim();
    var model = $("settingsModelName").value.trim();
    var apiKey = $("settingsApiKey") ? $("settingsApiKey").value.trim() : "";
    var apiKeyEnv = $("settingsApiKeyEnv") ? $("settingsApiKeyEnv").value.trim() : "";
    if (provider !== "unset" && provider !== "ollama" && !baseUrl) {
      setHint("请填写接口地址（base_url），或从上方预设里选一个服务提供方。", false);
      $("settingsBaseUrl").focus();
      return Promise.resolve(false);
    }
    if (baseUrl && !/^https?:\/\//.test(baseUrl)) {
      setHint("接口地址必须以 http:// 或 https:// 开头。", false);
      $("settingsBaseUrl").focus();
      return Promise.resolve(false);
    }
    if (provider !== "unset" && !model) {
      setHint("请填写模型名称（例如 glm-5.3-flash）。", false);
      $("settingsModelName").focus();
      return Promise.resolve(false);
    }
    var button = $("modelApplyBtn");
    if (button) button.disabled = true;
    setHint("正在保存到 config.json 并重启核心服务…", true);
    return invoke("set_model_config", {
      mode: provider,
      base_url: baseUrl,
      model: model,
      // 空字符串 = 不改动已存密钥（壳侧语义：None 保持、Some("") 清除）。
      // 要显式清除请用「清除已存密钥」按钮——避免"改个模型名顺手把密钥清空"。
      api_key: apiKey ? apiKey : null,
      api_key_env: apiKeyEnv || null,
      context_window: $("settingsContextWindow") ? $("settingsContextWindow").value.trim() : null,
      max_output_tokens: $("settingsMaxOutput") ? $("settingsMaxOutput").value.trim() : null,
      temperature: $("settingsTemperature") ? $("settingsTemperature").value.trim() : null,
      timeout_secs: $("settingsTimeout") ? $("settingsTimeout").value.trim() : null,
      keep_recent: $("settingsKeepRecent") ? $("settingsKeepRecent").value.trim() : null,
      compaction: $("settingsCompaction") ? $("settingsCompaction").value.trim() : null,
      models: knownModelNames(),
    }).then(function (result) {
      var ok = !!(result && result.ok);
      if (!ok) {
        setHint((result && result.error) || "保存失败", false);
        return false;
      }
      rememberModelName(result.model || model);
      setHint("已保存到 " + (result.configPath || "config.json") + "：" +
        (result.baseUrl || "内置端点") + " · " + (result.model || "默认模型") +
        (result.keyConfigured ? " · 凭据：" + sourceLabel(result.keySource) : " · 尚未配置凭据") +
        "；核心服务已按新配置重启。", true);
      // 壳已 replace 核心进程：让前端整体重握手（端口与 bearer 都会换代）。
      if (global.OwoApi && typeof global.OwoApi.resetCoreConnection === "function") {
        global.OwoApi.resetCoreConnection();
      }
      if (typeof global.owoRecoverService === "function") {
        global.owoRecoverService();
      }
      return refresh().then(function () { return true; });
    }).catch(function (error) {
      setHint("保存失败：" + String((error && error.message) || error), false);
      return false;
    }).finally(function () {
      if (button) button.disabled = false;
    });
  }

  /// 显式清除配置文件里的密钥（环境变量路径不受影响）。
  function clearStoredKey() {
    var button = $("modelClearKeyBtn");
    if (button) button.disabled = true;
    setHint("正在清除配置文件中的 API Key…", true);
    return invoke("set_model_config", { mode: $("settingsProvider").value, api_key: "" })
      .then(function (result) {
        if (!result || !result.ok) {
          setHint((result && result.error) || "清除失败", false);
          return false;
        }
        setHint(result.keyConfigured
          ? "已清除文件里的密钥；当前仍可用环境变量凭据（" + sourceLabel(result.keySource) + "）。"
          : "已清除文件里的密钥，当前没有可用凭据。", true);
        if (global.OwoApi && typeof global.OwoApi.resetCoreConnection === "function") {
          global.OwoApi.resetCoreConnection();
        }
        return refresh().then(function () { return true; });
      })
      .catch(function (error) {
        setHint("清除失败：" + String((error && error.message) || error), false);
        return false;
      })
      .finally(function () {
        if (button) button.disabled = false;
      });
  }

  /// 在资源管理器里定位配置文件（用户要手改时少找半天路径）。
  function revealConfigFile() {
    if (!invokeOwner()) {
      var path = $("modelConfigPath") ? $("modelConfigPath").textContent.trim() : "";
      setHint("请手动打开：" + (path || "config.json"), false);
      return;
    }
    invoke("reveal_model_config").then(function (result) {
      if (result && result.ok) {
        setHint(result.created === false
          ? (result.detail || "已打开配置目录")
          : "已在资源管理器中定位配置文件。", true);
      } else {
        setHint((result && result.error) || "无法打开配置文件位置", false);
      }
    }).catch(function (error) {
      setHint("无法打开配置文件位置：" + String((error && error.message) || error), false);
    });
  }

  /// R13：从磁盘重新读取 config.json 并生效（手改文件后点这个，不必重开应用）。
  function reloadFromFile() {
    var button = $("modelReloadBtn");
    if (button) button.disabled = true;
    setHint("正在从 config.json 重新加载…", true);
    return invoke("reload_model_config").then(function (result) {
      if (!result || !result.ok) {
        setHint((result && result.error) || "重载失败", false);
        return false;
      }
      setHint("已按文件重载：" + (result.baseUrl || "内置端点") + " · " + (result.model || "默认模型") +
        (result.contextWindow ? " · 上下文 " + result.contextWindow : "") +
        "；核心已重启。", true);
      if (global.OwoApi && typeof global.OwoApi.resetCoreConnection === "function") {
        global.OwoApi.resetCoreConnection();
      }
      if (typeof global.owoRecoverService === "function") global.owoRecoverService();
      return refresh().then(function () { return true; });
    }).catch(function (error) {
      setHint("重载失败：" + String((error && error.message) || error), false);
      return false;
    }).finally(function () {
      if (button) button.disabled = false;
    });
  }

  function testConnection() {
    var button = $("modelTestBtn");
    var api = global.OwoApi;
    if (!api || typeof api.post !== "function") {
      setHint("测试连接暂不可用（API 客户端未就绪）", false);
      return;
    }
    if (button) button.disabled = true;
    setHint("正在测试连接…", true);
    api.post("/settings/provider-test", {}).then(function (result) {
      setHint("测试结果：" + ((result && result.code) || "unknown") +
        (result && result.endpoint ? " · " + result.endpoint : "") +
        (result && typeof result.latency_ms === "number" ? " · " + result.latency_ms + "ms" : ""),
        !!(result && result.ok));
    }).catch(function (error) {
      setHint("测试连接失败：" + String((error && error.message) || error), false);
    }).finally(function () {
      if (button) button.disabled = false;
    });
  }

  // ---- 会话级模型切换（POST /session/{id}/model）----
  // 与"默认模型"的区别：默认模型要重启核心（进程级环境变量）；会话模型只改这一条
  // 会话的 override，立即生效，适合"这条任务想换更强的模型"。
  function currentSessionId() {
    // app.js 的顶层 state 是经典脚本全局（非模块），同页可见。
    try {
      return (typeof state !== "undefined" && state && state.sessionId) || null;
    } catch (_) {
      return null;
    }
  }

  function setSessionHint(text, ok) {
    var hint = $("sessionModelHint");
    if (!hint) return;
    hint.textContent = text || "";
    hint.classList.toggle("ok", ok === true);
    hint.classList.toggle("bad", ok === false && !!text);
  }

  function refreshSessionModel() {
    var card = $("sessionModelCard");
    if (!card) return;
    var api = global.OwoApi;
    var sessionId = currentSessionId();
    if (!sessionId) {
      card.classList.add("model-card-idle");
      setSessionHint("当前没有打开的会话：新建会话后即可在这里单独切换模型。", false);
      return;
    }
    card.classList.remove("model-card-idle");
    if (!api || typeof api.get !== "function") return;
    Promise.resolve(api.get("/session/" + encodeURIComponent(sessionId))).then(function (session) {
      var override = session && (session.model_override || "");
      var effective = (session && session.model) || "";
      var input = $("sessionModelName");
      if (input) input.value = override || "";
      setSessionHint(
        override
          ? "本会话固定使用：" + override + "（默认模型仍为" + (effective || "未配置") + "）"
          : "本会话跟随默认模型：" + (effective || "未配置"),
        true
      );
    }).catch(function (error) {
      setSessionHint("读取会话模型失败：" + String((error && error.message) || error), false);
    });
  }

  function applySessionModel(model) {
    var api = global.OwoApi;
    var sessionId = currentSessionId();
    if (!sessionId) {
      setSessionHint("请先在左侧新建或选择一个会话。", false);
      return Promise.resolve(false);
    }
    if (!api || typeof api.post !== "function") {
      setSessionHint("无法切换：本地服务未连接。", false);
      return Promise.resolve(false);
    }
    var button = $("sessionModelApplyBtn");
    if (button) button.disabled = true;
    // 空字符串 = 清除 override（回到默认模型），后端 set_model_override 支持 None 语义。
    return Promise.resolve(api.post("/session/" + encodeURIComponent(sessionId) + "/model", { model: model }))
      .then(function () {
        if (model) rememberModelName(model);
        setSessionHint(model ? "已切换为：" + model : "已恢复跟随默认模型", true);
        refreshSessionModel();
        return true;
      })
      .catch(function (error) {
        setSessionHint("切换失败：" + String((error && error.message) || error), false);
        return false;
      })
      .finally(function () {
        if (button) button.disabled = false;
      });
  }

  function init() {
    if (!$("modelCard")) return;
    renderPresets();
    renderModelNameCandidates();
    var provider = $("settingsProvider");
    if (provider) provider.addEventListener("change", syncProviderFields);
    var applyButton = $("modelApplyBtn");
    if (applyButton) applyButton.addEventListener("click", apply);
    var testButton = $("modelTestBtn");
    if (testButton) testButton.addEventListener("click", testConnection);
    var reloadButton = $("modelReloadBtn");
    if (reloadButton) reloadButton.addEventListener("click", reloadFromFile);
    var clearKeyButton = $("modelClearKeyBtn");
    if (clearKeyButton) clearKeyButton.addEventListener("click", clearStoredKey);
    var revealButton = $("modelOpenConfigBtn");
    if (revealButton) revealButton.addEventListener("click", revealConfigFile);
    // 密钥可见性切换：默认 password（防肩窥与截图泄露），按需明文核对。
    var keyToggle = $("settingsApiKeyToggle");
    if (keyToggle) {
      keyToggle.addEventListener("click", function () {
        var input = $("settingsApiKey");
        if (!input) return;
        var show = input.type === "password";
        input.type = show ? "text" : "password";
        keyToggle.textContent = show ? "隐藏" : "显示";
      });
    }
    var sessionApply = $("sessionModelApplyBtn");
    if (sessionApply) {
      sessionApply.addEventListener("click", function () {
        applySessionModel($("sessionModelName").value.trim());
      });
    }
    var sessionClear = $("sessionModelClearBtn");
    if (sessionClear) {
      sessionClear.addEventListener("click", function () {
        applySessionModel("");
      });
    }
    syncProviderFields();
    refresh();
    refreshSessionModel();
  }

  global.OwoSettings = {
    init: init,
    refresh: refresh,
    apply: apply,
    refreshSessionModel: refreshSessionModel,
    reloadFromFile: reloadFromFile,
    presets: presetList,
  };
})(typeof window !== "undefined" ? window : globalThis);
