use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Node;
use kube::api::PartialObjectMeta;

use crate::v1beta1::{self, SelectorExpression, SelectorOperator};

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

    exprs.iter().all(|expr| match expr.operator {
        SelectorOperator::In => matches_expression_in(
            labels,
            &expr.key,
            expr.values.as_ref().unwrap_or(&Vec::new()),
        ),
        SelectorOperator::DoesNotExist => matches_expression_doesnotexist(labels, &expr.key),
    })
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
}
