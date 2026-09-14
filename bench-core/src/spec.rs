//! Pass/fail specs — the rules that make this a test bench rather than a scope.
//!
//! A spec is a set of limits over [`Features`](crate::features::Features),
//! compiled to Prolog rules and stored in a logic cell. Once stored, the
//! verdict is *derived* — no tokens, no model, and the reasoning is auditable
//! because the rule that fired is a fact you can query.
//!
//! Bench does not evaluate specs locally. It could, but then the demo would be
//! a lie: the point is that a rule stored in a cell can be queried by anything
//! that reaches the gateway, and exposed as an HTTP endpoint via
//! `reactor.expose` without being reimplemented.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// A comparison a limit applies to a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cmp {
    Lt,
    Lte,
    Gt,
    Gte,
}

impl Cmp {
    /// The Prolog operator. `=<` rather than `<=` — ISO Prolog, not C.
    fn prolog(&self) -> &'static str {
        match self {
            Cmp::Lt => "<",
            Cmp::Lte => "=<",
            Cmp::Gt => ">",
            Cmp::Gte => ">=",
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            Cmp::Lt => "below",
            Cmp::Lte => "at most",
            Cmp::Gt => "above",
            Cmp::Gte => "at least",
        }
    }
}

/// One limit: a measurement must compare a certain way against a value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limit {
    /// Metric name as written by [`Features::to_facts`](crate::features::Features::to_facts),
    /// e.g. `thd_pct`.
    pub metric: String,
    pub cmp: Cmp,
    pub value: f64,
}

impl Limit {
    pub fn new(metric: impl Into<String>, cmp: Cmp, value: f64) -> Self {
        Self {
            metric: metric.into(),
            cmp,
            value,
        }
    }

    /// The goal that FAILS this limit, for a capture bound to `C`.
    ///
    /// Expressed as a violation rather than a satisfaction because a capture
    /// missing the metric entirely must not count as a failure — with no
    /// `metric/3` fact the goal simply does not unify, and the limit is
    /// silently inapplicable rather than falsely failing.
    fn violation_goal(&self) -> String {
        format!(
            "metric(C, {}, V), V {} {}",
            self.metric,
            invert(self.cmp).prolog(),
            fmt_num(self.value)
        )
    }

    pub fn describe(&self) -> String {
        format!(
            "{} {} {}",
            self.metric,
            self.cmp.describe(),
            fmt_num(self.value)
        )
    }
}

fn invert(cmp: Cmp) -> Cmp {
    match cmp {
        Cmp::Lt => Cmp::Gte,
        Cmp::Lte => Cmp::Gt,
        Cmp::Gt => Cmp::Lte,
        Cmp::Gte => Cmp::Lt,
    }
}

/// A named set of limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    /// Prolog-safe atom, e.g. `audio_amp_v1`.
    pub name: String,
    pub limits: Vec<Limit>,
}

impl Spec {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            limits: Vec::new(),
        }
    }

    pub fn with(mut self, limit: Limit) -> Self {
        self.limits.push(limit);
        self
    }

    /// The rules implementing this spec.
    ///
    /// Emits one `spec_violation/3` clause per limit plus a `spec_pass/2`
    /// that succeeds when none fire. Per-limit clauses rather than one big
    /// conjunction so a failing capture can report *which* limit failed —
    /// "fails spec" is not an actionable bench reading.
    pub fn to_rules(&self) -> Vec<String> {
        let mut rules: Vec<String> = self
            .limits
            .iter()
            .map(|limit| {
                format!(
                    "spec_violation({}, C, {}) :- {}.",
                    self.name,
                    limit.metric,
                    limit.violation_goal()
                )
            })
            .collect();

        rules.push(format!(
            "spec_pass({name}, C) :- metric(C, sample_count, _), \\+ spec_violation({name}, C, _).",
            name = self.name
        ));
        rules
    }

    /// `logic.batch` args storing every rule in one charged call.
    pub fn to_batch_args(&self, namespace: &str) -> Value {
        json!({
            "namespace": namespace,
            "ops": self.to_rules()
                .into_iter()
                .map(|rule| json!({"op": "assert_rule", "rule": rule}))
                .collect::<Vec<_>>(),
        })
    }

    /// The goal that asks for a capture's verdict.
    pub fn verdict_goal(&self, capture_id: &str) -> String {
        format!("spec_pass({}, {})", self.name, capture_id)
    }

    /// The goal listing a capture's violations.
    pub fn violations_goal(&self, capture_id: &str) -> String {
        format!("spec_violation({}, {}, Metric)", self.name, capture_id)
    }

    /// `reactor.expose` args turning this spec into an HTTP endpoint.
    ///
    /// The mode annotation is the contract: `+Capture` binds from the request,
    /// `-Verdict` is returned.
    pub fn to_expose_args(&self, namespace: &str) -> Value {
        json!({
            "namespace": namespace,
            "rule": "spec_pass",
            "modes": "+spec:atom, +capture:atom",
            "slug": format!("{}-spec", self.name.replace('_', "-")),
        })
    }
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.1}")
    } else {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thd_spec() -> Spec {
        Spec::new("audio_amp_v1")
            .with(Limit::new("thd_pct", Cmp::Lt, 3.0))
            .with(Limit::new("rms", Cmp::Gt, 0.1))
    }

    #[test]
    fn a_limit_compiles_to_its_violation() {
        // "THD must be below 3" is violated when THD >= 3.
        let limit = Limit::new("thd_pct", Cmp::Lt, 3.0);
        assert_eq!(limit.violation_goal(), "metric(C, thd_pct, V), V >= 3.0");
    }

    #[test]
    fn lte_inverts_to_prolog_gt_not_c_style() {
        let limit = Limit::new("rms", Cmp::Gte, 0.1);
        // Inverse of >= is <, and Prolog writes =< for the other direction.
        assert!(limit.violation_goal().contains("V < 0.1"));
    }

    #[test]
    fn emits_one_violation_rule_per_limit_plus_a_pass_rule() {
        let rules = thd_spec().to_rules();
        assert_eq!(rules.len(), 3);
        assert!(rules[0].starts_with("spec_violation(audio_amp_v1, C, thd_pct)"));
        assert!(rules[2].contains("\\+ spec_violation"));
    }

    #[test]
    fn pass_rule_requires_the_capture_to_exist() {
        // Without the sample_count guard, `spec_pass` would succeed for a
        // capture id that was never recorded — negation-as-failure over
        // nothing is vacuously true.
        let rules = thd_spec().to_rules();
        assert!(rules[2].contains("metric(C, sample_count, _)"));
    }

    #[test]
    fn batch_args_carry_one_op_per_rule() {
        let args = thd_spec().to_batch_args("bench");
        assert_eq!(args["ops"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn goals_name_the_spec_and_capture() {
        let spec = thd_spec();
        assert_eq!(
            spec.verdict_goal("capture_042"),
            "spec_pass(audio_amp_v1, capture_042)"
        );
        assert!(spec.violations_goal("capture_042").contains("Metric"));
    }

    #[test]
    fn expose_slug_is_url_safe() {
        let args = thd_spec().to_expose_args("bench");
        assert_eq!(args["slug"], json!("audio-amp-v1-spec"));
    }
}
