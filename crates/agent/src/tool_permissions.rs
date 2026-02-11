pub use agent_settings::{
    ToolPermissionDecision, decide_permission, decide_permission_from_settings,
};

#[cfg(test)]
mod tests {
    use super::*;
    use agent_settings::{CompiledRegex, InvalidRegexPattern, ToolPermissions, ToolRules};
    use settings::ToolPermissionMode;
    use std::sync::Arc;

    struct PermTest {
        tool: &'static str,
        input: &'static str,
        mode: ToolPermissionMode,
        allow: Vec<&'static str>,
        deny: Vec<&'static str>,
        confirm: Vec<&'static str>,
        global: bool,
    }

    impl PermTest {
        fn new(input: &'static str) -> Self {
            Self {
                tool: "terminal",
                input,
                mode: ToolPermissionMode::Confirm,
                allow: vec![],
                deny: vec![],
                confirm: vec![],
                global: false,
            }
        }

        fn tool(mut self, t: &'static str) -> Self {
            self.tool = t;
            self
        }
        fn mode(mut self, m: ToolPermissionMode) -> Self {
            self.mode = m;
            self
        }
        fn allow(mut self, p: &[&'static str]) -> Self {
            self.allow = p.to_vec();
            self
        }
        fn deny(mut self, p: &[&'static str]) -> Self {
            self.deny = p.to_vec();
            self
        }
        fn confirm(mut self, p: &[&'static str]) -> Self {
            self.confirm = p.to_vec();
            self
        }
        fn global(mut self, g: bool) -> Self {
            self.global = g;
            self
        }

        fn is_allow(self) {
            assert_eq!(
                self.run(),
                ToolPermissionDecision::Allow,
                "expected Allow for '{}'",
                self.input
            );
        }
        fn is_deny(self) {
            assert!(
                matches!(self.run(), ToolPermissionDecision::Deny(_)),
                "expected Deny for '{}'",
                self.input
            );
        }
        fn is_confirm(self) {
            assert_eq!(
                self.run(),
                ToolPermissionDecision::Confirm,
                "expected Confirm for '{}'",
                self.input
            );
        }

        fn run(&self) -> ToolPermissionDecision {
            let mut tools = collections::HashMap::default();
            tools.insert(
                Arc::from(self.tool),
                ToolRules {
                    default_mode: self.mode,
                    always_allow: self
                        .allow
                        .iter()
                        .filter_map(|p| CompiledRegex::new(p, false))
                        .collect(),
                    always_deny: self
                        .deny
                        .iter()
                        .filter_map(|p| CompiledRegex::new(p, false))
                        .collect(),
                    always_confirm: self
                        .confirm
                        .iter()
                        .filter_map(|p| CompiledRegex::new(p, false))
                        .collect(),
                    invalid_patterns: vec![],
                },
            );
            decide_permission(
                self.tool,
                self.input,
                &ToolPermissions { tools },
                self.global,
            )
        }
    }

    fn t(input: &'static str) -> PermTest {
        PermTest::new(input)
    }

    fn no_rules(input: &str, global: bool) -> ToolPermissionDecision {
        decide_permission(
            "terminal",
            input,
            &ToolPermissions {
                tools: collections::HashMap::default(),
            },
            global,
        )
    }

    // allow pattern matches
    #[test]
    fn allow_exact_match() {
        t("cargo test").allow(&["^cargo\\s"]).is_allow();
    }
    #[test]
    fn allow_with_args() {
        t("cargo build --release").allow(&["^cargo\\s"]).is_allow();
    }
    #[test]
    fn allow_one_of_many() {
        t("npm install").allow(&["^cargo\\s", "^npm\\s"]).is_allow();
    }
    #[test]
    fn allow_middle_pattern() {
        t("run cargo now").allow(&["cargo"]).is_allow();
    }
    #[test]
    fn allow_anchor_prevents_middle() {
        t("run cargo now").allow(&["^cargo"]).is_confirm();
    }

    // allow pattern doesn't match -> falls through
    #[test]
    fn allow_no_match_confirms() {
        t("python x.py").allow(&["^cargo\\s"]).is_confirm();
    }
    #[test]
    fn allow_no_match_global_allows() {
        t("python x.py")
            .allow(&["^cargo\\s"])
            .global(true)
            .is_allow();
    }

    // deny pattern matches
    #[test]
    fn deny_blocks() {
        t("rm -rf /").deny(&["rm\\s+-rf"]).is_deny();
    }
    #[test]
    fn deny_blocks_with_global() {
        t("rm -rf /").deny(&["rm\\s+-rf"]).global(true).is_deny();
    }
    #[test]
    fn deny_blocks_with_mode_allow() {
        t("rm -rf /")
            .deny(&["rm\\s+-rf"])
            .mode(ToolPermissionMode::Allow)
            .is_deny();
    }
    #[test]
    fn deny_middle_match() {
        t("echo rm -rf x").deny(&["rm\\s+-rf"]).is_deny();
    }
    #[test]
    fn deny_no_match_allows() {
        t("ls -la").deny(&["rm\\s+-rf"]).global(true).is_allow();
    }

    // confirm pattern matches
    #[test]
    fn confirm_requires_confirm() {
        t("sudo apt install").confirm(&["sudo\\s"]).is_confirm();
    }
    #[test]
    fn global_overrides_confirm() {
        t("sudo reboot")
            .confirm(&["sudo\\s"])
            .global(true)
            .is_allow();
    }
    #[test]
    fn confirm_overrides_mode_allow() {
        t("sudo x")
            .confirm(&["sudo"])
            .mode(ToolPermissionMode::Allow)
            .is_confirm();
    }

    // confirm beats allow
    #[test]
    fn confirm_beats_allow() {
        t("git push --force")
            .allow(&["^git\\s"])
            .confirm(&["--force"])
            .is_confirm();
    }
    #[test]
    fn confirm_beats_allow_overlap() {
        t("deploy prod")
            .allow(&["deploy"])
            .confirm(&["prod"])
            .is_confirm();
    }
    #[test]
    fn allow_when_confirm_no_match() {
        t("git status")
            .allow(&["^git\\s"])
            .confirm(&["--force"])
            .is_allow();
    }

    // deny beats allow
    #[test]
    fn deny_beats_allow() {
        t("rm -rf /tmp/x")
            .allow(&["/tmp/"])
            .deny(&["rm\\s+-rf"])
            .is_deny();
    }
    #[test]
    fn deny_beats_allow_diff() {
        t("bad deploy").allow(&["deploy"]).deny(&["bad"]).is_deny();
    }

    // deny beats confirm
    #[test]
    fn deny_beats_confirm() {
        t("sudo rm -rf /")
            .confirm(&["sudo"])
            .deny(&["rm\\s+-rf"])
            .is_deny();
    }

    // deny beats everything
    #[test]
    fn deny_beats_all() {
        t("bad cmd")
            .allow(&["cmd"])
            .confirm(&["cmd"])
            .deny(&["bad"])
            .is_deny();
    }

    // no patterns -> default_mode
    #[test]
    fn default_confirm() {
        t("python x.py")
            .mode(ToolPermissionMode::Confirm)
            .is_confirm();
    }
    #[test]
    fn default_allow() {
        t("python x.py").mode(ToolPermissionMode::Allow).is_allow();
    }
    #[test]
    fn default_deny() {
        t("python x.py").mode(ToolPermissionMode::Deny).is_deny();
    }
    #[test]
    fn default_deny_global_true() {
        t("python x.py")
            .mode(ToolPermissionMode::Deny)
            .global(true)
            .is_allow();
    }

    // default_mode confirm + global
    #[test]
    fn default_confirm_global_false() {
        t("x")
            .mode(ToolPermissionMode::Confirm)
            .global(false)
            .is_confirm();
    }
    #[test]
    fn default_confirm_global_true() {
        t("x")
            .mode(ToolPermissionMode::Confirm)
            .global(true)
            .is_allow();
    }

    // no rules at all -> global setting
    #[test]
    fn no_rules_global_false() {
        assert_eq!(no_rules("x", false), ToolPermissionDecision::Confirm);
    }
    #[test]
    fn no_rules_global_true() {
        assert_eq!(no_rules("x", true), ToolPermissionDecision::Allow);
    }

    // empty input
    #[test]
    fn empty_input_no_match() {
        t("").deny(&["rm"]).is_confirm();
    }
    #[test]
    fn empty_input_global() {
        t("").deny(&["rm"]).global(true).is_allow();
    }

    // multiple patterns - any match
    #[test]
    fn multi_deny_first() {
        t("rm x").deny(&["rm", "del", "drop"]).is_deny();
    }
    #[test]
    fn multi_deny_last() {
        t("drop x").deny(&["rm", "del", "drop"]).is_deny();
    }
    #[test]
    fn multi_allow_first() {
        t("cargo x").allow(&["^cargo", "^npm", "^git"]).is_allow();
    }
    #[test]
    fn multi_allow_last() {
        t("git x").allow(&["^cargo", "^npm", "^git"]).is_allow();
    }
    #[test]
    fn multi_none_match() {
        t("python x")
            .allow(&["^cargo", "^npm"])
            .deny(&["rm"])
            .is_confirm();
    }

    // tool isolation
    #[test]
    fn other_tool_not_affected() {
        let mut tools = collections::HashMap::default();
        tools.insert(
            Arc::from("terminal"),
            ToolRules {
                default_mode: ToolPermissionMode::Deny,
                always_allow: vec![],
                always_deny: vec![],
                always_confirm: vec![],
                invalid_patterns: vec![],
            },
        );
        tools.insert(
            Arc::from("edit_file"),
            ToolRules {
                default_mode: ToolPermissionMode::Allow,
                always_allow: vec![],
                always_deny: vec![],
                always_confirm: vec![],
                invalid_patterns: vec![],
            },
        );
        let p = ToolPermissions { tools };
        // With always_allow_tool_actions=true, even default_mode: Deny is overridden
        assert_eq!(
            decide_permission("terminal", "x", &p, true),
            ToolPermissionDecision::Allow
        );
        // With always_allow_tool_actions=false, default_mode: Deny is respected
        assert!(matches!(
            decide_permission("terminal", "x", &p, false),
            ToolPermissionDecision::Deny(_)
        ));
        assert_eq!(
            decide_permission("edit_file", "x", &p, false),
            ToolPermissionDecision::Allow
        );
    }

    #[test]
    fn partial_tool_name_no_match() {
        let mut tools = collections::HashMap::default();
        tools.insert(
            Arc::from("term"),
            ToolRules {
                default_mode: ToolPermissionMode::Deny,
                always_allow: vec![],
                always_deny: vec![],
                always_confirm: vec![],
                invalid_patterns: vec![],
            },
        );
        let p = ToolPermissions { tools };
        assert_eq!(
            decide_permission("terminal", "x", &p, true),
            ToolPermissionDecision::Allow
        );
    }

    // invalid patterns block the tool
    #[test]
    fn invalid_pattern_blocks() {
        let mut tools = collections::HashMap::default();
        tools.insert(
            Arc::from("terminal"),
            ToolRules {
                default_mode: ToolPermissionMode::Allow,
                always_allow: vec![CompiledRegex::new("echo", false).unwrap()],
                always_deny: vec![],
                always_confirm: vec![],
                invalid_patterns: vec![InvalidRegexPattern {
                    pattern: "[bad".into(),
                    rule_type: "always_deny".into(),
                    error: "err".into(),
                }],
            },
        );
        let p = ToolPermissions { tools };
        assert!(matches!(
            decide_permission("terminal", "echo hi", &p, true),
            ToolPermissionDecision::Deny(_)
        ));
    }

    // user scenario: only echo allowed, git should confirm
    #[test]
    fn user_scenario_only_echo() {
        t("echo hello").allow(&["^echo\\s"]).is_allow();
    }
    #[test]
    fn user_scenario_git_confirms() {
        t("git status").allow(&["^echo\\s"]).is_confirm();
    }
    #[test]
    fn user_scenario_rm_confirms() {
        t("rm -rf /").allow(&["^echo\\s"]).is_confirm();
    }

    // mcp tools
    #[test]
    fn mcp_allow() {
        t("")
            .tool("mcp:fs:read")
            .mode(ToolPermissionMode::Allow)
            .is_allow();
    }
    #[test]
    fn mcp_deny() {
        t("")
            .tool("mcp:bad:del")
            .mode(ToolPermissionMode::Deny)
            .is_deny();
    }
    #[test]
    fn mcp_confirm() {
        t("")
            .tool("mcp:gh:issue")
            .mode(ToolPermissionMode::Confirm)
            .is_confirm();
    }
    #[test]
    fn mcp_confirm_global() {
        t("")
            .tool("mcp:gh:issue")
            .mode(ToolPermissionMode::Confirm)
            .global(true)
            .is_allow();
    }

    // mcp vs builtin isolation
    #[test]
    fn mcp_doesnt_collide_with_builtin() {
        let mut tools = collections::HashMap::default();
        tools.insert(
            Arc::from("terminal"),
            ToolRules {
                default_mode: ToolPermissionMode::Deny,
                always_allow: vec![],
                always_deny: vec![],
                always_confirm: vec![],
                invalid_patterns: vec![],
            },
        );
        tools.insert(
            Arc::from("mcp:srv:terminal"),
            ToolRules {
                default_mode: ToolPermissionMode::Allow,
                always_allow: vec![],
                always_deny: vec![],
                always_confirm: vec![],
                invalid_patterns: vec![],
            },
        );
        let p = ToolPermissions { tools };
        assert!(matches!(
            decide_permission("terminal", "x", &p, false),
            ToolPermissionDecision::Deny(_)
        ));
        assert_eq!(
            decide_permission("mcp:srv:terminal", "x", &p, false),
            ToolPermissionDecision::Allow
        );
    }
}
