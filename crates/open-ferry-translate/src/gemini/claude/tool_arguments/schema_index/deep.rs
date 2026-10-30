//! Placeholder declaration is existential reachability, bounded to 16 edges.
//! A shortest path cannot repeat a reference target, so its result equals the
//! legacy visited-reference walk. Compute both keys for each reachable node,
//! rather than revisiting a target for every alias's reference history.
use super::*;

#[derive(Clone)]
pub(super) struct Program {
    declared: [bool; 2],
    reference: Option<usize>,
    children: Arc<Vec<Id>>,
}

impl SchemaIndex {
    pub(super) fn build_deep_program(&mut self, node: Id) -> Program {
        let mut program = Program {
            declared: [false; 2],
            reference: None,
            children: Arc::default(),
        };
        if self.is_object(node) {
            let required = self.own_required(node);
            for (at, name) in ["reason", "_"].into_iter().enumerate() {
                let key = self.name_ids[name];
                program.declared[at] = self.maps.get(required, key).is_some()
                    || self.field(node, "properties").is_some_and(|properties| {
                        self.records[&properties].fields.contains_key(&key)
                    });
            }
            program.reference = self.field(node, "$ref").map(|id| self.spelling(id));
            let mut children = Vec::new();
            for keyword in ["allOf", "anyOf", "oneOf"] {
                if let Some(branches) = self.field(node, keyword) {
                    let branches = self.garray(branches);

                    children.extend(branches);
                }
            }
            program.children = Arc::new(children);
        }
        program
    }

    pub(super) fn deep_declares(&mut self, node: Id, scope: Id, key: usize) -> bool {
        if !self.deep.contains_key(&(node, scope)) {
            // Cached closures are sinks with already known distances. Uncached
            // nodes are expanded once, including all edges in cyclic graphs.
            let mut todo = vec![node];
            let mut seen = HashSet::new();
            let mut reverse: HashMap<Id, Vec<Id>> = HashMap::new();
            let mut distances = HashMap::new();
            while let Some(next) = todo.pop() {
                if !seen.insert(next) {
                    continue;
                }

                if let Some(found) = self.deep.get(&(next, scope)) {
                    distances.insert(next, *found);
                    continue;
                }
                let program = Arc::clone(&self.own(next, next).deep);
                distances.insert(next, program.declared.map(|has| if has { 0 } else { 17 }));
                let target = program
                    .reference
                    .and_then(|name| self.reference(scope, name));
                for child in program.children.iter().copied().chain(target) {
                    reverse.entry(child).or_default().push(next);
                    todo.push(child);
                }
            }
            // Unit-weight shortest paths, with a fixed number of buckets. A
            // node/edge is processed at most once per placeholder, not per path.
            for at in 0..2 {
                let mut buckets: [Vec<Id>; 17] = std::array::from_fn(|_| Vec::new());
                for (&id, distance) in &distances {
                    if distance[at] <= 16 {
                        buckets[usize::from(distance[at])].push(id);
                    }
                }
                for distance in 0..16 {
                    for child in std::mem::take(&mut buckets[distance]) {
                        if distances[&child][at] != distance as u8 {
                            continue;
                        }
                        if let Some(parents) = reverse.get(&child) {
                            for &parent in parents {
                                let found = &mut distances.get_mut(&parent).unwrap()[at];
                                if *found > distance as u8 + 1 {
                                    *found = distance as u8 + 1;
                                    buckets[distance + 1].push(parent);
                                }
                            }
                        }
                    }
                }
            }
            for (id, distance) in distances {
                self.deep.insert((id, scope), distance);
            }
        }
        self.deep[&(node, scope)][usize::from(&*self.names[key] == "_")] <= 16
    }
}
