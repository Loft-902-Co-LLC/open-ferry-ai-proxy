//! A streaming call's allowance also covers schema walks and copies. Ordinary
//! argument cleaning has no active meter and keeps its existing behavior.

use crate::common::vertex_partial_args::WorkBudget;
use serde_json::Value;
use std::cell::RefCell;

thread_local! {
    static ACTIVE: RefCell<Option<WorkBudget>> = const { RefCell::new(None) };
}

struct Scope(Option<WorkBudget>);

impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.replace(self.0.take()));
    }
}

pub(super) fn with<T>(work: &WorkBudget, run: impl FnOnce() -> T) -> T {
    let _scope = Scope(ACTIVE.with(|active| active.replace(Some(work.clone()))));
    run()
}

pub(super) fn enabled() -> bool {
    ACTIVE.with(|active| active.borrow().is_some())
}

pub(super) fn charge(amount: usize) -> bool {
    ACTIVE.with(|active| {
        active
            .borrow()
            .as_ref()
            .is_none_or(|work| work.charge(amount).is_ok())
    })
}

pub(super) fn predicate(amount: usize) -> bool {
    ACTIVE.with(|active| {
        active
            .borrow()
            .as_ref()
            .is_none_or(|work| work.charge_predicate(amount).is_ok())
    })
}

pub(super) fn predicate_object(_amount: usize) {
    #[cfg(test)]
    ACTIVE.with(|active| {
        if let Some(work) = active.borrow().as_ref() {
            work.add_predicate_allowance(_amount);
        }
    });
}

pub(super) fn analyzed(_node: usize, _scope: usize, _depth: usize, _active: usize) {
    #[cfg(test)]
    ACTIVE.with(|active| {
        if let Some(work) = active.borrow().as_ref() {
            work.record_analysis(_node, _scope, _depth, _active);
        }
    });
}

pub(super) fn value(value: &Value) -> bool {
    copies(value, 1)
}

// The recursive cleaner visits children itself. Charging their complete
// subtrees again at each ancestor would make even a probe's own path quadratic.
pub(super) fn node(value: &Value) -> bool {
    if !enabled() {
        return true;
    }
    let bytes = match value {
        Value::String(text) => text.len(),
        Value::Number(number) => number.as_str().len(),
        _ => 1,
    };
    charge(bytes.saturating_add(32))
        && match value {
            Value::Object(map) => map.keys().all(|key| charge(key.len().saturating_add(32))),
            Value::Array(values) => charge(values.len().saturating_mul(32)),
            _ => true,
        }
}

// Include container entries, key comparisons, scalar coercions, and copied
// strings. Charge before pushing children, so even the preflight stack is bounded.
pub(super) fn copies(value: &Value, passes: usize) -> bool {
    ACTIVE.with(|active| {
        let active = active.borrow();
        let Some(work) = active.as_ref() else {
            return true;
        };
        let mut stack = vec![value];
        while let Some(value) = stack.pop() {
            let bytes = match value {
                Value::String(text) => text.len(),
                Value::Number(number) => number.as_str().len(),
                _ => 1,
            };
            if work
                .charge(bytes.saturating_add(32).saturating_mul(passes))
                .is_err()
            {
                return false;
            }
            match value {
                Value::Object(map) => {
                    for (key, value) in map {
                        if work
                            .charge(key.len().saturating_add(32).saturating_mul(passes))
                            .is_err()
                        {
                            return false;
                        }
                        stack.push(value);
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        if work.charge(32usize.saturating_mul(passes)).is_err() {
                            return false;
                        }
                        stack.push(value);
                    }
                }
                _ => {}
            }
        }
        true
    })
}

pub(super) fn copy(data: &Value) -> Value {
    if value(data) {
        data.clone()
    } else {
        Value::Null
    }
}

pub(super) fn text(data: &Value) -> String {
    if value(data) {
        data.to_string()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scopes_restore_the_prior_budget_including_unwind() {
        let outer = WorkBudget::default();
        let inner = WorkBudget::default();
        with(&outer, || {
            assert!(charge(7));
            with(&inner, || assert!(charge(11)));
            assert!(charge(13));
        });
        assert_eq!(outer.used(), 20);
        assert_eq!(inner.used(), 11);
        let _ = std::panic::catch_unwind(|| with(&outer, || panic!("scope fixture")));
        assert!(!enabled());
        assert!(charge(usize::MAX));
        assert!(outer.check().is_ok());
    }

    #[test]
    fn memoized_info_copies_spend_the_same_call_allowance() {
        let schema =
            json!({"type":"object","properties":{"value":{"description":"x".repeat(4096)}}});
        let info = super::super::analyze(&schema, &schema);
        let work = WorkBudget::default();
        work.charge(64 * 1024 * 1024 - 1024).unwrap();
        with(&work, || {
            let _ = info.clone();
        });
        assert!(work.check().is_err());
    }

    #[test]
    fn projection_work_is_charged_even_for_a_tiny_placeholder_probe() {
        let schema =
            json!({"type":"object","properties":{"value":{"description":"x".repeat(4096)}}});
        let work = WorkBudget::default();
        work.charge(64 * 1024 * 1024 - 32 * 1024).unwrap();
        with(&work, || {
            super::super::reclean::requires(&schema, "_");
        });
        assert!(work.check().is_err());
        assert!(super::super::reclean::requires(&schema, "_"));
    }
}
