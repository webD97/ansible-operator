use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Node;
use kube::api::PartialObjectMeta;

use crate::v1beta1::{self, SelectorExpression, SelectorOperator};

/// Returns `true` if the node satisfies the given selector term.
///
/// A node satisfies a term when it matches **all** `matchLabels` key-value
/// pairs and **all** `matchExpressions` expressions. Missing fields are
/// treated as empty and therefore always satisfied.
///
/// If `selector` is `None` the node is considered a match unconditionally.
pub fn node_matches(
    node: &PartialObjectMeta<Node>,
    selector: Option<&v1beta1::NodeSelectorTerm>,
) -> bool {
    let Some(selector) = selector else {
        return true;
    };

    let matches_labels = selector
        .match_labels
        .as_ref()
        .map(|match_labels| node_matches_match_labels(node, match_labels))
        .unwrap_or(true);

    let matches_expressions = selector
        .match_expressions
        .as_ref()
        .map(|match_expressions| node_matches_match_expressions(node, match_expressions))
        .unwrap_or(true);

    matches_labels && matches_expressions
}

fn node_matches_match_labels(node: &PartialObjectMeta<Node>, labels: &v1beta1::LabelMap) -> bool {
    use kube::ResourceExt as _;
    let actual_labels = node.labels();

    labels
        .iter()
        .all(|(key, value)| actual_labels.get(key).is_some_and(|v| v == value))
}

fn node_matches_match_expressions(
    node: &PartialObjectMeta<Node>,
    exprs: &[SelectorExpression],
) -> bool {
    use kube::ResourceExt as _;
    let labels = node.labels();

    exprs.iter().all(|expr| eval_expression(labels, expr))
}

/// Evaluates a single `matchExpressions` term against a raw label map.
fn eval_expression(labels: &BTreeMap<String, String>, expr: &SelectorExpression) -> bool {
    match expr.operator {
        SelectorOperator::In => {
            matches_expression_in(labels, &expr.key, expr.values.as_deref().unwrap_or(&[]))
        }
        SelectorOperator::NotIn => {
            matches_expression_notin(labels, &expr.key, expr.values.as_deref().unwrap_or(&[]))
        }
        SelectorOperator::Exists => matches_expression_exists(labels, &expr.key),
        SelectorOperator::DoesNotExist => matches_expression_doesnotexist(labels, &expr.key),
    }
}

/// Evaluates a label selector against a raw label map with Kubernetes' **default** semantics: an
/// absent `matchLabels`/`matchExpressions` imposes no constraint, so an entirely empty selector
/// matches *everything*. Works on any object's labels (Node, Namespace, …).
pub fn selector_matches(
    labels: &BTreeMap<String, String>,
    selector: &v1beta1::NodeSelectorTerm,
) -> bool {
    let matches_labels = selector
        .match_labels
        .as_ref()
        .map(|ml| {
            ml.iter()
                .all(|(k, v)| labels.get(k).is_some_and(|actual| actual == v))
        })
        .unwrap_or(true);

    let matches_expressions = selector
        .match_expressions
        .as_ref()
        .map(|exprs| exprs.iter().all(|expr| eval_expression(labels, expr)))
        .unwrap_or(true);

    matches_labels && matches_expressions
}

/// **Fail-closed** variant for authorization ceilings such as `NodeAccessPolicy`: an *empty*
/// selector (no `matchLabels` entries and no `matchExpressions`) matches *nothing* rather than
/// everything. Any non-empty selector is evaluated exactly as [`selector_matches`]. This is the
/// opposite default from [`node_matches`]/[`selector_matches`] and must be used wherever an empty
/// selector should grant no access.
pub fn selector_matches_fail_closed(
    labels: &BTreeMap<String, String>,
    selector: &v1beta1::NodeSelectorTerm,
) -> bool {
    let is_empty = selector
        .match_labels
        .as_ref()
        .map(|m| m.is_empty())
        .unwrap_or(true)
        && selector
            .match_expressions
            .as_ref()
            .map(|e| e.is_empty())
            .unwrap_or(true);

    !is_empty && selector_matches(labels, selector)
}

fn matches_expression_in(
    map: &BTreeMap<String, String>,
    key: &str,
    values: &[impl AsRef<str>],
) -> bool {
    map.get(key)
        .map(|v| values.iter().any(|s| s.as_ref() == v.as_str()))
        .unwrap_or(false)
}

fn matches_expression_notin(
    map: &BTreeMap<String, String>,
    key: &str,
    values: &[impl AsRef<str>],
) -> bool {
    map.get(key)
        .map(|v| !values.iter().any(|s| s.as_ref() == v.as_str()))
        .unwrap_or(true)
}

fn matches_expression_exists(map: &BTreeMap<String, String>, key: &str) -> bool {
    map.contains_key(key)
}

fn matches_expression_doesnotexist(map: &BTreeMap<String, String>, key: &str) -> bool {
    !map.contains_key(key)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use k8s_openapi::api::core::v1::Node;
    use kube::{Resource as _, api::PartialObjectMeta};

    use super::{node_matches, node_matches_match_expressions, node_matches_match_labels};
    use crate::v1beta1::{NodeSelectorTerm, SelectorExpression, SelectorOperator};

    fn make_node(
        labels: impl IntoIterator<Item = (&'static str, &'static str)>,
    ) -> PartialObjectMeta<Node> {
        let mut node = Node::default();
        node.metadata.labels = Some(
            labels
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        PartialObjectMeta {
            metadata: node.meta().clone(),
            ..Default::default()
        }
    }

    fn label_selector(
        pairs: impl IntoIterator<Item = (&'static str, &'static str)>,
    ) -> BTreeMap<String, String> {
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn node_matches_no_selector_always_true() {
        let node = make_node([("key", "value")]);
        assert!(node_matches(&node, None));
    }

    #[test]
    fn node_matches_empty_selector_always_true() {
        let node = make_node([]);
        let selector = NodeSelectorTerm {
            match_labels: None,
            match_expressions: None,
        };
        assert!(node_matches(&node, Some(&selector)));
    }

    #[test]
    fn node_matches_labels_and_expressions_both_pass() {
        let node = make_node([("env", "prod"), ("region", "eu")]);
        let selector = NodeSelectorTerm {
            match_labels: Some(label_selector([("env", "prod")])),
            match_expressions: Some(vec![SelectorExpression {
                operator: SelectorOperator::DoesNotExist,
                key: "spot".to_string(),
                values: None,
            }]),
        };
        assert!(node_matches(&node, Some(&selector)));
    }

    #[test]
    fn node_matches_fails_when_labels_fail() {
        let node = make_node([("env", "staging")]);
        let selector = NodeSelectorTerm {
            match_labels: Some(label_selector([("env", "prod")])),
            match_expressions: None,
        };
        assert!(!node_matches(&node, Some(&selector)));
    }

    #[test]
    fn node_matches_fails_when_expressions_fail() {
        let node = make_node([("env", "prod"), ("spot", "true")]);
        let selector = NodeSelectorTerm {
            match_labels: Some(label_selector([("env", "prod")])),
            match_expressions: Some(vec![SelectorExpression {
                operator: SelectorOperator::DoesNotExist,
                key: "spot".to_string(),
                values: None,
            }]),
        };
        assert!(!node_matches(&node, Some(&selector)));
    }

    #[test]
    fn match_labels_all_present_and_equal() {
        let node = make_node([("a", "1"), ("b", "2"), ("c", "3")]);
        let selector = label_selector([("a", "1"), ("b", "2")]);
        assert!(node_matches_match_labels(&node, &selector));
    }

    #[test]
    fn match_labels_key_missing_from_node() {
        let node = make_node([("a", "1")]);
        let selector = label_selector([("z", "99")]);
        assert!(!node_matches_match_labels(&node, &selector));
    }

    #[test]
    fn match_labels_key_present_wrong_value() {
        let node = make_node([("env", "staging")]);
        let selector = label_selector([("env", "prod")]);
        assert!(!node_matches_match_labels(&node, &selector));
    }

    #[test]
    fn match_labels_empty_selector_matches_any_node() {
        let node = make_node([("a", "1")]);
        let selector = label_selector([]);
        assert!(node_matches_match_labels(&node, &selector));
    }

    #[test]
    fn match_labels_node_has_no_labels() {
        let node = make_node([]);
        let selector = label_selector([("a", "1")]);
        assert!(!node_matches_match_labels(&node, &selector));
    }

    #[test]
    fn expressions_in_key_matches_value() {
        let node = make_node([("zone", "eu-west-1")]);
        let exprs = vec![SelectorExpression {
            operator: SelectorOperator::In,
            key: "zone".to_string(),
            values: Some(vec!["eu-west-1".to_string(), "eu-central-1".to_string()]),
        }];
        assert!(node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_in_key_value_not_in_list() {
        let node = make_node([("zone", "us-east-1")]);
        let exprs = vec![SelectorExpression {
            operator: SelectorOperator::In,
            key: "zone".to_string(),
            values: Some(vec!["eu-west-1".to_string(), "eu-central-1".to_string()]),
        }];
        assert!(!node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_in_key_missing_from_node() {
        let node = make_node([]);
        let exprs = vec![SelectorExpression {
            operator: SelectorOperator::In,
            key: "zone".to_string(),
            values: Some(vec!["eu-west-1".to_string()]),
        }];
        assert!(!node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_in_empty_values_never_matches() {
        let node = make_node([("zone", "eu-west-1")]);
        let exprs = vec![SelectorExpression {
            operator: SelectorOperator::In,
            key: "zone".to_string(),
            values: None,
        }];
        assert!(!node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_doesnotexist_key_absent() {
        let node = make_node([("env", "prod")]);
        let exprs = vec![SelectorExpression {
            operator: SelectorOperator::DoesNotExist,
            key: "spot".to_string(),
            values: None,
        }];
        assert!(node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_doesnotexist_key_present() {
        let node = make_node([("spot", "true")]);
        let exprs = vec![SelectorExpression {
            operator: SelectorOperator::DoesNotExist,
            key: "spot".to_string(),
            values: None,
        }];
        assert!(!node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_doesnotexist_node_has_no_labels() {
        let node = make_node([]);
        let exprs = vec![SelectorExpression {
            operator: SelectorOperator::DoesNotExist,
            key: "spot".to_string(),
            values: None,
        }];
        assert!(node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_multiple_all_must_pass() {
        let node = make_node([("zone", "eu-west-1"), ("env", "prod")]);
        let exprs = vec![
            SelectorExpression {
                operator: SelectorOperator::In,
                key: "zone".to_string(),
                values: Some(vec!["eu-west-1".to_string()]),
            },
            SelectorExpression {
                operator: SelectorOperator::DoesNotExist,
                key: "spot".to_string(),
                values: None,
            },
        ];
        assert!(node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_multiple_one_failing_fails_all() {
        let node = make_node([("zone", "eu-west-1"), ("spot", "true")]);
        let exprs = vec![
            SelectorExpression {
                operator: SelectorOperator::In,
                key: "zone".to_string(),
                values: Some(vec!["eu-west-1".to_string()]),
            },
            SelectorExpression {
                operator: SelectorOperator::DoesNotExist,
                key: "spot".to_string(),
                values: None,
            },
        ];
        assert!(!node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_exists_key_present() {
        let node = make_node([("env", "prod")]);
        let exprs = vec![SelectorExpression {
            operator: SelectorOperator::Exists,
            key: "env".to_string(),
            values: None,
        }];
        assert!(node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_exists_key_absent() {
        let node = make_node([("env", "prod")]);
        let exprs = vec![SelectorExpression {
            operator: SelectorOperator::Exists,
            key: "spot".to_string(),
            values: None,
        }];
        assert!(!node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_exists_node_has_no_labels() {
        let node = make_node([]);
        let exprs = vec![SelectorExpression {
            operator: SelectorOperator::Exists,
            key: "env".to_string(),
            values: None,
        }];
        assert!(!node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_notin_key_absent_always_matches() {
        let node = make_node([("env", "prod")]);
        let exprs = vec![SelectorExpression {
            operator: SelectorOperator::NotIn,
            key: "zone".to_string(),
            values: Some(vec!["eu-west-1".to_string()]),
        }];
        assert!(node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_notin_key_present_value_not_in_list() {
        let node = make_node([("zone", "us-east-1")]);
        let exprs = vec![SelectorExpression {
            operator: SelectorOperator::NotIn,
            key: "zone".to_string(),
            values: Some(vec!["eu-west-1".to_string(), "eu-central-1".to_string()]),
        }];
        assert!(node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_notin_key_present_value_in_list() {
        let node = make_node([("zone", "eu-west-1")]);
        let exprs = vec![SelectorExpression {
            operator: SelectorOperator::NotIn,
            key: "zone".to_string(),
            values: Some(vec!["eu-west-1".to_string(), "eu-central-1".to_string()]),
        }];
        assert!(!node_matches_match_expressions(&node, &exprs));
    }

    #[test]
    fn expressions_notin_empty_values_always_matches() {
        // NotIn [] means "value is not in the empty set" → always true
        let node = make_node([("zone", "eu-west-1")]);
        let exprs = vec![SelectorExpression {
            operator: SelectorOperator::NotIn,
            key: "zone".to_string(),
            values: None,
        }];
        assert!(node_matches_match_expressions(&node, &exprs));
    }

    // --- fail-closed selector matching (NodeAccessPolicy) ---

    use super::{selector_matches, selector_matches_fail_closed};

    fn labels(
        pairs: impl IntoIterator<Item = (&'static str, &'static str)>,
    ) -> BTreeMap<String, String> {
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn selector_matches_empty_selector_matches_everything() {
        // Default (non-fail-closed) semantics: empty selector matches any labels.
        let empty = NodeSelectorTerm::default();
        assert!(selector_matches(&labels([("a", "1")]), &empty));
        assert!(selector_matches(&labels([]), &empty));
    }

    #[test]
    fn fail_closed_empty_selector_matches_nothing() {
        // The security-critical inversion: an empty ceiling grants no access.
        let empty = NodeSelectorTerm::default();
        assert!(!selector_matches_fail_closed(&labels([("a", "1")]), &empty));
        assert!(!selector_matches_fail_closed(&labels([]), &empty));

        // An empty matchLabels map (not just `None`) is still "empty" → nothing.
        let empty_map = NodeSelectorTerm {
            match_labels: Some(label_selector([])),
            match_expressions: Some(vec![]),
        };
        assert!(!selector_matches_fail_closed(
            &labels([("a", "1")]),
            &empty_map
        ));
    }

    #[test]
    fn fail_closed_nonempty_selector_matches_like_normal() {
        let sel = NodeSelectorTerm {
            match_labels: Some(label_selector([("node-pool", "business")])),
            match_expressions: None,
        };
        assert!(selector_matches_fail_closed(
            &labels([("node-pool", "business")]),
            &sel
        ));
        assert!(!selector_matches_fail_closed(
            &labels([("node-pool", "platform")]),
            &sel
        ));

        // "Allow all" must be expressed explicitly, e.g. Exists on a ubiquitous label.
        let all = NodeSelectorTerm {
            match_labels: None,
            match_expressions: Some(vec![SelectorExpression {
                operator: SelectorOperator::Exists,
                key: "kubernetes.io/hostname".to_string(),
                values: None,
            }]),
        };
        assert!(selector_matches_fail_closed(
            &labels([("kubernetes.io/hostname", "n1")]),
            &all
        ));
        assert!(!selector_matches_fail_closed(
            &labels([("other", "x")]),
            &all
        ));
    }
}
