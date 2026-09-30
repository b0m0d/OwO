use super::*;

#[test]
fn goal_status_machine_transitions() {
    let mut goal = Goal::new("g1", "测试目标");
    assert_eq!(goal.status, GoalStatus::Pending);
    goal.transition(GoalStatus::Planning);
    goal.transition(GoalStatus::Running);
    goal.transition(GoalStatus::Verifying);
    goal.transition(GoalStatus::Succeeded);
    assert!(goal.status.is_terminal());
    assert!(!GoalStatus::Running.is_terminal());
}

#[test]
fn run_state_serde_roundtrip() {
    let plan = Plan::new("p1", "g1");
    let state = GoalRunState::new(Goal::new("g1", "目标"), plan);
    let json = serde_json::to_string(&state).unwrap();
    let restored: GoalRunState = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.run_id, state.run_id);
    assert_eq!(restored.records.len(), 0);
}
