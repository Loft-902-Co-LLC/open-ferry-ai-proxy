//! A call-owned, shallow schema graph and shared decision index.
use super::*;
mod clean;
mod deep;
mod info;
mod own;
mod probe;
mod trie;
use trie::{Keys, Map, Maps, Op};
type Id = usize;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum ProbeState {
    Clean(Map, Map, bool),
    Removed,
    Opaque,
}
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
enum Raw {
    Null,
    Bool(bool),
    Number(usize),
    String(usize),
    Array(Vec<Id>),
    Object(Vec<(usize, Id)>),
}
#[derive(Clone)]
struct Record {
    raw: Raw,
    fields: HashMap<usize, Id>,
    canonical: usize,
}
#[derive(Clone, Copy, Default, Hash, Eq, PartialEq)]
struct Info {
    properties: Map,
    required: Map,
    branches: Map,
    object: bool,
    allows: bool,
    forbids: bool,
    pattern: bool,
    complex: bool,
    unsafe_complex: bool,
    dynamic: bool,
    parent_probe: Keys,
    branch_probe: Keys,
    parent_static: Option<Keys>,
    branch_static: Option<Keys>,
}
#[derive(Clone, Copy)]
struct BranchSummary {
    dynamic: bool,
    parent: Keys,
    static_parent: Option<Keys>,
}
#[derive(Clone, Copy, Default)]
struct SetInfo {
    merged: usize,
    candidates: Map,
    count: usize,
    all_objects: bool,
    has_union: bool,
    schema_objects: bool,
    source_dynamic: bool,
    candidate_probe: Keys,
    implicit_probe: Keys,
}
#[derive(Default)]
struct ObjectState {
    present: Option<HashSet<usize>>,
    matches: HashMap<usize, bool>,
    parents: HashMap<(usize, u8), Keys>,
    candidates: HashMap<(Map, bool, u8), Keys>,
    removed: u8,
}

pub(super) struct SchemaIndex {
    records: HashMap<Id, Record>,
    shapes: HashMap<Raw, usize>,
    names: Vec<Arc<str>>,
    name_ids: HashMap<Arc<str>, usize>,
    maps: Maps,
    infos: Vec<Info>,
    info_ids: HashMap<(Id, Id, usize, Map), usize>,
    own_nodes: HashMap<Id, Arc<own::Own>>,
    graph_shapes: HashMap<(Id, Id), Option<own::Shape>>,
    info_intern: HashMap<Info, usize>,
    branch_summaries: HashMap<Map, BranchSummary>,
    required_cache: HashMap<Id, Map>,
    references: HashMap<(Id, usize), Option<Id>>,
    sets: HashMap<(Map, Id), SetInfo>,
    items: HashMap<(Id, Id, usize, Map), Option<Map>>,
    item_sets: HashMap<(Map, Id), Option<Map>>,
    deep: HashMap<(Id, Id), [u8; 2]>,
    injected: HashMap<(Id, usize, bool), bool>,
    reclean: HashMap<(Id, usize), bool>,
    original: Id,
    original_group: Map,
    translated: Map,
    failed: bool,
    probe_states: Vec<ProbeState>,
    probe_ids: HashMap<ProbeState, usize>,
    probe_edges: HashMap<(usize, usize), (bool, usize)>,
    allowances: HashMap<usize, usize>,
    deep_sets: HashMap<(Map, usize), bool>,
    injection_sets: HashMap<(Map, usize, bool, bool), bool>,
}
impl SchemaIndex {
    pub(super) fn new(original: &Value, translated: &[Value]) -> Self {
        let mut result = Self {
            records: HashMap::new(),
            shapes: HashMap::new(),
            names: Vec::new(),
            name_ids: HashMap::new(),
            maps: Maps::new(),
            infos: vec![Info {
                complex: true,
                branch_probe: Keys::ALL,
                parent_static: Some(Keys::NONE),
                branch_static: Some(Keys::ALL),
                ..Info::default()
            }],
            info_ids: HashMap::new(),
            own_nodes: HashMap::new(),
            graph_shapes: HashMap::new(),
            info_intern: HashMap::new(),
            branch_summaries: HashMap::new(),
            required_cache: HashMap::new(),
            references: HashMap::new(),
            sets: HashMap::new(),
            items: HashMap::new(),
            item_sets: HashMap::new(),
            deep: HashMap::new(),
            injected: HashMap::new(),
            reclean: HashMap::new(),
            original: 0,
            original_group: 0,
            translated: 0,
            failed: false,
            probe_states: Vec::new(),
            probe_ids: HashMap::new(),
            probe_edges: HashMap::new(),
            allowances: HashMap::new(),
            deep_sets: HashMap::new(),
            injection_sets: HashMap::new(),
        };
        for name in ["", "reason", "_", "__cpa_probe__"] {
            result.intern(name);
        }
        result.records.insert(
            0,
            Record {
                raw: Raw::Null,
                fields: HashMap::new(),
                canonical: 0,
            },
        );
        result.original = result.ingest(original);
        result.original_group = result.singleton(result.original);
        for value in translated {
            let id = result.ingest(value);
            let one = result.singleton(id);
            result.translated = result.maps.merge(Op::Union, result.translated, one);
        }
        for state in [
            ProbeState::Clean(result.original_group, result.translated, true),
            ProbeState::Removed,
            ProbeState::Opaque,
        ] {
            result.probe_id(state);
        }
        result
    }
    fn intern(&mut self, text: &str) -> usize {
        if let Some(id) = self.name_ids.get(text) {
            return *id;
        }
        if !budget::charge(text.len().saturating_add(16)) {
            self.failed = true;
            return 0;
        }
        let id = self.names.len();
        let value: Arc<str> = text.into();
        self.names.push(Arc::clone(&value));
        self.name_ids.insert(value, id);
        id
    }
    fn ingest(&mut self, value: &Value) -> Id {
        let id = std::ptr::from_ref(value) as usize;
        if self.records.contains_key(&id) {
            return id;
        }
        let entries = match value {
            Value::Array(a) => a.len(),
            Value::Object(o) => o.len(),
            _ => 0,
        };
        if !budget::charge(32 + entries.saturating_mul(32)) {
            self.failed = true;
            return 0;
        }
        let raw = match value {
            Value::Null => Raw::Null,
            Value::Bool(v) => Raw::Bool(*v),
            Value::Number(v) => Raw::Number(self.intern(v.as_str())),
            Value::String(v) => Raw::String(self.intern(v)),
            Value::Array(v) => Raw::Array(v.iter().map(|v| self.ingest(v)).collect()),
            Value::Object(v) => Raw::Object(
                v.iter()
                    .map(|(k, v)| {
                        let k = self.intern(k);
                        (k, self.ingest(v))
                    })
                    .collect(),
            ),
        };
        let mut shape = raw.clone();
        match &mut shape {
            Raw::Array(v) => {
                for child in v {
                    *child = self.records[child].canonical;
                }
            }
            Raw::Object(v) => {
                for (_, child) in v {
                    *child = self.records[child].canonical;
                }
            }
            _ => {}
        }
        let next = self.shapes.len() + 1;
        let canonical = *self.shapes.entry(shape).or_insert(next);
        let fields = match &raw {
            Raw::Object(v) => v.iter().copied().collect(),
            _ => HashMap::new(),
        };
        self.records.insert(
            id,
            Record {
                raw,
                fields,
                canonical,
            },
        );
        id
    }
    fn singleton(&mut self, id: Id) -> Map {
        self.maps.singleton(self.records[&id].canonical, id)
    }
    fn field(&self, id: Id, key: &str) -> Option<Id> {
        self.name_ids
            .get(key)
            .and_then(|k| self.records[&id].fields.get(k))
            .copied()
    }
    fn array(&self, id: Id) -> Option<Vec<Id>> {
        if let Raw::Array(v) = &self.records[&id].raw {
            Some(v.clone())
        } else {
            None
        }
    }
    fn object(&self, id: Id) -> Option<Vec<(usize, Id)>> {
        if let Raw::Object(v) = &self.records[&id].raw {
            Some(v.clone())
        } else {
            None
        }
    }
    fn string(&self, id: Id) -> Option<&str> {
        if let Raw::String(k) = self.records[&id].raw {
            Some(&self.names[k])
        } else {
            None
        }
    }
    fn is_string(&self, id: Id, text: &str) -> bool {
        self.string(id) == Some(text)
    }
    fn is_object(&self, id: Id) -> bool {
        matches!(self.records[&id].raw, Raw::Object(_))
    }
    fn truth_false(&self, id: Id) -> bool {
        self.records[&id].raw == Raw::Bool(false)
    }
    fn garray(&self, id: Id) -> Vec<Id> {
        match &self.records[&id].raw {
            Raw::Null => Vec::new(),
            Raw::Array(v) => v.clone(),
            _ => vec![id],
        }
    }
    fn spelling(&mut self, id: Id) -> usize {
        match self.records[&id].raw {
            Raw::String(k) | Raw::Number(k) => k,
            Raw::Null => 0,
            Raw::Bool(b) => self.intern(if b { "true" } else { "false" }),
            _ => {
                let raw = self.restore(id).to_string();
                self.intern(&raw)
            }
        }
    }
    fn restore(&self, id: Id) -> Value {
        if !budget::charge(32) {
            return Value::Null;
        }
        match &self.records[&id].raw {
            Raw::Null => Value::Null,
            Raw::Bool(b) => Value::Bool(*b),
            Raw::Number(k) => {
                budget::charge(self.names[*k].len());
                crate::json::exact::from_str(&self.names[*k]).unwrap_or(Value::Null)
            }
            Raw::String(k) => {
                budget::charge(self.names[*k].len());
                Value::String(self.names[*k].to_string())
            }
            Raw::Array(v) => Value::Array(v.iter().map(|v| self.restore(*v)).collect()),
            Raw::Object(v) => Value::Object(
                v.iter()
                    .map(|(k, v)| {
                        budget::charge(self.names[*k].len());
                        (self.names[*k].to_string(), self.restore(*v))
                    })
                    .collect(),
            ),
        }
    }
    fn reference(&mut self, scope: Id, name: usize) -> Option<Id> {
        if let Some(result) = self.references.get(&(scope, name)) {
            return *result;
        }
        let text = Arc::clone(&self.names[name]);
        let result = (|| {
            if !budget::charge(text.len() + 1) {
                return None;
            }
            let mut current = scope;
            for segment in text.strip_prefix("#/")?.split('/') {
                let segment = segment.replace("~1", "/").replace("~0", "~");
                current = match &self.records[&current].raw {
                    Raw::Object(_) => *self.records[&current]
                        .fields
                        .get(self.name_ids.get(segment.as_str())?)?,
                    Raw::Array(values) => {
                        let at = usize::try_from(segment.parse::<i64>().ok()?).ok()?;
                        *values.get(at)?
                    }
                    _ => return None,
                };
            }
            Some(current)
        })();
        self.references.insert((scope, name), result);
        result
    }
    fn branches(&mut self, node: Id) -> Option<Vec<Id>> {
        let mut found = HashSet::new();
        let mut out = Vec::new();
        for keyword in ["anyOf", "oneOf"] {
            if let Some(id) = self.field(node, keyword) {
                for branch in self.array(id)? {
                    if let Some(kind) = self.field(branch, "type") {
                        if self.is_string(kind, "null")
                            || self
                                .array(kind)
                                .is_some_and(|v| v.len() == 1 && self.is_string(v[0], "null"))
                        {
                            continue;
                        }
                    }
                    if found.insert(self.records[&branch].canonical) {
                        out.push(branch);
                    }
                }
            }
        }
        Some(out)
    }
    fn union_info(&mut self, a: Info, b: Info) -> Info {
        Info {
            properties: self.maps.merge(Op::Properties, a.properties, b.properties),
            required: self.maps.merge(Op::Union, a.required, b.required),
            branches: self.maps.merge(Op::Union, a.branches, b.branches),
            object: a.object || b.object,
            allows: a.allows || b.allows,
            forbids: a.forbids || b.forbids,
            pattern: a.pattern || b.pattern,
            complex: a.complex || b.complex,
            unsafe_complex: a.unsafe_complex || b.unsafe_complex,
            ..Info::default()
        }
    }
    fn own_required(&mut self, node: Id) -> Map {
        if let Some(value) = self.required_cache.get(&node) {
            return *value;
        }
        let mut out = 0;
        if let Some(required) = self.field(node, "required") {
            for value in self.garray(required) {
                let key = self.spelling(value);
                let one = self.maps.singleton(key, 1);
                out = self.maps.merge(Op::Union, out, one);
            }
        }
        self.required_cache.insert(node, out);
        out
    }
    fn required_singleton(&self, required: Map) -> Keys {
        match self.maps.len(required) {
            0 => Keys::ALL,
            1 => Keys {
                default: false,
                exceptions: required,
            },
            _ => Keys::NONE,
        }
    }
    // Fold each persistent branch-map subtrie once. Parents that share a wide
    // target reuse its fold, including when they add their own constraints.
    fn branch_summary(&mut self, branches: Map) -> BranchSummary {
        if let Some(summary) = self.branch_summaries.get(&branches) {
            return *summary;
        }
        budget::charge(8);
        let result = match self.maps.kind(branches) {
            trie::Kind::Empty => BranchSummary {
                dynamic: false,
                parent: Keys::NONE,
                static_parent: Some(Keys::NONE),
            },
            trie::Kind::Leaf(branch, _) => {
                let child = self.infos[branch];
                let mask = self.maps.mask(child.properties);
                let undeclared = Keys {
                    default: true,
                    exceptions: mask,
                };
                BranchSummary {
                    dynamic: child.dynamic,
                    parent: child.branch_probe.and(undeclared, &mut self.maps),
                    static_parent: child
                        .branch_static
                        .map(|value| value.and(undeclared, &mut self.maps)),
                }
            }
            trie::Kind::Fork(_, left, right) => {
                let a = self.branch_summary(left);
                let b = self.branch_summary(right);
                BranchSummary {
                    dynamic: a.dynamic || b.dynamic,
                    parent: a.parent.or(b.parent, &mut self.maps),
                    static_parent: match (a.static_parent, b.static_parent) {
                        (Some(a), Some(b)) => Some(a.or(b, &mut self.maps)),
                        _ => None,
                    },
                }
            }
        };
        self.branch_summaries.insert(branches, result);
        result
    }
    fn complete_info(&mut self, mut info: Info) -> Info {
        let branches = self.branch_summary(info.branches);
        let parent = branches.parent;
        let parent_static = branches.static_parent;
        info.dynamic = info.pattern || (info.allows && !info.forbids) || branches.dynamic;
        info.parent_probe = parent;
        info.parent_static = parent_static;
        let own = if info.pattern || (!info.forbids && info.allows) {
            Keys::ALL
        } else if !info.forbids {
            self.required_singleton(info.required)
        } else {
            Keys::NONE
        };
        info.branch_probe = own.or(parent, &mut self.maps);
        let own_static = if info.pattern || (!info.forbids && (info.allows || info.required == 0)) {
            Some(Keys::ALL)
        } else if info.forbids {
            Some(Keys::NONE)
        } else {
            None
        };
        info.branch_static = match (own_static, parent_static) {
            (Some(a), _) if a.default && a.exceptions == 0 => Some(a),
            (_, Some(b)) if b.default && b.exceptions == 0 => Some(b),
            (Some(a), Some(b)) => Some(a.or(b, &mut self.maps)),
            _ => None,
        };
        info
    }
    fn add_info(&mut self, info: Info) -> usize {
        if let Some(id) = self.info_intern.get(&info) {
            return *id;
        }
        let id = self.infos.len();
        self.infos.push(info);
        self.info_intern.insert(info, id);
        id
    }
}
