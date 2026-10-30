//! Desktop-controlled access to the existing Agent from the input method.
use crate::permission_spec::PermissionSpec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImeAccessMode {
    Disabled,
    ReadOnly,
    #[default]
    FollowAgent,
    Custom,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImeAccessSettings {
    pub mode: ImeAccessMode,
    /// None uses the daemon's workspace; the gateway cannot choose this path.
    pub workspace: Option<String>,
    /// An additional ceiling, never an alternative to the Agent policy.
    pub permission_spec: Option<PermissionSpec>,
    /// None includes all currently available tools; Some([]) includes no tools.
    pub allowed_tools: Option<Vec<String>>,
}
impl ImeAccessSettings {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(workspace) = &self.workspace {
            if workspace.trim().is_empty()
                || workspace.len() > 32768
                || workspace.chars().any(char::is_control)
                || !std::path::Path::new(workspace).is_absolute()
            {
                return Err("ime/invalid_workspace".into());
            }
        }
        if self.mode == ImeAccessMode::Custom && self.permission_spec.is_none() {
            return Err("ime/custom_permissions_required".into());
        }
        if self.allowed_tools.as_ref().is_some_and(|tools| {
            tools.len() > 512
                || tools.iter().any(|tool| {
                    tool.is_empty()
                        || tool.len() > 256
                        || !tool
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"_-.:/".contains(&c))
                })
        }) {
            return Err("ime/invalid_tools".into());
        }
        // PermissionSpec's legacy scopes are descriptive, not an enforcement boundary.
        // IME scope is the canonical workspace plus tools and enforced dimensions.
        if self.permission_spec.as_ref().is_some_and(|spec| {
            !spec.scopes.is_empty()
                || spec.filesystem == crate::permission_spec::FilesystemScope::Custom
        }) {
            return Err("ime/unsupported_scope_use_workspace_and_tools".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_settings_follow_the_existing_agent_policy() {
        let settings: crate::Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.ime_access.mode, ImeAccessMode::FollowAgent);
        assert_eq!(settings.ime_access.allowed_tools, None);
    }
    #[test]
    fn invalid_modes_and_missing_custom_permissions_are_rejected() {
        assert!(serde_json::from_str::<ImeAccessSettings>(r#"{"mode":"unrestricted"}"#).is_err());
        let settings: ImeAccessSettings = serde_json::from_str(r#"{"mode":"custom"}"#).unwrap();
        assert!(settings.validate().is_err());
        let settings: ImeAccessSettings = serde_json::from_str(r#"{"allowed_tools":[]}"#).unwrap();
        assert_eq!(settings.allowed_tools, Some(Vec::new()));
        assert!(settings.validate().is_ok());
    }
}
