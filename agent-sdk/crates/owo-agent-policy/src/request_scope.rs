//! Caller-specific restrictions layered over the live host policy.
use crate::{
    permission_spec::PermissionSpec,
    permissions::{Level, PermissionRequest},
};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
pub struct RequestPermissionScope {
    read_only: bool,
    spec: Option<PermissionSpec>,
    tools: Option<Vec<String>>,
    revoked: AtomicBool,
}
impl RequestPermissionScope {
    pub fn new(read_only: bool, spec: Option<PermissionSpec>, tools: Option<Vec<String>>) -> Self {
        Self {
            read_only,
            spec,
            tools,
            revoked: AtomicBool::new(false),
        }
    }
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }
    pub fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }
    pub fn includes_tool(&self, name: &str) -> bool {
        !self.is_revoked()
            && self
                .tools
                .as_ref()
                .is_none_or(|tools| tools.iter().any(|tool| tool == name))
    }
    /// Trusted lifecycle commands cannot be sandboxed to a narrower per-request tool or file scope.
    pub fn allows_lifecycle_hooks(&self) -> bool {
        !self.is_revoked() && !self.read_only && self.spec.is_none() && self.tools.is_none()
    }
    pub fn requires_approval(&self, request: &PermissionRequest) -> bool {
        use crate::{permission_spec::RuleScope, tool_effects::ToolDimension};
        let Some(spec) = &self.spec else {
            return false;
        };
        crate::tool_effects::capability_dimensions(&request.tool)
            .iter()
            .any(|dimension| match dimension {
                ToolDimension::Command => spec.command == RuleScope::Allowlisted,
                ToolDimension::Network => spec.network == RuleScope::Allowlisted,
                _ => false,
            })
    }
    pub fn denial(&self, request: &PermissionRequest) -> Option<String> {
        if self.is_revoked() {
            return Some("IME access changed or was revoked".into());
        }
        if self.read_only && request.level != Level::Read {
            return Some("IME access is read-only".into());
        }
        if self
            .tools
            .as_ref()
            .is_some_and(|tools| !tools.contains(&request.tool))
        {
            return Some(format!("IME access does not include tool {}", request.tool));
        }
        self.spec
            .as_ref()
            .and_then(|spec| spec.extra_denial(request))
            .map(|_| "IME permission dimensions prohibit this operation".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_spec::{FilesystemScope, PersistenceScope, RuleScope};
    use crate::permissions::{Decision, PermissionProfile, Policy};
    use serde_json::json;
    use std::sync::Arc;
    fn request(tool: &str, level: Level) -> PermissionRequest {
        PermissionRequest::new(
            tool,
            json!({"path":"note.txt","command":"echo fixture"}),
            level,
            "fixture",
        )
    }
    #[test]
    fn restrictions_are_inherited_and_revoked_without_changing_the_host() {
        let host = Policy::new(".");
        host.set_profile(PermissionProfile::FullAccess);
        let scope = Arc::new(RequestPermissionScope::new(
            false,
            None,
            Some(vec!["read_file".into()]),
        ));
        let child = host
            .for_workspace(".")
            .unwrap()
            .with_request_scope(Arc::clone(&scope));
        assert_eq!(
            host.decision(&request("write_file", Level::Write)),
            Decision::Allow
        );
        assert_eq!(
            child.decision(&request("write_file", Level::Write)),
            Decision::Deny
        );
        let nested = child.for_workspace(".").unwrap();
        assert_eq!(
            nested.decision(&request("read_file", Level::Read)),
            Decision::Allow
        );
        scope.revoke();
        assert!(nested
            .execution_denial(&request("read_file", Level::Read))
            .is_some());
        assert_eq!(
            host.decision(&request("read_file", Level::Read)),
            Decision::Allow
        );
    }
    #[test]
    fn a_request_scope_cannot_relax_the_host() {
        let host = Policy::read_only(".");
        let child = host
            .for_workspace(".")
            .unwrap()
            .with_request_scope(Arc::new(RequestPermissionScope::new(false, None, None)));
        assert_eq!(
            child.decision(&request("write_file", Level::Write)),
            Decision::Deny
        );
    }
    #[test]
    fn custom_dimensions_and_explicit_empty_tools_are_enforced() {
        let spec = PermissionSpec {
            filesystem: FilesystemScope::WorkspaceRead,
            command: RuleScope::Deny,
            network: RuleScope::Deny,
            persistence: PersistenceScope::Once,
            scopes: Vec::new(),
        };
        let scope = RequestPermissionScope::new(false, Some(spec), None);
        assert!(scope.denial(&request("write_file", Level::Write)).is_some());
        assert!(scope
            .denial(&request("run_command", Level::Execute))
            .is_some());
        let no_tools = RequestPermissionScope::new(false, None, Some(Vec::new()));
        assert!(no_tools
            .denial(&request("read_file", Level::Read))
            .is_some());
    }
    #[test]
    fn a_custom_approval_ceiling_survives_a_broad_host_profile() {
        let host = Policy::new(".");
        host.set_profile(PermissionProfile::FullAccess);
        let spec = PermissionSpec {
            filesystem: FilesystemScope::Unrestricted,
            command: RuleScope::Allowlisted,
            network: RuleScope::Unrestricted,
            persistence: PersistenceScope::Once,
            scopes: Vec::new(),
        };
        let scoped = host.with_request_scope(Arc::new(RequestPermissionScope::new(
            false,
            Some(spec),
            None,
        )));
        assert_eq!(
            scoped.decision(&request("run_command", Level::Execute)),
            Decision::Ask
        );
    }
}
