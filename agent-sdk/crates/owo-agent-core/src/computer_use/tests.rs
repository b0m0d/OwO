use super::drivers::*;
use super::sim::*;
use super::tools::*;
use serde_json::json;

#[test]
fn browser_tool_names_map_to_driver_commands() {
    // 宏按“browser_”前缀去映射驱动命令，这里验证命名契约。
    let names = [
        "browser_navigate",
        "browser_search",
        "browser_snapshot",
        "browser_click",
        "browser_type",
        "browser_press",
    ];
    for name in names {
        let cmd = name.trim_start_matches("browser_");
        assert!(matches!(
            cmd,
            "navigate" | "search" | "snapshot" | "click" | "type" | "press"
        ));
    }
}

#[test]
fn node_runtime_falls_back_without_panicking() {
    let (node, node_path) = node_runtime();
    assert!(!node.is_empty());
    let _ = node_path;
}

#[test]
fn find_ocr_line_matches_text_and_role_filter() {
    let ocr = json!({
        "lines": [
            { "text": "发送", "x": 0, "y": 0, "width": 10, "height": 10, "role_hint": "button" },
            { "text": "对方正在输入…", "x": 0, "y": 20, "width": 10, "height": 10, "role_hint": "status" },
            { "text": "今晚吃什么", "x": 0, "y": 40, "width": 10, "height": 10, "role_hint": "message" }
        ]
    });
    let button = find_ocr_line(&ocr, "发送", "button").expect("应匹配发送按钮");
    assert_eq!(button["role_hint"], "button");
    assert!(find_ocr_line(&ocr, "发送", "message").is_none());
    assert!(find_ocr_line(&ocr, "输入中", "").is_none());
    let message = find_ocr_line(&ocr, "吃什么", "").expect("应匹配消息行");
    assert_eq!(message["y"], 40);
}

#[test]
fn find_ocr_line_is_case_insensitive() {
    let ocr = json!({
        "lines": [{ "text": "Hello World", "x": 0, "y": 0, "width": 10, "height": 10, "role_hint": "text" }]
    });
    assert!(find_ocr_line(&ocr, "hello", "").is_some());
}

#[test]
fn sim_anchor_matches_line_text_and_role() {
    use crate::learn::SemanticAnchor;
    let input = json!({ "text": "输入消息...", "role_hint": "input" });
    let send = json!({ "text": "发送", "role_hint": "button" });
    let message = json!({ "text": "今晚吃什么", "role_hint": "message" });
    assert!(sim_anchor_matches(
        &input,
        &SemanticAnchor {
            app_id: None,
            role: Some("edit".into()),
            name: "输入消息".into(),
            parent: None,
            element_id: None,
        }
    ));
    assert!(sim_anchor_matches(
        &send,
        &SemanticAnchor {
            app_id: None,
            role: Some("button".into()),
            name: "发送".into(),
            parent: None,
            element_id: None,
        }
    ));
    assert!(!sim_anchor_matches(
        &send,
        &SemanticAnchor {
            app_id: None,
            role: Some("input".into()),
            name: "发送".into(),
            parent: None,
            element_id: None,
        }
    ));
    assert!(sim_anchor_matches(
        &message,
        &SemanticAnchor {
            app_id: None,
            role: Some("text".into()),
            name: "吃什么".into(),
            parent: None,
            element_id: None,
        }
    ));
    assert!(!sim_anchor_matches(
        &message,
        &SemanticAnchor {
            app_id: None,
            role: None,
            name: String::new(),
            parent: None,
            element_id: None,
        }
    ));
}

#[test]
fn vision_grounding_from_value_parses_box_and_flags() {
    let value = json!({
        "matched": true,
        "description": "发送按钮",
        "box": [815, 624, 170, 36],
        "confidence": 0.88,
        "cross_validated": true,
        "surface": "desktop"
    });
    let grounding = vision_grounding_from_value(&value, "发送按钮").expect("解析成功");
    assert_eq!(grounding.x, 815);
    assert_eq!(grounding.y, 624);
    assert_eq!(grounding.width, 170);
    assert_eq!(grounding.height, 36);
    assert!(grounding.cross_validated);
    assert!((grounding.confidence - 0.88).abs() < 1e-9);

    let bad = json!({ "matched": true, "box": [1, 2, 3] });
    assert!(vision_grounding_from_value(&bad, "x").is_err());

    let no_confidence = json!({ "matched": true, "box": [1, 2, 3, 4] });
    let grounding = vision_grounding_from_value(&no_confidence, "x").expect("默认置信度");
    assert!((grounding.confidence - 0.7).abs() < 1e-9);
}
