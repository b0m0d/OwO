use super::*;
use crate::error::AgentError;
use crate::gateway::ChatMessage;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

#[test]
fn session_store_round_trip_and_list() {
    let root =
        std::env::temp_dir().join(format!("owo-session-store-test-{}", uuid::Uuid::new_v4()));
    let store = JsonSessionStore::new(&root);
    let session = store
        .create(std::path::Path::new("."), "mock", None)
        .unwrap();
    store.save(&session).unwrap();
    assert_eq!(store.list().len(), 1);
    let loaded = store.load(&session.id).unwrap();
    assert_eq!(loaded.id, session.id);
    let _ = std::fs::remove_dir_all(&root);
}

/// M4.2：`model_override` JSON 存储往返 + 旧文件（缺该字段）兼容 + fork 继承。
#[test]
fn model_override_roundtrip_legacy_compat_and_fork_inherits() {
    let root = std::env::temp_dir().join(format!(
        "owo-session-override-test-{}",
        uuid::Uuid::new_v4()
    ));
    let store = JsonSessionStore::new(&root);
    let session = store
        .create(std::path::Path::new("."), "glm-5.3-flash", None)
        .unwrap()
        .with_model_override(Some("vision-pro".to_string()));
    store.save(&session).unwrap();
    let loaded = store.load(&session.id).unwrap();
    assert_eq!(loaded.model_override.as_deref(), Some("vision-pro"));
    // 展示模型与路由覆盖解耦：覆盖不改展示值。
    assert_eq!(loaded.model, "glm-5.3-flash");
    // 空串覆盖 = 清除（回退 Provider 解析链）；"default" 哨兵同样归一为自动，
    // 且绝不作为展示/存储值（M4.2 哨兵不泄漏）。
    let cleared = loaded.clone().with_model_override(Some("  ".to_string()));
    assert_eq!(cleared.model_override, None);
    let sentinel = loaded
        .clone()
        .with_model_override(Some("default".to_string()));
    assert_eq!(sentinel.model_override, None);
    let mut pinned = loaded.clone();
    pinned.set_model_override(Some("wire-y".to_string()));
    assert_eq!(pinned.model_override.as_deref(), Some("wire-y"));
    assert_eq!(pinned.model, "wire-y", "固定时展示同步");
    pinned.set_model_override(Some("default".to_string()));
    assert_eq!(pinned.model_override, None, "哨兵必须清除覆盖");
    assert_eq!(pinned.model, "wire-y", "清除不改展示（不猜测缺省值）");
    // fork 继承覆盖（路由语义随历史派生）。
    assert_eq!(loaded.fork(0).model_override.as_deref(), Some("vision-pro"));
    // 旧格式文件（无 model_override 字段）必须照常加载为 None。
    let legacy = store
        .create(std::path::Path::new("."), "old-model", None)
        .unwrap();
    let mut raw: serde_json::Value = serde_json::to_value(&legacy).expect("会话应可序列化");
    raw.as_object_mut()
        .expect("会话是对象")
        .remove("model_override");
    std::fs::write(store.plain_path(&legacy.id), raw.to_string()).unwrap();
    assert_eq!(store.load(&legacy.id).unwrap().model_override, None);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn fork_creates_child_with_history() {
    let mut session = Session::new(".", "mock", None);
    session.push(ChatMessage::user("a".to_string()));
    session.push(ChatMessage::assistant_text("b".to_string()));
    session.push(ChatMessage::user("c".to_string()));

    let child = session.fork(1);
    assert_eq!(child.messages.len(), 2);
    assert_eq!(child.parent_id.as_deref(), Some(session.id.as_str()));
    assert_eq!(child.fork_point, Some(1));
    assert!(child.snapshots.is_empty());
    assert!(child.redo_stack.is_empty());
}

#[test]
fn fork_on_empty_session_does_not_panic() {
    let session = Session::new(".", "mock", None);
    let child = session.fork(999999);
    assert!(child.messages.is_empty());
    assert_eq!(child.parent_id.as_deref(), Some(session.id.as_str()));
}

#[test]
fn rewind_and_redo_round_trip() {
    let mut session = Session::new(".", "mock", None);
    for index in 0..5 {
        session.push(ChatMessage::user(format!("m{index}")));
    }
    let removed = session.rewind(2);
    assert_eq!(removed.len(), 3);
    assert_eq!(session.messages.len(), 2);

    let restored = session.redo().expect("存在可恢复历史");
    assert_eq!(restored.len(), 3);
    assert_eq!(session.messages.len(), 5);
    assert!(session.redo().is_none());
}

#[tokio::test]
async fn rewind_and_revert_restores_files_before_truncating_history() {
    let workspace =
        std::env::temp_dir().join(format!("owo-session-rewind-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&workspace).unwrap();
    let path = workspace.join("changed.txt");
    std::fs::write(&path, "after").unwrap();

    let mut session = Session::new(&workspace, "mock", None);
    session.push(ChatMessage::user("first".to_string()));
    session.push(ChatMessage::assistant_text("reply".to_string()));
    session.snapshots.insert(
        path.to_string_lossy().replace('\\', "/"),
        SnapshotEntry {
            original_b64: Some(BASE64.encode("before")),
            expected_after_sha256: Some(crate::CasStore::hash_of(b"after")),
        },
    );

    session.revert().await.unwrap();
    let removed = session.rewind(1);

    assert_eq!(removed.len(), 1);
    assert_eq!(session.messages.len(), 1);
    assert!(session.snapshots.is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "before");
    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn execution_receipt_revert_is_scoped_and_persistable() {
    let workspace = std::env::temp_dir().join(format!(
        "owo-session-execution-receipt-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace).unwrap();
    let path = workspace.join("receipt.txt");
    std::fs::write(&path, "before").unwrap();

    let mut session = Session::new(&workspace, "mock", None);
    session.snapshots.insert(
        path.to_string_lossy().replace('\\', "/"),
        SnapshotEntry {
            original_b64: Some(BASE64.encode("before")),
            expected_after_sha256: Some(crate::CasStore::hash_of(b"after")),
        },
    );
    std::fs::write(&path, "after").unwrap();
    let receipt = session
        .record_file_execution("write_file", "turn-1", &path)
        .unwrap()
        .expect("内容变化必须产生执行收据");
    assert_eq!(receipt.changed_files, vec!["receipt.txt"]);
    assert_eq!(session.execution_receipts.len(), 1);
    let serialized = serde_json::to_value(&session).unwrap();
    assert!(serialized.get("execution_receipts").is_some());

    let restored = session
        .revert_receipt(Some(&receipt.receipt_id))
        .await
        .unwrap();
    assert_eq!(restored, vec!["receipt.txt"]);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "before");
    assert_eq!(session.execution_receipts[0].status, "reverted");
    assert!(session.snapshots.is_empty());
    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn revert_conflict_preflight_prevents_partial_overwrite() {
    let workspace = std::env::temp_dir().join(format!(
        "owo-session-revert-conflict-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace).unwrap();
    let user_changed = workspace.join("a-user-edited.txt");
    let agent_written = workspace.join("b-agent-written.txt");
    std::fs::write(&user_changed, "user edit").unwrap();
    std::fs::write(&agent_written, "agent version").unwrap();

    let mut session = Session::new(&workspace, "mock", None);
    session.snapshots.insert(
        user_changed.to_string_lossy().replace('\\', "/"),
        SnapshotEntry {
            original_b64: Some(BASE64.encode("before a")),
            expected_after_sha256: Some(crate::CasStore::hash_of(b"agent version")),
        },
    );
    session.snapshots.insert(
        agent_written.to_string_lossy().replace('\\', "/"),
        SnapshotEntry {
            original_b64: Some(BASE64.encode("before b")),
            expected_after_sha256: Some(crate::CasStore::hash_of(b"agent version")),
        },
    );

    let error = session
        .revert()
        .await
        .expect_err("外部修改必须阻止整批撤销");
    match error {
        AgentError::RevertConflict { paths } => {
            assert_eq!(paths, vec!["a-user-edited.txt".to_string()])
        }
        other => panic!("应返回结构化撤销冲突，实际：{other}"),
    }
    assert_eq!(std::fs::read_to_string(&user_changed).unwrap(), "user edit");
    assert_eq!(
        std::fs::read_to_string(&agent_written).unwrap(),
        "agent version",
        "预检发现任意冲突时，不得先回滚其他文件"
    );
    assert_eq!(session.snapshots.len(), 2, "冲突时保留快照以便用户处理");
    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn legacy_snapshot_without_write_hash_fails_closed() {
    let workspace = std::env::temp_dir().join(format!(
        "owo-session-revert-legacy-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&workspace).unwrap();
    let path = workspace.join("legacy.txt");
    std::fs::write(&path, "possibly user-edited").unwrap();
    let mut session = Session::new(&workspace, "mock", None);
    session.snapshots.insert(
        path.to_string_lossy().replace('\\', "/"),
        SnapshotEntry {
            original_b64: Some(BASE64.encode("before")),
            expected_after_sha256: None,
        },
    );

    assert!(matches!(
        session.revert().await,
        Err(AgentError::RevertConflict { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "possibly user-edited"
    );
    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn rewind_does_not_change_files_when_keep_is_current_length() {
    let workspace =
        std::env::temp_dir().join(format!("owo-session-rewind-noop-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&workspace).unwrap();
    let path = workspace.join("changed.txt");
    std::fs::write(&path, "after").unwrap();

    let mut session = Session::new(&workspace, "mock", None);
    session.push(ChatMessage::user("first".to_string()));
    session.snapshots.insert(
        path.to_string_lossy().replace('\\', "/"),
        SnapshotEntry {
            original_b64: Some(BASE64.encode("before")),
            expected_after_sha256: Some(crate::CasStore::hash_of(b"after")),
        },
    );

    let removed = session.rewind(1);

    assert!(removed.is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "after");
    assert!(!session.snapshots.is_empty());
    let _ = std::fs::remove_dir_all(&workspace);
}

#[test]
fn message_undo_and_redo_round_trip() {
    let mut session = Session::new(".", "mock", None);
    for index in 0..4 {
        session.push(ChatMessage::user(format!("m{index}")));
    }
    let removed = session.undo_message(2).expect("存在可撤销消息");
    assert_eq!(removed.len(), 2);
    assert_eq!(session.messages.len(), 2);
    assert!(session.undo_message(0).is_none());

    let restored = session.redo_message().expect("存在可恢复消息");
    assert_eq!(restored.len(), 2);
    assert_eq!(session.messages.len(), 4);
    assert!(session.redo_message().is_none());
}

#[test]
fn pushing_new_history_invalidates_both_redo_stacks() {
    let mut session = Session::new(".", "mock", None);
    for index in 0..3 {
        session.push(ChatMessage::user(format!("m{index}")));
    }
    session.rewind(1);
    session.undo_message(1);

    session.push(ChatMessage::user("new branch".to_string()));

    assert!(session.redo().is_none());
    assert!(session.redo_message().is_none());
}

#[test]
fn title_archive_pin_round_trip() {
    let mut session = Session::new(".", "mock", None);
    session.push(ChatMessage::user("给 parseConfig 补测试".to_string()));
    assert_eq!(session.display_title(), "给 parseConfig 补测试");
    session.rename("我的任务".to_string());
    assert_eq!(session.display_title(), "我的任务");
    session.set_pinned(true);
    session.set_archived(true);
    assert!(session.pinned);
    assert!(session.archived);
    let child = session.fork(0);
    assert!(child.title.is_none());
    assert!(!child.pinned);
    assert!(!child.archived);
    assert_eq!(child.display_title(), "给 parseConfig 补测试");
}
