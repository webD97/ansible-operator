use crate::v1beta1;

/// Evaluates the triggers configured on a PlaybookPlan and tells if the playbook must be
/// executed now (true) or at some later point in time (false).
pub fn evaluate_triggers(triggers: Option<&v1beta1::ExecutionTriggers>) -> bool {
    if triggers.is_none() {
        return true;
    }

    if triggers.is_some_and(|triggers| triggers.delayed_until.is_none()) {
        return true;
    }

    false
}
