//! 感知内核（M14，ADR-002）。
//!
//! 本 crate 是指南 §3 里"Perception Worker"的**代码边界**（进程边界待指南 §9 A2/A3
//! 之后再做，见 ADR-002 §3.2）：把"看"和"听"从权威 Agent 运行时里切出来。
//!
//! | 模块 | 行数 | 内容 |
//! |---|---:|---|
//! | [`onnx_ocr`] | 1,217 | ONNX Runtime 文字检测 + 识别（DB 后处理、CTC 解码、字典加载） |
//! | [`scene`] | 800 | 场景图：UIA / OCR / 视觉三源融合、稳定 id、废弃淘汰 |
//! | [`perception`] | 704 | 感知层（L0–L3）与情境快照、屏障与内容引用 |
//! | [`vision`] | 662 | 视觉通道：截图→VLM 描述/定位/校验（可选云端） |
//! | [`locate`] | 531 | 元素定位：稳定 id / 名称 / 上下文矩形 / 历史与模板先验 |
//! | [`element_registry`] | 527 | 元素注册表：跨帧稳定 id、漂移容忍 |
//! | [`paddle_ocr`] | 389 | 云端 PaddleOCR 通道（可选） |
//! | [`ocr`] | 372 | OCR 抽象与行分组（本地 ONNX / 云端二选一） |
//! | [`window_template`] | 296 | 窗口模板 ROI 命中率与检测 |
//! | [`stt`] | 214 | 本地语音识别（Sherpa-ONNX）——**可选 feature** |
//! | [`accessibility`] | 158 | UIA 无障碍树读取（`foreground_ui_tree` / `ui_tree_for_hwnd`） |
//!
//! 边界（`docs/adr/ADR-002-perception-kernel.md`）：
//!
//! * 群组内所有 `crate::` 引用都落在群组内部，**零倒置**；
//! * 唯一逃逸边 `stt → settings::SttSettings` 按「配置类型随域走」把类型一起搬进来
//!   （core 的 `settings.rs` 用 `pub use` 转出，`owo_agent_core::settings::SttSettings`
//!   与 `Settings.stt` 字段类型均不变）；
//! * 对 `platform::*` 的引用改为绝对路径 [`owo_agent_kernel::platform`]（M0 已下沉）；
//! * **普通依赖里不得出现 `owo-agent-core`**。
//!
//! 为什么要切这一刀：迁移前 core 直接依赖 ORT/Sherpa/ndarray/windows，导致 core 的
//! 每个测试二进制都要静态链接它们（实测单目标链接 8–16 s、73 个 exe 共 2.68 GB）。
//! 切出去之后，改 core 不再触发 ONNX 侧的编译与链接。

pub mod accessibility;
pub mod element_registry;
pub mod locate;
pub mod ocr;
pub mod onnx_ocr;
pub mod paddle_ocr;
pub mod perception;
pub mod scene;
pub mod stt_settings;
pub mod vision;
pub mod window_template;

// `stt` 是可选能力：群组内没有模块引用它（只有 server 经 core 的 re-export 使用），
// 因此可以用一个 feature 把它整块关掉，从而完全不解析 sherpa-onnx。
#[cfg(feature = "stt")]
pub mod stt;

// 迁移期约定（与 M0–M13 一致）：用 glob 再导出，让公共面等价性由编译器证明。
pub use accessibility::*;
pub use element_registry::*;
pub use locate::*;
pub use ocr::*;
pub use onnx_ocr::*;
pub use paddle_ocr::*;
pub use perception::*;
pub use scene::*;
pub use stt_settings::*;
pub use vision::*;
pub use window_template::*;

#[cfg(feature = "stt")]
pub use stt::*;
