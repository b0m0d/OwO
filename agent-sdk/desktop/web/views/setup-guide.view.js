/* §4.6/§4.8 首次启动引导（NoWorkspace + 提供商选择）。
 * 当壳报告 no_workspace（尚未选择项目目录）时渲染：选择项目工作区 →
 * 选择模型提供商（云端 / 本地 Ollama / 稍后配置）→ 重启 core 进入任务页。
 * 密钥永不回到前端：只读 keyConfigured 布尔与端点/模型名。
 */
(function (global) {
  "use strict";

  function esc(value) {
    return String(value == null ? "" : value).replace(/[&<>"']/g, function (ch) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[ch];
    });
  }

  function invokeOwner() {
    const internal = global.__TAURI_INTERNALS__;
    const publicCore = global.__TAURI__ && global.__TAURI__.core;
    const owner = publicCore && typeof publicCore.invoke === "function" ? publicCore : internal;
    return owner && typeof owner.invoke === "function" ? owner : null;
  }

  function invoke(command, args) {
    const owner = invokeOwner();
    if (!owner) return Promise.reject(new Error("非桌面环境"));
    return Promise.resolve(owner.invoke.call(owner, command, args || {}));
  }

  function setBusy(form, busy) {
    if (!form) return;
    form.querySelectorAll("button, input, select").forEach(function (node) {
      node.disabled = busy;
    });
  }

  function showMessage(form, text, ok) {
    let line = form.querySelector(".setup-message");
    if (!line) {
      line = document.createElement("p");
      line.className = "setup-message";
      form.appendChild(line);
    }
    line.textContent = text || "";
    line.classList.toggle("ok", !!ok);
  }

  // 工作区选择表单：桌面版走 **Tauri 原生目录选择器**（§4.4 禁止要求手输完整
  // 路径）；只有纯浏览器 dev 模式没有原生选择器时才保留文本框可输入。
  function renderWorkspaceCard(root, diagnostics) {
    const card = document.createElement("section");
    card.className = "setup-card";
    card.innerHTML =
      "<h3>1 · 选择项目工作区</h3>" +
      "<p>选择存放项目文件的目录。没有工作区时不会启用任何文件工具；" +
      "核心程序安装目录只用于运行，不与你的项目混在一起。</p>" +
      (diagnostics && diagnostics.message
        ? '<p class="sub">' + esc(diagnostics.message) + "</p>"
        : "") +
      '<form data-role="workspace-form">' +
      '<label for="setup-workspace-path">项目目录</label>' +
      '<div class="setup-row">' +
      '<input id="setup-workspace-path" data-role="path" type="text" placeholder="例如 D:\\projects\\my-app" autocomplete="off" spellcheck="false" />' +
      '<button type="button" data-role="browse">浏览…</button>' +
      "</div>" +
      '<input data-role="dir-picker" type="file" webkitdirectory hidden />' +
      '<p class="sub" data-role="path-hint">提示：也可粘贴完整绝对路径（必须真实存在）。</p>' +
      '<button type="submit" class="primary">使用此目录</button>' +
      "</form>";
    root.appendChild(card);

    const form = card.querySelector('[data-role="workspace-form"]');
    const pathInput = card.querySelector('[data-role="path"]');
    const browseButton = card.querySelector('[data-role="browse"]');
    const pathHint = card.querySelector('[data-role="path-hint"]');
    const picker = card.querySelector('[data-role="dir-picker"]');
    const nativePicker = global.OwoFolderPicker && global.OwoFolderPicker.isNativeAvailable(global);
    if (nativePicker) {
      // 原生选择器一旦选定，壳侧已完成校验 + 持久化 + 受控重启：
      // 本卡直接判定完成，不再要求用户多点一次「使用此目录」。
      global.OwoFolderPicker.attach(pathInput, browseButton, {
        title: "选择项目工作区（原生目录选择器）",
        onPicked: function (workspace) {
          if (!form.dataset.done) {
            form.dataset.done = "1";
            global.__owoSetupContinue && global.__owoSetupContinue("workspace");
          }
          showMessage(form, "工作区已设置：" + workspace, true);
        },
        onCanceled: function () {
          showMessage(form, "已取消选择，工作区保持不变。", true);
        },
        onError: function (message) {
          showMessage(form, "选择失败：" + message, false);
        },
      });
      pathHint.textContent = "由原生目录选择器设定，不需要手输完整路径。";
    } else {
      // 浏览器 dev 模式：webkitdirectory 只能给出相对名，取不到绝对路径。
      browseButton.addEventListener("click", function () {
        picker.value = "";
        picker.click();
      });
      picker.addEventListener("change", function () {
        if (picker.files && picker.files.length > 0) {
          const first = picker.files[0];
          let dir = first.webkitRelativePath || first.name;
          const slash = dir.indexOf("/");
          if (slash > 0) dir = dir.slice(0, slash);
          // webkitdirectory 只能给出相对名，无法还原绝对路径；
          // 回退为提示用户手动粘贴（路径解析在 rust 侧 canonicalize 保证存在性）。
          showMessage(form, "请粘贴该目录的完整路径（浏览器安全限制无法读取绝对路径）", false);
        }
      });
    }

    invoke("get_workspace").then(function (state) {
      if (state && state.workspace) {
        pathInput.value = state.workspace;
        pathInput.title = state.workspace;
        // R3-B：工作区已配置过 → 本卡自动完成，用户只需处理提供商（provider
        // 未配置场景引导页直达模型配置部分，不被重复的目录确认卡住）。
        if (!form.dataset.done && typeof global.__owoSetupContinue === "function") {
          form.dataset.done = "1";
          showMessage(form, "已保存的工作区：" + state.workspace, true);
          global.__owoSetupContinue("workspace");
        }
      }
    }).catch(function () { /* 诊断不影响表单 */ });

    form.addEventListener("submit", function (event) {
      event.preventDefault();
      const path = pathInput.value.trim();
      if (!path) {
        showMessage(form, "请填写项目目录", false);
        return;
      }
      setBusy(form, true);
      showMessage(form, "正在保存并启动核心…", false);
      invoke("set_workspace", { path: path }).then(function (result) {
        if (result && result.ok) {
          form.dataset.done = "1";
          showMessage(form, "工作区已设置：" + result.workspace, true);
          if (typeof global.renderOwoSetupGuide === "function" && global.__owoSetupContinue) {
            global.__owoSetupContinue("workspace");
          }
        } else {
          showMessage(form, (result && result.error) || "设置失败", false);
        }
      }).catch(function (error) {
        showMessage(form, String((error && error.message) || error), false);
      }).finally(function () {
        setBusy(form, false);
      });
    });
  }

  function renderProviderCard(root, diagnostics) {
    // §3.4：引导页必须自带稳定码。壳在没有提供商时以 provider/not_configured 失败，
    // 该码是本页的"归因锚点"——即使用户随后手动点了「稍后配置」，这里展示的仍是
    // 当前判定依据，而不是等 get_provider_status 回来的竞态结果。
    const shellCode = diagnostics && typeof diagnostics.errorCode === "string" ? diagnostics.errorCode : "";
    const providerUnset = shellCode === "provider/not_configured";
    const card = document.createElement("section");
    card.className = "setup-card";
    card.innerHTML =
      "<h3>2 · 选择模型提供商</h3>" +
      "<p>核心就绪前先确认模型连接：选择云端、本地 Ollama 或稍后配置。" +
      "密钥始终只存在于系统环境变量，应用不会读取或保存密钥本身。</p>" +
      '<form data-role="provider-form">' +
      '<label data-role="fields-cloud" hidden>云端端点（缺省 BigModel）</label>' +
      '<input data-role="base-url" type="text" placeholder="https://open.bigmodel.cn/api/paas/v4" hidden />' +
      '<label data-role="fields-model" hidden>模型名（缺省按提供商）</label>' +
      '<input data-role="model" type="text" placeholder="glm-5.3-flash" hidden />' +
      '<div class="setup-row">' +
      '<label><input type="radio" name="provider-mode" value="cloud" /> 云端（OpenAI 兼容）</label>' +
      '<label><input type="radio" name="provider-mode" value="ollama" /> 本地 Ollama</label>' +
      '<label><input type="radio" name="provider-mode" value="unset" /> 稍后配置</label>' +
      "</div>" +
      '<p class="sub" data-role="status">读取提供商状态…</p>' +
      // §3.4「provider 未配置」契约动作：打开模型设置 / 测试连接（TCP 层探测，不发真实请求）。
      '<div class="inline">' +
      '<button type="button" data-action="open_provider_settings" data-role="open-settings">打开模型设置</button>' +
      '<button type="button" data-action="test_connection" data-role="test-connection">测试连接</button>' +
      "</div>" +
      '<button type="submit" class="primary">保存并应用</button>' +
      "</form>";
    root.appendChild(card);

    const form = card.querySelector('[data-role="provider-form"]');
    const baseUrl = card.querySelector('[data-role="base-url"]');
    const model = card.querySelector('[data-role="model"]');
    const status = card.querySelector('[data-role="status"]');

    function syncFields() {
      const mode = form.querySelector('input[name="provider-mode"]:checked');
      const show = mode && mode.value === "cloud";
      baseUrl.hidden = !show;
      model.hidden = !show;
      card.querySelector('[data-role="fields-cloud"]').hidden = !show;
      card.querySelector('[data-role="fields-model"]').hidden = !show;
    }
    form.querySelectorAll('input[name="provider-mode"]').forEach(function (radio) {
      radio.addEventListener("change", syncFields);
    });

    invoke("get_provider_status").then(function (state) {
      if (!state) return;
      const mode = state.provider || "unset";
      const radio = form.querySelector('input[name="provider-mode"][value="' + esc(mode) + '"]');
      if (radio) radio.checked = true;
      if (state.baseUrl) baseUrl.value = state.baseUrl;
      if (state.model) model.value = state.model;
      syncFields();
      let text = "当前：" + esc(mode === "cloud" ? "云端" : mode === "ollama" ? "本地 Ollama" : "未配置");
      if (state.baseUrl) text += " · " + esc(state.baseUrl);
      if (state.model) text += " · " + esc(state.model);
      if (mode === "cloud") text += state.keyConfigured ? " · 已检测到 API 密钥（不显示内容）" : " · 缺少模型凭据（需先在系统环境变量配置）";
      if (state.ready && !providerUnset) {
        text += " · 就绪";
        status.textContent = "当前：" + (mode === "cloud" ? "云端" : mode === "ollama" ? "本地 Ollama" : "未配置") +
          (state.baseUrl ? " · " + state.baseUrl : "") + (state.model ? " · " + state.model : "") +
          (mode === "cloud" ? (state.keyConfigured ? " · 已检测到 API 密钥（不显示内容）" : " · 缺少 OPENAI_API_KEY（需先在系统环境变量配置）") : "") +
          " · 就绪";
      } else {
        // §3.4 契约：未配置态必须携带稳定错误码（矩阵/单测按码断言，不匹配中文）。
        status.innerHTML = text + ' · <code>provider/not_configured</code>（模型暂不可用，选择提供商后即可开始）';
      }
    }).catch(function () {
      // 读不到状态也必须给出归因码：静默降级成"稍后在设置中调整"会让矩阵的
      // "引导页呈现稳定错误码"断言失去依据，用户也不知道下一步该做什么。
      status.innerHTML = providerUnset
        ? '模型提供商未配置 · <code>provider/not_configured</code>（选择提供商后即可开始）'
        : "无法读取提供商状态（可稍后在设置中调整）";
    });

    // §3.4 契约动作：打开模型设置（进设置页）与测试连接（核心侧 TCP 探测，稳定码回执）。
    const openSettingsBtn = card.querySelector('[data-role="open-settings"]');
    if (openSettingsBtn) {
      openSettingsBtn.addEventListener("click", function () {
        if (global.owoRouter && typeof global.owoRouter.go === "function") global.owoRouter.go("settings");
        else showMessage(form, "引导页内即可完成提供商选择（设置页暂不可达）", false);
      });
    }
    const testBtn = card.querySelector('[data-role="test-connection"]');
    if (testBtn) {
      testBtn.addEventListener("click", function () {
        const api = global.OwoApi;
        if (!api || typeof api.post !== "function") {
          showMessage(form, "测试连接暂不可用（API 客户端未就绪）", false);
          return;
        }
        testBtn.disabled = true;
        api.post("/settings/provider-test", {}).then(function (result) {
          showMessage(form, "测试结果：" + ((result && result.code) || "unknown") +
            (result && result.endpoint ? " · " + result.endpoint : "") +
            (result && typeof result.latency_ms === "number" ? " · " + result.latency_ms + "ms" : ""),
            !!(result && result.ok));
        }).catch(function (error) {
          showMessage(form, "测试连接失败：" + String((error && error.message) || error), false);
        }).finally(function () {
          testBtn.disabled = false;
        });
      });
    }

    form.addEventListener("submit", function (event) {
      event.preventDefault();
      const modeNode = form.querySelector('input[name="provider-mode"]:checked');
      const mode = modeNode ? modeNode.value : "unset";
      setBusy(form, true);
      showMessage(form, "正在保存并重启核心…", false);
      invoke("set_provider", {
        mode: mode,
        base_url: mode === "cloud" ? baseUrl.value.trim() : "",
        model: mode === "cloud" ? model.value.trim() : "",
      }).then(function (result) {
        if (result && result.ok) {
          form.dataset.done = "1";
          showMessage(form, "提供商已保存：" + result.provider + " · " + (result.baseUrl || "稍后配置"), true);
          if (typeof global.renderOwoSetupGuide === "function" && global.__owoSetupContinue) {
            global.__owoSetupContinue("provider");
          }
        } else {
          showMessage(form, (result && result.error) || "保存失败", false);
        }
      }).catch(function (error) {
        showMessage(form, String((error && error.message) || error), false);
      }).finally(function () {
        setBusy(form, false);
      });
    });
  }

  // 每个表单完成后回调：两者都完成 → onReady（上层恢复轮询）。
  global.renderOwoSetupGuide = function (root, diagnostics, onReady) {
    if (!root) return;
    root.innerHTML = "";
    const owner = invokeOwner();
    if (!owner) {
      root.innerHTML =
        '<div class="service-error" role="alert">' +
        "<strong>需要桌面环境</strong>" +
        "<p>首次配置请在 OwO Agent 桌面应用中完成。</p></div>";
      return;
    }
    const guide = document.createElement("div");
    guide.className = "setup-guide";
    root.appendChild(guide);
    let completed = { workspace: false, provider: false };
    global.__owoSetupContinue = function (which) {
      completed[which] = true;
      if (completed.workspace && completed.provider && typeof onReady === "function") {
        global.__owoSetupContinue = null;
        onReady();
      }
    };
    renderWorkspaceCard(guide, diagnostics || null);
    renderProviderCard(guide, diagnostics || null);
  };
})(typeof window !== "undefined" ? window : globalThis);