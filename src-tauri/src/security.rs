//! AI command security engine — three-tier interception
//!
//! Priority (from hardest to softest):
//! 1. Session-level blocklist regexes — always block (no confirm), no bypass
//! 2. Global blocklist regexes — always block
//! 3. Global confirm-list regexes — pop a confirm dialog (in Recommended mode)
//! 4. Otherwise — allow

use parking_lot::RwLock;
use regex::Regex;
use std::sync::Arc;

use crate::state::AIPolicyOverride;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SecurityPolicy {
    /// Every AI exec must be confirmed
    Strict,
    /// Blocklist blocks immediately; confirm-list pops a confirm; otherwise allow
    Recommended,
    /// Only session-level blocklist blocks; everything else allowed
    Permissive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    Allow,
    Block { rule: String },
    WaitConfirm { rule: String },
}

#[derive(Clone)]
pub struct SecurityEngine {
    global_blocklist: Arc<RwLock<Vec<Regex>>>,
    confirm_list: Arc<RwLock<Vec<Regex>>>,
    session_blocklist: Arc<RwLock<Vec<Regex>>>,
    /// root 执行专用黑名单（命中即拒绝）
    sudo_blocklist: Arc<RwLock<Vec<Regex>>>,
    /// root 执行专用确认清单（命中需人工批准）
    sudo_confirm_list: Arc<RwLock<Vec<Regex>>>,
    policy_override: Arc<RwLock<AIPolicyOverride>>,
}

impl SecurityEngine {
    pub fn new(
        global_blocklist: Vec<String>,
        confirm_list: Vec<String>,
        policy_override: AIPolicyOverride,
    ) -> Self {
        Self {
            global_blocklist: Arc::new(RwLock::new(compile_all(&global_blocklist))),
            confirm_list: Arc::new(RwLock::new(compile_all(&confirm_list))),
            session_blocklist: Arc::new(RwLock::new(Vec::new())),
            sudo_blocklist: Arc::new(RwLock::new(Vec::new())),
            sudo_confirm_list: Arc::new(RwLock::new(Vec::new())),
            policy_override: Arc::new(RwLock::new(policy_override)),
        }
    }

    pub fn update_global(&self, global_blocklist: &[String], confirm_list: &[String]) {
        *self.global_blocklist.write() = compile_all(global_blocklist);
        *self.confirm_list.write() = compile_all(confirm_list);
    }

    pub fn update_session(&self, policy_override: AIPolicyOverride, session_blocklist: &[String]) {
        *self.policy_override.write() = policy_override;
        *self.session_blocklist.write() = compile_all(session_blocklist);
    }

    /// 更新 root 执行专用策略（方案 B）。
    pub fn update_sudo(&self, sudo_blocklist: &[String], sudo_confirm_list: &[String]) {
        *self.sudo_blocklist.write() = compile_all(sudo_blocklist);
        *self.sudo_confirm_list.write() = compile_all(sudo_confirm_list);
    }

    /// 普通（登录用户）执行检查。
    pub fn check(&self, command: &str, global_default: SecurityPolicy) -> CheckResult {
        self.check_with_run_as(command, global_default, false)
    }

    /// 统一检查入口；`root` 为 true 时在全局黑名单之后插入 sudo 专用两层。
    pub fn check_with_run_as(
        &self,
        command: &str,
        global_default: SecurityPolicy,
        root: bool,
    ) -> CheckResult {
        // 1. Session blocklist — always block
        if let Some(m) = self
            .session_blocklist
            .read()
            .iter()
            .find(|rule| rule.is_match(command))
        {
            return CheckResult::Block {
                rule: m.as_str().into(),
            };
        }

        // 2. Global blocklist — always block
        if let Some(m) = self
            .global_blocklist
            .read()
            .iter()
            .find(|rule| rule.is_match(command))
        {
            return CheckResult::Block {
                rule: m.as_str().into(),
            };
        }

        // 3. sudo 专用黑名单 — 命中即拒绝
        if root {
            if let Some(m) = self
                .sudo_blocklist
                .read()
                .iter()
                .find(|rule| rule.is_match(command))
            {
                return CheckResult::Block {
                    rule: format!("sudo:{}", m.as_str()),
                };
            }
            // 4. sudo 专用确认清单 — 命中逐条确认
            if let Some(m) = self
                .sudo_confirm_list
                .read()
                .iter()
                .find(|rule| rule.is_match(command))
            {
                return CheckResult::WaitConfirm {
                    rule: format!("sudo:{}", m.as_str()),
                };
            }
        }

        let policy = match *self.policy_override.read() {
            AIPolicyOverride::Inherit => global_default,
            AIPolicyOverride::Strict => SecurityPolicy::Strict,
            AIPolicyOverride::Recommended => SecurityPolicy::Recommended,
            AIPolicyOverride::Permissive => SecurityPolicy::Permissive,
        };
        match policy {
            SecurityPolicy::Strict => CheckResult::WaitConfirm {
                rule: "strict policy".into(),
            },
            SecurityPolicy::Permissive => CheckResult::Allow,
            SecurityPolicy::Recommended => {
                if let Some(m) = self
                    .confirm_list
                    .read()
                    .iter()
                    .find(|rule| rule.is_match(command))
                {
                    CheckResult::WaitConfirm {
                        rule: m.as_str().into(),
                    }
                } else {
                    CheckResult::Allow
                }
            }
        }
    }
}

fn compile_all(patterns: &[String]) -> Vec<Regex> {
    patterns
        .iter()
        .filter_map(|p| match Regex::new(p) {
            Ok(re) => Some(re),
            Err(e) => {
                tracing::warn!("Skip invalid regex {p}: {e}");
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn sudo_layers_only_apply_to_root_execution() {
        let engine = SecurityEngine::new(
            Vec::new(),
            patterns(&["^confirm me$"]),
            AIPolicyOverride::Recommended,
        );
        engine.update_sudo(&patterns(&[r"\bvisudo\b"]), &patterns(&[r"\brm\b"]));
        // 普通用户执行不受 sudo 列表影响
        assert!(matches!(
            engine.check("visudo", SecurityPolicy::Recommended),
            CheckResult::Allow
        ));
        // root 执行命中 sudo 黑名单 → 直接拒绝
        assert!(matches!(
            engine.check_with_run_as("visudo", SecurityPolicy::Recommended, true),
            CheckResult::Block { .. }
        ));
        // root 执行命中 sudo 确认清单 → 逐条确认
        match engine.check_with_run_as("rm -f /tmp/x", SecurityPolicy::Recommended, true) {
            CheckResult::WaitConfirm { rule } => assert!(rule.starts_with("sudo:")),
            other => panic!("expected WaitConfirm, got {other:?}"),
        }
        // 未命中任何规则 → 放行
        assert!(matches!(
            engine.check_with_run_as("ss -ulnp", SecurityPolicy::Recommended, true),
            CheckResult::Allow
        ));
    }

    #[test]
    fn strict_blocks_blocklists_before_requiring_confirmation() {
        let engine = SecurityEngine::new(
            patterns(&["^global danger$"]),
            patterns(&["^confirm me$"]),
            AIPolicyOverride::Strict,
        );
        engine.update_session(AIPolicyOverride::Strict, &patterns(&["^session danger$"]));

        assert_eq!(
            engine.check("session danger", SecurityPolicy::Recommended),
            CheckResult::Block {
                rule: "^session danger$".into()
            }
        );
        assert_eq!(
            engine.check("global danger", SecurityPolicy::Recommended),
            CheckResult::Block {
                rule: "^global danger$".into()
            }
        );
        assert_eq!(
            engine.check("confirm me", SecurityPolicy::Recommended),
            CheckResult::WaitConfirm {
                rule: "strict policy".into()
            }
        );
        assert_eq!(
            engine.check("harmless", SecurityPolicy::Recommended),
            CheckResult::WaitConfirm {
                rule: "strict policy".into()
            }
        );
    }

    #[test]
    fn recommended_blocks_confirms_and_allows() {
        let engine = SecurityEngine::new(
            patterns(&["^blocked$"]),
            patterns(&["^confirm$"]),
            AIPolicyOverride::Recommended,
        );

        assert_eq!(
            engine.check("blocked", SecurityPolicy::Recommended),
            CheckResult::Block {
                rule: "^blocked$".into()
            }
        );
        assert_eq!(
            engine.check("confirm", SecurityPolicy::Recommended),
            CheckResult::WaitConfirm {
                rule: "^confirm$".into()
            }
        );
        assert_eq!(
            engine.check("allowed", SecurityPolicy::Recommended),
            CheckResult::Allow
        );
    }

    #[test]
    fn permissive_keeps_blocklists_but_skips_confirmations() {
        let engine = SecurityEngine::new(
            patterns(&["^global blocked$"]),
            patterns(&["^confirm$"]),
            AIPolicyOverride::Permissive,
        );
        engine.update_session(
            AIPolicyOverride::Permissive,
            &patterns(&["^session blocked$"]),
        );

        assert_eq!(
            engine.check("session blocked", SecurityPolicy::Recommended),
            CheckResult::Block {
                rule: "^session blocked$".into()
            }
        );
        assert_eq!(
            engine.check("global blocked", SecurityPolicy::Recommended),
            CheckResult::Block {
                rule: "^global blocked$".into()
            }
        );
        assert_eq!(
            engine.check("confirm", SecurityPolicy::Recommended),
            CheckResult::Allow
        );
        assert_eq!(
            engine.check("allowed", SecurityPolicy::Recommended),
            CheckResult::Allow
        );
    }

    #[test]
    fn session_blocklist_has_priority_over_global_blocklist_in_every_policy() {
        for policy in [
            AIPolicyOverride::Strict,
            AIPolicyOverride::Recommended,
            AIPolicyOverride::Permissive,
        ] {
            let engine = SecurityEngine::new(patterns(&["danger"]), Vec::new(), policy);
            engine.update_session(policy, &patterns(&["^danger$"]));

            assert_eq!(
                engine.check("danger", SecurityPolicy::Recommended),
                CheckResult::Block {
                    rule: "^danger$".into()
                }
            );
        }
    }

    #[test]
    fn invalid_regexes_are_ignored_without_disabling_valid_rules() {
        let engine = SecurityEngine::new(
            patterns(&["[", "^blocked$"]),
            patterns(&["(", "^confirm$"]),
            AIPolicyOverride::Recommended,
        );
        engine.update_session(
            AIPolicyOverride::Recommended,
            &patterns(&["*", "^session$"]),
        );

        assert_eq!(
            engine.check("blocked", SecurityPolicy::Recommended),
            CheckResult::Block {
                rule: "^blocked$".into()
            }
        );
        assert_eq!(
            engine.check("session", SecurityPolicy::Recommended),
            CheckResult::Block {
                rule: "^session$".into()
            }
        );
        assert_eq!(
            engine.check("confirm", SecurityPolicy::Recommended),
            CheckResult::WaitConfirm {
                rule: "^confirm$".into()
            }
        );
        assert_eq!(
            engine.check("allowed", SecurityPolicy::Recommended),
            CheckResult::Allow
        );
    }

    #[test]
    fn inherit_reads_the_current_global_default_for_every_check() {
        let engine = SecurityEngine::new(
            Vec::new(),
            patterns(&["^confirm$"]),
            AIPolicyOverride::Inherit,
        );

        assert_eq!(
            engine.check("anything", SecurityPolicy::Strict),
            CheckResult::WaitConfirm {
                rule: "strict policy".into()
            }
        );
        assert_eq!(
            engine.check("confirm", SecurityPolicy::Recommended),
            CheckResult::WaitConfirm {
                rule: "^confirm$".into()
            }
        );
        assert_eq!(
            engine.check("confirm", SecurityPolicy::Permissive),
            CheckResult::Allow
        );
    }

    #[test]
    fn live_session_update_replaces_policy_and_blocklist() {
        let engine = SecurityEngine::new(
            Vec::new(),
            patterns(&["^confirm$"]),
            AIPolicyOverride::Recommended,
        );
        engine.update_session(AIPolicyOverride::Recommended, &patterns(&["^old$"]));
        assert_eq!(
            engine.check("old", SecurityPolicy::Recommended),
            CheckResult::Block {
                rule: "^old$".into()
            }
        );

        engine.update_session(AIPolicyOverride::Permissive, &patterns(&["^new$"]));
        assert_eq!(
            engine.check("old", SecurityPolicy::Recommended),
            CheckResult::Allow
        );
        assert_eq!(
            engine.check("new", SecurityPolicy::Recommended),
            CheckResult::Block {
                rule: "^new$".into()
            }
        );
        assert_eq!(
            engine.check("confirm", SecurityPolicy::Recommended),
            CheckResult::Allow
        );
    }
}
