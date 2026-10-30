use super::*;
impl SchemaIndex {
    fn alias(&self, node: Id) -> bool {
        self.field(node, "$ref").is_some()
            && self.object(node).is_some_and(|fields| {
                fields.iter().all(|(key, _)| {
                    matches!(
                        &*self.names[*key],
                        "$ref" | "$defs" | "definitions" | "description" | "title"
                    ) || self.names[*key].starts_with("x-")
                })
            })
    }
    pub(super) fn info(
        &mut self,
        mut node: Id,
        scope: Id,
        mut depth: usize,
        mut active: Map,
    ) -> usize {
        let origin = node;
        while depth <= 16 && self.alias(node) {
            let Some(raw) = self.field(node, "$ref") else {
                unreachable!()
            };
            let name = self.spelling(raw);
            if self.maps.get(active, name).is_some() {
                return 0;
            }
            let Some(target) = self.reference(scope, name) else {
                return 0;
            };
            let one = self.maps.singleton(name, 1);
            active = self.maps.merge(Op::Union, active, one);
            node = target;
            depth += 1;
        }
        if depth > 16 || self.failed {
            return 0;
        }
        let cache_node = node;
        let key = (cache_node, scope, depth, active);
        if let Some(id) = self.info_ids.get(&key) {
            return *id;
        }
        budget::analyzed(node, scope, depth, active);
        let _ = origin;
        let Some(fields) = self.object(node) else {
            self.info_ids.insert(key, 0);
            return 0;
        };
        if !budget::charge(32 + fields.len() * 8) {
            return 0;
        }
        let mut out = Info::default();
        const KNOWN: &str = "type description title properties required items additionalProperties patternProperties anyOf oneOf allOf $ref $defs definitions nullable minLength maxLength minimum maximum exclusiveMinimum exclusiveMaximum multipleOf pattern format default examples minItems maxItems maxProperties $schema $id id $comment deprecated readOnly writeOnly example contentEncoding contentMediaType";
        let unsafe_complex = fields.iter().any(|(key, _)| {
            !self.names[*key].starts_with("x-")
                && !KNOWN
                    .split_whitespace()
                    .any(|known| known == &*self.names[*key])
        });
        if unsafe_complex {
            let id = self.add_info(Info {
                complex: true,
                unsafe_complex: true,
                ..out
            });
            self.info_ids.insert(key, id);
            return id;
        }
        out.object = self.field(node, "type").is_some_and(|id| {
            self.is_string(id, "object")
                || self
                    .array(id)
                    .is_some_and(|v| v.into_iter().any(|v| self.is_string(v, "object")))
        });
        let has_union = self.field(node, "anyOf").is_some() || self.field(node, "oneOf").is_some();
        let mut multiple = false;
        if has_union {
            let Some(branches) = self.branches(node).filter(|v| !v.is_empty()) else {
                out.complex = true;
                let complete = self.complete_info(out);
                let id = self.add_info(complete);
                self.info_ids.insert(key, id);
                return id;
            };
            multiple = branches.len() > 1;
            for branch in branches {
                let branch = self.info(branch, scope, depth + 1, active);
                let info = self.infos[branch];
                if multiple {
                    let one = self.maps.singleton(branch, 1);
                    out.branches = self.maps.merge(Op::Union, out.branches, one);
                }
                out = self.union_info(out, info);
            }
            out.complex |= multiple;
        }
        if let Some(raw) = self.field(node, "$ref") {
            let name = self.spelling(raw);
            let target = if self.maps.get(active, name).is_none() {
                self.reference(scope, name)
            } else {
                None
            };
            if let Some(target) = target {
                let one = self.maps.singleton(name, 1);
                let visited = self.maps.merge(Op::Union, active, one);
                let target = self.info(target, scope, depth + 1, visited);
                out = self.union_info(out, self.infos[target]);
            } else if !multiple {
                out.complex = true;
                let complete = self.complete_info(out);
                let id = self.add_info(complete);
                self.info_ids.insert(key, id);
                return id;
            }
        }
        if let Some(all) = self.field(node, "allOf") {
            if let Some(values) = self.array(all) {
                for value in values {
                    let child = self.info(value, scope, depth + 1, active);
                    out = self.union_info(out, self.infos[child]);
                }
            } else {
                out.complex = true;
            }
        }
        out.pattern |= self
            .field(node, "patternProperties")
            .is_some_and(|id| self.object(id).is_some_and(|v| !v.is_empty()));
        out.allows |= out.pattern
            || self
                .field(node, "additionalProperties")
                .is_some_and(|id| !self.truth_false(id));
        out.forbids |= self
            .field(node, "additionalProperties")
            .is_some_and(|id| self.truth_false(id));
        if let Some(properties) = self.field(node, "properties") {
            if let Some(values) = self.object(properties) {
                out.object = true;
                for (name, value) in values {
                    if !budget::charge(32) {
                        return 0;
                    }
                    let group = self.singleton(value);
                    let one = self.maps.singleton(name, group);
                    out.properties = self.maps.merge(Op::Properties, out.properties, one);
                }
            } else {
                out.complex = true;
            }
        }
        let required = self.own_required(node);
        out.required = self.maps.merge(Op::Union, out.required, required);
        let complete = self.complete_info(out);
        let id = self.add_info(complete);
        self.info_ids.insert(key, id);
        id
    }
    pub(super) fn set_info(&mut self, group: Map, scope: Id) -> SetInfo {
        if let Some(result) = self.sets.get(&(group, scope)) {
            return *result;
        }
        let mut merged = Info::default();
        let mut candidates = 0;
        let mut all_objects = true;
        let mut schema_objects = true;
        let mut has_union = false;
        let mut source_dynamic = false;
        let count = self.maps.len(group);
        let mut candidate_probe = Keys::NONE;
        let mut implicit_probe = Keys::NONE;
        for (_, node) in self.maps.entries(group) {
            let info = self.info(node, if scope == 0 { node } else { scope }, 0, 0);
            let entry = self.infos[info];
            source_dynamic |= entry.dynamic;
            all_objects &= self.is_object(node);
            schema_objects &= entry.object;
            has_union |= ["anyOf", "oneOf"].into_iter().any(|key| {
                self.field(node, key)
                    .is_some_and(|id| self.array(id).is_some())
            });
            merged = self.union_info(merged, entry);
            let one = self.maps.singleton(info, 1);
            candidates = self.maps.merge(Op::Union, candidates, one);
            if !entry.complex {
                let own = if entry.pattern {
                    Keys::ALL
                } else if entry.forbids {
                    Keys::NONE
                } else {
                    self.required_singleton(entry.required)
                };
                let mask = self.maps.mask(entry.properties);
                let own = own.and(
                    Keys {
                        default: true,
                        exceptions: mask,
                    },
                    &mut self.maps,
                );
                candidate_probe = candidate_probe.or(own, &mut self.maps);
                let implicit = if entry.forbids {
                    Keys::NONE
                } else {
                    self.required_singleton(entry.required)
                };
                implicit_probe = implicit_probe.or(implicit, &mut self.maps);
            }
        }
        let complete = self.complete_info(merged);
        let merged = self.add_info(complete);
        let result = SetInfo {
            merged,
            candidates,
            count,
            all_objects,
            has_union,
            candidate_probe,
            implicit_probe,
            schema_objects,
            source_dynamic,
        };
        self.sets.insert((group, scope), result);
        result
    }
    fn array_items(&mut self, node: Id, scope: Id, depth: usize, active: Map) -> Option<Map> {
        if depth > 16 || !self.is_object(node) {
            return None;
        }
        let key = (node, scope, depth, active);
        if let Some(result) = self.items.get(&key) {
            return *result;
        }
        let result = (|| {
            let mut out = 0;
            if let Some(raw) = self.field(node, "$ref") {
                let name = self.spelling(raw);
                if self.maps.get(active, name).is_some() {
                    return None;
                }
                let target = self.reference(scope, name)?;
                let one = self.maps.singleton(name, 1);
                let visited = self.maps.merge(Op::Union, active, one);
                let found = self.array_items(target, scope, depth + 1, visited)?;
                out = self.maps.merge(Op::Union, out, found);
            }
            if self.field(node, "anyOf").is_some() || self.field(node, "oneOf").is_some() {
                let branches = self.branches(node)?;
                if branches.len() != 1 {
                    return None;
                }
                let found = self.array_items(branches[0], scope, depth + 1, active)?;
                out = self.maps.merge(Op::Union, out, found);
            }
            if let Some(all) = self.field(node, "allOf") {
                for branch in self.array(all)? {
                    let found = self.array_items(branch, scope, depth + 1, active)?;
                    out = self.maps.merge(Op::Union, out, found);
                }
            }
            if let Some(item) = self.field(node, "items") {
                let one = self.singleton(item);
                out = self.maps.merge(Op::Union, out, one);
            }
            Some(out)
        })();
        self.items.insert(key, result);
        result
    }
    pub(super) fn items_for(&mut self, group: Map, scope: Id) -> Option<Map> {
        if let Some(found) = self.item_sets.get(&(group, scope)) {
            return *found;
        }
        let result = (|| {
            let mut out = 0;
            for (_, node) in self.maps.entries(group) {
                let found = self.array_items(node, if scope == 0 { node } else { scope }, 0, 0)?;
                let found = if scope != 0 && found == 0 {
                    self.singleton(node)
                } else {
                    found
                };
                out = self.maps.merge(Op::Union, out, found);
            }
            Some(out)
        })();
        self.item_sets.insert((group, scope), result);
        result
    }
    pub(super) fn deep_declares(
        &mut self,
        node: Id,
        scope: Id,
        key: usize,
        depth: usize,
        active: Map,
    ) -> bool {
        if depth > 16 || !self.is_object(node) {
            return false;
        }
        let cache = (node, scope, depth, active, key);
        if let Some(found) = self.deep.get(&cache) {
            return *found;
        }
        let result = (|| {
            if let Some(raw) = self.field(node, "$ref") {
                let name = self.spelling(raw);
                if self.maps.get(active, name).is_none() {
                    if let Some(target) = self.reference(scope, name) {
                        let one = self.maps.singleton(name, 1);
                        let visited = self.maps.merge(Op::Union, active, one);
                        if self.deep_declares(target, scope, key, depth + 1, visited) {
                            return true;
                        }
                    }
                }
            }
            if self
                .field(node, "properties")
                .is_some_and(|id| self.records[&id].fields.contains_key(&key))
            {
                return true;
            }
            let required = self.own_required(node);
            if self.maps.get(required, key).is_some() {
                return true;
            }
            for keyword in ["allOf", "anyOf", "oneOf"] {
                if let Some(branches) = self.field(node, keyword) {
                    for branch in self.garray(branches) {
                        if self.deep_declares(branch, scope, key, depth + 1, active) {
                            return true;
                        }
                    }
                }
            }
            false
        })();
        self.deep.insert(cache, result);
        result
    }
    pub(super) fn injection(&mut self, node: Id, key: usize, top: bool) -> bool {
        if let Some(value) = self.injected.get(&(node, key, top)) {
            return *value;
        }
        let plan = self.info(node, node, 0, 0);
        let info = self.infos[plan];
        let property = self.maps.get(info.properties, key);
        let mut result = false;
        if let Some(children) = property {
            let required = self.own_required(node);
            result = self.maps.get(required, key).is_some();
            if !result {
                for (_, child) in self.maps.entries(children) {
                    if &*self.names[key] == "reason"
                        && self.field(child, "description").is_some_and(|id| {
                            self.is_string(id, "Brief explanation of why you are calling this tool")
                        })
                    {
                        result = true;
                        break;
                    }
                    if &*self.names[key] == "_"
                        && !top
                        && self
                            .field(child, "type")
                            .is_some_and(|id| self.is_string(id, "boolean"))
                    {
                        result = true;
                        break;
                    }
                }
            }
        }
        if !result {
            for keyword in ["anyOf", "oneOf"] {
                if let Some(list) = self.field(node, keyword).and_then(|id| self.array(id)) {
                    for branch in list {
                        let plan = self.info(branch, branch, 0, 0);
                        let own = self.own_required(branch);
                        if self.maps.get(self.infos[plan].properties, key).is_some()
                            && self.maps.get(own, key).is_some()
                        {
                            result = true;
                            break;
                        }
                    }
                }
            }
        }
        self.injected.insert((node, key, top), result);
        result
    }
    pub(super) fn requires(&mut self, node: Id, key: usize) -> bool {
        if let Some(result) = self.reclean.get(&(node, key)) {
            return *result;
        }
        let raw = self.restore(node);
        let result = reclean::requires(&raw, &self.names[key]);
        self.reclean.insert((node, key), result);
        result
    }
}
