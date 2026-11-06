//! Model routing rules: rewrite one model to another, or deny a model.
//!
//! Rules come from the config file and from the admin API (stored in the
//! shared store and reloaded by every replica with the API keys). They apply
//! to the requested model and to every fallback model before routing.

use std::collections::HashMap;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::config::{RoutingRuleConfig, RuleAction};

/// Store hash for rules created through the admin API: field = id.
pub const STORED_RULES: &str = "routing_rules";
const MAX_REWRITES: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub source: String,
    pub action: RuleAction,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// `config` or `admin`.
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub created_at: u64,
}

fn enabled() -> bool {
    true
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The model to serve (possibly unchanged).
    Model(String),
    Denied {
        rule_id: String,
        reason: Option<String>,
    },
    Loop,
}

impl Rule {
    fn matches(&self, model: &str) -> bool {
        self.enabled
            && (self.source == "*"
                || self.source == model
                || self
                    .source
                    .strip_suffix("/*")
                    .is_some_and(|p| model.split_once('/').is_some_and(|(mp, _)| mp == p)))
    }
}

#[derive(Default)]
pub struct RuleSet {
    config: RwLock<Vec<Rule>>,
    stored: RwLock<Vec<Rule>>,
}

impl RuleSet {
    pub fn set_config(&self, rules: &[RoutingRuleConfig]) {
        *self.config.write() = rules
            .iter()
            .enumerate()
            .map(|(i, r)| Rule {
                id: format!("config-{}", i + 1),
                source: r.source.clone(),
                action: r.action,
                destination: r.destination.clone(),
                reason: r.reason.clone(),
                enabled: true,
                origin: "config".into(),
                created_at: 0,
            })
            .collect();
    }

    pub fn set_stored(&self, mut rules: Vec<Rule>) {
        rules.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        *self.stored.write() = rules;
    }

    pub fn upsert_stored(&self, rule: Rule) {
        let mut s = self.stored.write();
        s.retain(|r| r.id != rule.id);
        s.push(rule);
        s.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
    }

    pub fn remove_stored(&self, id: &str) {
        self.stored.write().retain(|r| r.id != id);
    }

    pub fn all(&self) -> Vec<Rule> {
        let mut v = self.config.read().clone();
        v.extend(self.stored.read().iter().cloned());
        v
    }

    /// Applies rules to `model`. Config rules come first, then admin rules,
    /// each in creation order; the first match wins at each step.
    pub fn apply(&self, model: &str) -> Outcome {
        let rules = self.all();
        if rules.is_empty() {
            return Outcome::Model(model.to_string());
        }
        let mut current = model.to_string();
        let mut seen: HashMap<String, ()> = HashMap::new();
        for _ in 0..=MAX_REWRITES {
            let Some(rule) = rules.iter().find(|r| r.matches(&current)) else {
                return Outcome::Model(current);
            };
            match rule.action {
                RuleAction::Deny => {
                    return Outcome::Denied { rule_id: rule.id.clone(), reason: rule.reason.clone() }
                }
                RuleAction::Rewrite => {
                    let Some(dest) = rule.destination.clone() else { return Outcome::Model(current) };
                    if dest == current {
                        return Outcome::Model(current);
                    }
                    if seen.insert(current.clone(), ()).is_some() {
                        return Outcome::Loop;
                    }
                    current = dest;
                }
            }
        }
        Outcome::Loop
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(source: &str, action: RuleAction, dest: Option<&str>) -> RoutingRuleConfig {
        RoutingRuleConfig { source: source.into(), action, destination: dest.map(Into::into), reason: None }
    }

    #[test]
    fn rewrites_denies_and_detects_loops() {
        let rs = RuleSet::default();
        assert_eq!(rs.apply("a/x"), Outcome::Model("a/x".into()));
        rs.set_config(&[
            cfg("a/expensive", RuleAction::Rewrite, Some("a/cheap")),
            cfg("banned/*", RuleAction::Deny, None),
        ]);
        assert_eq!(rs.apply("a/expensive"), Outcome::Model("a/cheap".into()));
        assert!(matches!(rs.apply("banned/model"), Outcome::Denied { .. }));
        assert_eq!(rs.apply("a/other"), Outcome::Model("a/other".into()));

        rs.upsert_stored(Rule {
            id: "r1".into(),
            source: "a/cheap".into(),
            action: RuleAction::Rewrite,
            destination: Some("a/expensive".into()),
            reason: None,
            enabled: true,
            origin: "admin".into(),
            created_at: 1,
        });
        assert_eq!(rs.apply("a/expensive"), Outcome::Loop);
        rs.remove_stored("r1");
        assert_eq!(rs.apply("a/expensive"), Outcome::Model("a/cheap".into()));
    }
}
