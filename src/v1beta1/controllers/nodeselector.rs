use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Node;

use crate::v1beta1;

pub fn node_matches(node: &Node, selector: &v1beta1::NodeSelectorTerm) -> bool {
    match selector {
        v1beta1::NodeSelectorTerm::MatchLabels { labels } => {
            node_matches_match_labels(node, labels)
        }
    }
}

fn node_matches_match_labels(node: &Node, labels: &v1beta1::LabelMap) -> bool {
    const EMPTY_LABELS: &v1beta1::LabelMap = &BTreeMap::new();

    let actual_labels = node.metadata.labels.as_ref().unwrap_or(EMPTY_LABELS);

    labels
        .iter()
        .all(|(key, value)| actual_labels.get(key).is_some_and(|v| v == value))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use k8s_openapi::api::core::v1::Node;

    use crate::v1beta1::controllers::nodeselector::node_matches_match_labels;

    #[test]
    fn test_node_matches_match_labels() {
        // Given
        let mut node = Node::default();
        let labels = {
            let mut labels = BTreeMap::new();

            labels.insert("key-a".to_string(), "value-a".to_string());
            labels.insert("key-b".to_string(), "value-b".to_string());
            labels.insert("key-c".to_string(), "value-c".to_string());

            labels
        };
        node.metadata.labels = Some(labels);

        // When
        let selector1 = {
            let mut selector = BTreeMap::new();
            selector.insert("key-a".to_string(), "value-a".to_string());
            selector
        };
        let selector2 = {
            let mut selector = BTreeMap::new();
            selector.insert("key-z".to_string(), "value-z".to_string());
            selector
        };

        let selector1_matches = node_matches_match_labels(&node, &selector1);
        let selector2_matches = node_matches_match_labels(&node, &selector2);

        // Then
        assert!(selector1_matches);
        assert!(!selector2_matches);
    }
}
