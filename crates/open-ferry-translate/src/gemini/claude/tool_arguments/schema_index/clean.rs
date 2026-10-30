use super::*;
impl SchemaIndex {
    pub(crate) fn parameterless(&mut self) -> bool {
        let set = self.set_info(self.original_group, self.original);
        let info = self.infos[set.merged];
        !info.complex
            && info.forbids
            && info.object
            && info.required == 0
            && info.properties == 0
            && !info.dynamic
    }
    pub(crate) fn clean(&mut self, args: &Value) -> Value {
        if self.failed || !budget::value(args) {
            return Value::Null;
        }
        // The upstream entry point cleans objects only; nested arrays are
        // traversed after entering a declared object property.
        if !args.is_object() {
            return budget::copy(args);
        }
        let mut args = budget::copy(args);
        self.clean_value(&mut args, self.original_group, self.translated, true, 0);
        args
    }
    fn matches(&mut self, plan: usize, value: &Value, state: &mut ObjectState) -> bool {
        let required = self.infos[plan].required;
        if required == 0 {
            return true;
        }
        if value.as_object().is_some_and(|v| v.len() <= 1) {
            return value
                .as_object()
                .unwrap()
                .keys()
                .next()
                .and_then(|key| self.name_ids.get(key.as_str()))
                .is_some_and(|key| self.required_singleton(required).contains(&self.maps, *key));
        }
        let matched = if let Some(matched) = state.matches.get(&required) {
            *matched
        } else {
            if !budget::predicate(self.maps.len(required)) {
                return false;
            }
            let present = state.present.get_or_insert_with(|| {
                value
                    .as_object()
                    .unwrap()
                    .keys()
                    .filter_map(|key| self.name_ids.get(key.as_str()).copied())
                    .collect()
            });
            let matched = self
                .maps
                .required_keys(required)
                .into_iter()
                .all(|key| present.contains(&key));
            state.matches.insert(required, matched);
            matched
        };
        if !matched {
            return false;
        }
        for (bit, name) in [(1, "reason"), (2, "_")] {
            if state.removed & bit != 0
                && self
                    .name_ids
                    .get(name)
                    .is_some_and(|key| self.maps.get(required, *key).is_some())
            {
                return false;
            }
        }
        true
    }
    fn parent_keys(&mut self, plan: usize, value: &Value, state: &mut ObjectState) -> Keys {
        let info = self.infos[plan];
        if let Some(keys) = info.parent_static {
            return keys;
        }
        if let Some(keys) = state.parents.get(&(plan, state.removed)) {
            return *keys;
        }
        let mut result = Keys::NONE;
        for (branch, _) in self.maps.entries(info.branches) {
            let child = self.infos[branch];
            let own = child.pattern
                || (!child.forbids && (child.allows || self.matches(branch, value, state)));
            let nested = if own {
                Keys::ALL
            } else {
                self.parent_keys(branch, value, state)
            };
            let mask = self.maps.mask(child.properties);
            let domain = nested.and(
                Keys {
                    default: true,
                    exceptions: mask,
                },
                &mut self.maps,
            );
            result = result.or(domain, &mut self.maps);
            if result.default && result.exceptions == 0 {
                break;
            }
        }
        state.parents.insert((plan, state.removed), result);
        result
    }
    fn candidate_keys(
        &mut self,
        set: SetInfo,
        implicit: bool,
        value: &Value,
        state: &mut ObjectState,
    ) -> Keys {
        if set.count <= 1 {
            return Keys::NONE;
        }
        if let Some(keys) = state
            .candidates
            .get(&(set.candidates, implicit, state.removed))
        {
            return *keys;
        }
        let mut result = Keys::NONE;
        for (plan, _) in self.maps.entries(set.candidates) {
            let info = self.infos[plan];
            if info.complex {
                continue;
            }
            let accepted = if !implicit && info.pattern {
                true
            } else {
                !info.forbids && self.matches(plan, value, state)
            };
            if accepted {
                let domain = if implicit {
                    Keys::ALL
                } else {
                    let mask = self.maps.mask(info.properties);
                    Keys {
                        default: true,
                        exceptions: mask,
                    }
                };
                result = result.or(domain, &mut self.maps);
                if result.default && result.exceptions == 0 {
                    break;
                }
            }
        }
        state
            .candidates
            .insert((set.candidates, implicit, state.removed), result);
        result
    }
    fn allowance(&mut self, set: SetInfo) -> usize {
        if let Some(value) = self.allowances.get(&set.merged) {
            return *value;
        }
        let branches = self.infos[set.merged].branches;
        let mut plans = branches;
        if set.count > 1 {
            for (candidate, _) in self.maps.entries(set.candidates) {
                if !self.infos[candidate].complex {
                    let one = self.maps.singleton(candidate, 1);
                    plans = self.maps.merge(Op::Union, plans, one);
                }
            }
        }
        let amount = self
            .maps
            .entries(plans)
            .iter()
            .map(|(plan, _)| self.maps.len(self.infos[*plan].required))
            .sum();
        self.allowances.insert(set.merged, amount);
        amount
    }
    pub(super) fn translated_children(&mut self, group: Map, key: usize) -> Map {
        let info = self.set_info(group, 0);
        self.maps
            .get(self.infos[info.merged].properties, key)
            .unwrap_or(0)
    }
    fn permit(&mut self, set: SetInfo, key: usize, value: &Value, state: &mut ObjectState) -> bool {
        if value.as_object().is_some_and(|v| v.len() == 1) {
            return self.infos[set.merged]
                .parent_probe
                .contains(&self.maps, key)
                || (set.count > 1 && set.candidate_probe.contains(&self.maps, key));
        }
        self.parent_keys(set.merged, value, state)
            .contains(&self.maps, key)
            || self
                .candidate_keys(set, false, value, state)
                .contains(&self.maps, key)
    }
    fn implicit_permit(
        &mut self,
        set: SetInfo,
        key: usize,
        value: &Value,
        state: &mut ObjectState,
    ) -> bool {
        if value.as_object().is_some_and(|v| v.len() == 1) {
            return set.count > 1 && set.implicit_probe.contains(&self.maps, key);
        }
        self.candidate_keys(set, true, value, state)
            .contains(&self.maps, key)
    }
    pub(super) fn deep_for(&mut self, group: Map, key: usize) -> bool {
        if let Some(value) = self.deep_sets.get(&(group, key)) {
            return *value;
        }
        let mut result = false;
        for (_, node) in self.maps.entries(group) {
            if self.deep_declares(node, self.original, key) {
                result = true;
                break;
            }
        }
        self.deep_sets.insert((group, key), result);
        result
    }
    pub(super) fn injected_for(
        &mut self,
        group: Map,
        key: usize,
        top: bool,
        forbids: bool,
    ) -> bool {
        if let Some(value) = self.injection_sets.get(&(group, key, top, forbids)) {
            return *value;
        }
        let mut result = false;
        for (_, node) in self.maps.entries(group) {
            if self.injection(node, key, top) || (top && forbids && self.requires(node, key)) {
                result = true;
                break;
            }
        }
        self.injection_sets
            .insert((group, key, top, forbids), result);
        result
    }
    fn clean_value(
        &mut self,
        value: &mut Value,
        originals: Map,
        translated: Map,
        top: bool,
        depth: usize,
    ) {
        if depth > 128 || originals == 0 || !budget::node(value) {
            return;
        }
        let set = self.set_info(originals, self.original);
        let info = self.infos[set.merged];
        if info.unsafe_complex {
            return;
        }
        if let Some(values) = value.as_array_mut() {
            let Some(original_items) = self.items_for(originals, self.original) else {
                return;
            };
            let Some(translated_items) = self.items_for(translated, 0) else {
                return;
            };
            if translated != 0 && translated_items == 0 {
                return;
            }
            for value in values {
                self.clean_value(value, original_items, translated_items, false, depth + 1);
            }
            return;
        }
        if !value.is_object() || (translated != 0 && !set.all_objects) {
            return;
        }
        if translated == 0 {
            if info.complex || !info.object {
                return;
            }
        } else if info.complex && !set.has_union {
            return;
        }
        let mut state = ObjectState::default();
        if value.as_object().unwrap().len() > 1 {
            let allowance = self.allowance(set);
            budget::predicate_object(allowance);
        }
        let keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        for text in keys {
            if !budget::charge(8) {
                return;
            }
            let Some(key) = self.name_ids.get(text.as_str()).copied() else {
                continue;
            };
            let Some(children) = self.maps.get(info.properties, key) else {
                continue;
            };
            if self.permit(set, key, value, &mut state) {
                continue;
            }
            let translated_children = self.translated_children(translated, key);
            if translated != 0 && translated_children == 0 {
                continue;
            }
            self.clean_value(
                value.get_mut(&text).unwrap(),
                children,
                translated_children,
                false,
                depth + 1,
            );
        }
        let dynamic = info.dynamic || set.source_dynamic;
        for (bit, text) in [(1, "reason"), (2, "_")] {
            if value.get(text).is_none() {
                continue;
            }
            let key = self.intern(text);
            if self.maps.get(info.properties, key).is_some()
                || self.maps.get(info.required, key).is_some()
                || dynamic
                || self.permit(set, key, value, &mut state)
                || self.implicit_permit(set, key, value, &mut state)
                || (translated != 0 && self.deep_for(originals, key))
            {
                continue;
            }
            if translated == 0 || self.injected_for(translated, key, top, info.forbids) {
                value.as_object_mut().unwrap().shift_remove(text);
                state.removed |= bit;
            }
        }
        if !info.complex
            && info.forbids
            && set.schema_objects
            && info.required == 0
            && info.properties == 0
            && !dynamic
        {
            let key = usize::MAX;
            if !self.implicit_permit(set, key, value, &mut state) {
                *value = json!({});
            }
        }
    }
}
