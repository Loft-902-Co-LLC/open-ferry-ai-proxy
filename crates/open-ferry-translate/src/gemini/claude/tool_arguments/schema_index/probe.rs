//! Incremental singleton-probe decisions. The assembler carries this state at
//! each argument node; no schema or argument-prefix Value is reconstructed.
use super::*;
use crate::common::vertex_partial_args::ProbeStep;
impl SchemaIndex {
    pub(super) fn probe_id(&mut self, state: ProbeState) -> usize {
        if let Some(id) = self.probe_ids.get(&state) {
            return *id;
        }
        let id = self.probe_states.len();
        self.probe_states.push(state);
        self.probe_ids.insert(state, id);
        id
    }
    fn object_enabled(&self, set: SetInfo, translated: Map) -> bool {
        let info = self.infos[set.merged];
        !info.unsafe_complex
            && if translated == 0 {
                !info.complex && info.object
            } else {
                set.all_objects && (!info.complex || set.has_union)
            }
    }
    fn removed_key(
        &mut self,
        set: SetInfo,
        originals: Map,
        translated: Map,
        key: usize,
        top: bool,
    ) -> bool {
        let info = self.infos[set.merged];
        let dynamic = info.dynamic || set.source_dynamic;
        let implicit = set.count > 1 && set.implicit_probe.contains(&self.maps, key);
        let whole = !info.complex
            && info.forbids
            && set.schema_objects
            && info.required == 0
            && info.properties == 0
            && !dynamic
            && !implicit;
        if whole {
            return true;
        }
        if !matches!(self.names.get(key).map(|s| &**s), Some("reason" | "_")) {
            return false;
        }
        if self.maps.get(info.properties, key).is_some()
            || self.maps.get(info.required, key).is_some()
            || dynamic
            || info.parent_probe.contains(&self.maps, key)
            || (set.count > 1 && set.candidate_probe.contains(&self.maps, key))
            || implicit
            || (translated != 0 && self.deep_for(originals, key))
        {
            return false;
        }
        translated == 0 || self.injected_for(translated, key, top, info.forbids)
    }
    pub(crate) fn probe(&mut self, parent: usize, step: ProbeStep<'_>) -> (bool, usize) {
        if !budget::charge(8) {
            return (false, 2);
        }
        let state = self.probe_states[parent];
        let index = matches!(step, ProbeStep::Index);
        match state {
            ProbeState::Removed => return (!index, 1),
            ProbeState::Opaque => return (false, 2),
            _ => {}
        }
        let ProbeState::Clean(originals, translated, top) = state else {
            unreachable!()
        };
        if index && top {
            return (false, 2);
        }
        let key = match step {
            ProbeStep::Key(text) => {
                if !budget::charge(text.len()) {
                    return (false, 2);
                }
                self.name_ids.get(text).copied().unwrap_or(usize::MAX)
            }
            ProbeStep::Index => usize::MAX - 1,
        };
        if let Some(result) = self.probe_edges.get(&(parent, key)) {
            return *result;
        }
        let set = self.set_info(originals, self.original);
        let info = self.infos[set.merged];
        let result = if index {
            if info.unsafe_complex {
                (false, 2)
            } else {
                match (
                    self.items_for(originals, self.original),
                    self.items_for(translated, 0),
                ) {
                    (Some(original_items), Some(translated_items))
                        if translated == 0 || translated_items != 0 =>
                    {
                        let next = self.probe_id(ProbeState::Clean(
                            original_items,
                            translated_items,
                            false,
                        ));
                        (false, next)
                    }
                    _ => (false, 2),
                }
            }
        } else if !self.object_enabled(set, translated) {
            (false, 2)
        } else {
            let probe_key = if matches!(self.names.get(key).map(|s| &**s), Some("reason" | "_")) {
                key
            } else {
                self.name_ids
                    .get("__cpa_probe__")
                    .copied()
                    .unwrap_or(usize::MAX)
            };
            let held = self.removed_key(set, originals, translated, probe_key, top);
            let next = if self.removed_key(set, originals, translated, key, top) {
                1
            } else if info.parent_probe.contains(&self.maps, key)
                || (set.count > 1 && set.candidate_probe.contains(&self.maps, key))
            {
                2
            } else if let Some(children) = self.maps.get(info.properties, key) {
                let translated_children = self.translated_children(translated, key);
                if translated != 0 && translated_children == 0 {
                    2
                } else {
                    self.probe_id(ProbeState::Clean(children, translated_children, false))
                }
            } else {
                2
            };
            (held, next)
        };
        self.probe_edges.insert((parent, key), result);
        result
    }
}
