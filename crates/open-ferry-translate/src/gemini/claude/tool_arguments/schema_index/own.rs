//! Parse a schema node's immediate syntax once. Guard-sensitive semantic plans
//! share these programs; neither guard depth nor ref spelling copies properties.
use super::*;
#[derive(Clone)]
pub(super) struct Own {
    pub(super) head: Info,
    pub(super) tail: Info,
    pub(super) valid: bool,
    pub(super) has_union: bool,
    pub(super) union: Option<Arc<Vec<Id>>>,
    pub(super) all: Option<Option<Arc<Vec<Id>>>>,
    pub(super) reference: Option<usize>,
    pub(super) deep: Arc<deep::Program>,
}
#[derive(Clone, Copy)]
pub(super) struct Shape {
    pub(super) depth: usize,
    pub(super) references: Map,
    pub(super) items_safe: bool,
}
impl SchemaIndex {
    pub(super) fn own(&mut self, node: Id, _origin: Id) -> Arc<Own> {
        let key = node;
        if let Some(own) = self.own_nodes.get(&key) {
            return Arc::clone(own);
        }

        let mut own = Own {
            head: Info::default(),
            tail: Info::default(),
            valid: false,
            has_union: false,
            union: None,
            all: None,
            reference: None,
            deep: Arc::new(self.build_deep_program(node)),
        };
        if let Some(fields) = self.object(node) {
            own.valid = true;

            own.head.object = self.field(node, "type").is_some_and(|id| {
                self.is_string(id, "object")
                    || self
                        .array(id)
                        .is_some_and(|v| v.into_iter().any(|v| self.is_string(v, "object")))
            });
            const KNOWN: &str = "type description title properties required items additionalProperties patternProperties anyOf oneOf allOf $ref $defs definitions nullable minLength maxLength minimum maximum exclusiveMinimum exclusiveMaximum multipleOf pattern format default examples minItems maxItems maxProperties $schema $id id $comment deprecated readOnly writeOnly example contentEncoding contentMediaType";
            own.head.unsafe_complex = fields.iter().any(|(key, _)| {
                !self.names[*key].starts_with("x-")
                    && !KNOWN
                        .split_whitespace()
                        .any(|known| known == &*self.names[*key])
            });
            if own.head.unsafe_complex {
                own.head.complex = true;
            } else {
                own.has_union =
                    self.field(node, "anyOf").is_some() || self.field(node, "oneOf").is_some();
                own.union = self.branches(node).filter(|v| !v.is_empty()).map(Arc::new);
                own.reference = self.field(node, "$ref").map(|raw| self.spelling(raw));
                own.all = self
                    .field(node, "allOf")
                    .map(|id| self.array(id).map(Arc::new));
                own.tail.pattern = self
                    .field(node, "patternProperties")
                    .is_some_and(|id| self.object(id).is_some_and(|v| !v.is_empty()));
                own.tail.allows = own.tail.pattern
                    || self
                        .field(node, "additionalProperties")
                        .is_some_and(|id| !self.truth_false(id));
                own.tail.forbids = self
                    .field(node, "additionalProperties")
                    .is_some_and(|id| self.truth_false(id));
                if let Some(properties) = self.field(node, "properties") {
                    if let Some(values) = self.object(properties) {
                        own.tail.object = true;
                        for (name, value) in values {
                            let group = self.singleton(value);
                            let one = self.maps.singleton(name, group);
                            own.tail.properties =
                                self.maps.merge(Op::Properties, own.tail.properties, one);
                        }
                    } else {
                        own.tail.complex = true;
                    }
                }
                own.tail.required = self.own_required(node);
            }
        }
        let own = Arc::new(own);
        self.own_nodes.insert(key, Arc::clone(&own));
        own
    }
    pub(super) fn shape(
        &mut self,
        node: Id,
        scope: Id,
        visiting: &mut HashSet<Id>,
    ) -> Option<Shape> {
        if let Some(shape) = self.graph_shapes.get(&(node, scope)) {
            return *shape;
        }
        if visiting.len() > 16 || !visiting.insert(node) {
            return None;
        }
        let own = self.own(node, node);
        let result = (|| {
            let mut result = Shape {
                depth: 0,
                references: 0,
                items_safe: !own.head.unsafe_complex,
            };
            if !own.valid || own.head.unsafe_complex || (own.has_union && own.union.is_none()) {
                return Some(result);
            }
            let mut children = Vec::new();
            if let Some(branches) = &own.union {
                children.extend(branches.iter().copied());
            }
            if let Some(Some(all)) = &own.all {
                children.extend(all.iter().copied());
            }
            if let Some(name) = own.reference {
                let one = self.maps.singleton(name, 1);
                result.references = self.maps.merge(Op::Union, result.references, one);
                if let Some(target) = self.reference(scope, name) {
                    children.push(target);
                }
            }
            for child in children {
                let next = self.shape(child, scope, visiting)?;
                result.items_safe &= next.items_safe;
                result.depth = result.depth.max(next.depth + 1);
                result.references = self
                    .maps
                    .merge(Op::Union, result.references, next.references);
            }
            Some(result)
        })();
        visiting.remove(&node);
        self.graph_shapes.insert((node, scope), result);
        result
    }
}
