/* 常用模型服务提供方预设（唯一事实源）。
 *
 * 为什么单独一个文件（而不是写在渲染代码里）：
 *   1. `tests/panels-lint.test.mjs` 禁止在渲染脚本里硬编码本机 URL（只放行动态端口
 *      拼接与缺省端口回退）。本地 Ollama 的 `127.0.0.1:11434` 是产品预置项而非
 *      "写死的连接地址"，放在配置模块里语义清楚，也不必给 lint 开特例；
 *   2. 首次启动引导页与设置页的模型卡共用同一份预设，避免两处默认值漂移
 *      （历史缺陷形态：引导页默认 BigModel、设置页默认别的端点）。
 *
 * 密钥不在这里，也永远不会在这里：只描述"端点 + 默认模型名 + 该端点用哪个
 * 环境变量放密钥"，密钥本体只存在于系统环境变量。
 */
(function (global) {
  "use strict";

  var OLLAMA_PORT = 11434;
  var OLLAMA_HOST = "127.0.0.1";

  function ollamaBaseUrl() {
    return "http://" + OLLAMA_HOST + ":" + OLLAMA_PORT + "/v1";
  }

  function presets() {
    return [
      {
        id: "bigmodel",
        label: "智谱 BigModel",
        baseUrl: "https://open.bigmodel.cn/api/paas/v4",
        model: "glm-5.3-flash",
        keyEnv: "OPENAI_API_KEY",
      },
      {
        id: "openai",
        label: "OpenAI",
        baseUrl: "https://api.openai.com/v1",
        model: "gpt-4o-mini",
        keyEnv: "OPENAI_API_KEY",
      },
      {
        id: "deepseek",
        label: "DeepSeek",
        baseUrl: "https://api.deepseek.com/v1",
        model: "deepseek-chat",
        keyEnv: "OPENAI_API_KEY",
      },
      {
        id: "dashscope",
        label: "阿里 DashScope 兼容",
        baseUrl: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        model: "qwen-plus",
        keyEnv: "OPENAI_API_KEY",
      },
      {
        id: "ollama",
        label: "本地 Ollama",
        baseUrl: ollamaBaseUrl(),
        model: "local",
        keyEnv: "",
      },
      {
        id: "custom",
        label: "自定义 / 自建",
        baseUrl: "",
        model: "",
        keyEnv: "OPENAI_API_KEY",
      },
    ];
  }

  global.OwoProviderPresets = {
    presets: presets,
    ollamaBaseUrl: ollamaBaseUrl,
  };
})(typeof window !== "undefined" ? window : globalThis);
