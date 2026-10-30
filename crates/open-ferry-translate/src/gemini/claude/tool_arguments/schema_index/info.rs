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
        let own = self.own(node, origin);
        let shape = self.shape(node, scope, &mut HashSet::new());
        let cache_node = node;
        let cache_depth = if shape.is_some_and(|shape| depth + shape.depth <= 16) {
            0
        } else {
            depth
        };
        let cache_active = if let Some(shape) = shape {
            self.maps.merge(Op::Intersection, active, shape.references)
        } else {
            active
        };
        let key = (cache_node, scope, cache_depth, cache_active);
        if let Some(id) = self.info_ids.get(&key) {
            return *id;
        }
        if !own.valid {
            self.info_ids.insert(key, 0);
            return 0;
        }
        let mut out = own.head;
        if out.unsafe_complex {
            let complete = self.complete_info(out);
            let id = self.add_info(complete);
            self.info_ids.insert(key, id);
            return id;
        }
        let has_union = own.has_union;
        let mut multiple = false;
        if has_union {
            let Some(branches) = own.union.as_ref() else {
                out.complex = true;
                let complete = self.complete_info(out);
                let id = self.add_info(complete);
                self.info_ids.insert(key, id);
                return id;
            };
            multiple = branches.len() > 1;
            for &branch in branches.iter() {
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
        if let Some(name) = own.reference {
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
        if let Some(all) = &own.all {
            if let Some(values) = all {
                for &value in values.iter() {
                    let child = self.info(value, scope, depth + 1, active);
                    out = self.union_info(out, self.infos[child]);
                }
            } else {
                out.complex = true;
            }
        }
        out = self.union_info(out, own.tail);
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
    fn array_items(
        &mut self,
        mut node: Id,
        scope: Id,
        mut depth: usize,
        mut active: Map,
    ) -> Option<Map> {
        while depth <= 16 && self.alias(node) {
            let raw = self.field(node, "$ref")?;
            let name = self.spelling(raw);
            if self.maps.get(active, name).is_some() {
                return None;
            }
            node = self.reference(scope, name)?;
            let one = self.maps.singleton(name, 1);
            active = self.maps.merge(Op::Union, active, one);
            depth += 1;
        }
        if depth > 16 || !self.is_object(node) {
            return None;
        }
        let shape = self
            .shape(node, scope, &mut HashSet::new())
            .filter(|shape| shape.items_safe);
        let cache_depth = if shape.is_some_and(|shape| depth + shape.depth <= 16) {
            0
        } else {
            depth
        };
        let cache_active = if let Some(shape) = shape {
            self.maps.merge(Op::Intersection, active, shape.references)
        } else {
            active
        };
        let key = (node, scope, cache_depth, cache_active);
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
