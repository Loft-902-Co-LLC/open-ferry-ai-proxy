//! Hash-consed, persistent Patricia maps. Integer lookup has at most usize::BITS
//! steps. Merges reuse shared subtries, and never copy JSON schema subtrees.
use super::*;
pub(super) type Map = usize;
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
enum Kind {
    Empty,
    Leaf(usize, usize),
    Fork(u32, Map, Map),
}
#[derive(Clone, Copy)]
struct Node {
    kind: Kind,
    sample: usize,
    len: usize,
}
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub(super) enum Op {
    Union,
    Properties,
    Intersection,
    Difference,
}
#[derive(Default)]
pub(super) struct Maps {
    nodes: Vec<Node>,
    intern: HashMap<Kind, Map>,
    memo: HashMap<(Op, Map, Map), Map>,
    masks: HashMap<Map, Map>,
}
impl Maps {
    pub(super) fn new() -> Self {
        Self {
            nodes: vec![Node {
                kind: Kind::Empty,
                sample: 0,
                len: 0,
            }],
            ..Self::default()
        }
    }
    fn save(&mut self, kind: Kind) -> Map {
        if let Some(id) = self.intern.get(&kind) {
            return *id;
        }
        if !budget::charge(32) {
            return 0;
        }
        let (sample, len) = match kind {
            Kind::Empty => (0, 0),
            Kind::Leaf(k, _) => (k, 1),
            Kind::Fork(_, a, b) => (self.nodes[a].sample, self.nodes[a].len + self.nodes[b].len),
        };
        let id = self.nodes.len();
        self.nodes.push(Node { kind, sample, len });
        self.intern.insert(kind, id);
        id
    }
    pub(super) fn singleton(&mut self, key: usize, value: usize) -> Map {
        self.save(Kind::Leaf(key, value))
    }
    fn fork(&mut self, bit: u32, a: Map, b: Map) -> Map {
        if a == 0 {
            return b;
        }
        if b == 0 {
            return a;
        }
        self.save(Kind::Fork(bit, a, b))
    }
    fn split(a: usize, b: usize) -> u32 {
        usize::BITS - 1 - (a ^ b).leading_zeros()
    }
    fn join(&mut self, a: Map, b: Map) -> Map {
        let bit = Self::split(self.nodes[a].sample, self.nodes[b].sample);
        if self.nodes[a].sample & (1usize << bit) == 0 {
            self.fork(bit, a, b)
        } else {
            self.fork(bit, b, a)
        }
    }
    pub(super) fn get(&self, mut map: Map, key: usize) -> Option<usize> {
        while map != 0 {
            if !budget::charge(1) {
                return None;
            }
            match self.nodes[map].kind {
                Kind::Leaf(k, v) => return (k == key).then_some(v),
                Kind::Fork(bit, a, b) => map = if key & (1usize << bit) == 0 { a } else { b },
                Kind::Empty => return None,
            }
        }
        None
    }
    pub(super) fn len(&self, map: Map) -> usize {
        self.nodes[map].len
    }
    pub(super) fn entries(&self, map: Map) -> Vec<(usize, usize)> {
        let mut stack = vec![map];
        let mut out = Vec::new();
        while let Some(id) = stack.pop() {
            if !budget::charge(1) {
                break;
            }
            match self.nodes[id].kind {
                Kind::Leaf(k, v) => out.push((k, v)),
                Kind::Fork(_, a, b) => {
                    stack.push(b);
                    stack.push(a);
                }
                Kind::Empty => {}
            }
        }
        out
    }
    // Called only after the required-list primitive has been charged by length.
    pub(super) fn required_keys(&self, map: Map) -> Vec<usize> {
        let mut stack = vec![map];
        let mut out = Vec::new();
        while let Some(id) = stack.pop() {
            match self.nodes[id].kind {
                Kind::Leaf(key, _) => out.push(key),
                Kind::Fork(_, a, b) => {
                    stack.push(b);
                    stack.push(a);
                }
                Kind::Empty => {}
            }
        }
        out
    }
    pub(super) fn mask(&mut self, map: Map) -> Map {
        if map == 0 {
            return 0;
        }
        if let Some(id) = self.masks.get(&map) {
            return *id;
        }
        let id = match self.nodes[map].kind {
            Kind::Leaf(k, _) => self.singleton(k, 1),
            Kind::Fork(bit, a, b) => {
                let a = self.mask(a);
                let b = self.mask(b);
                self.fork(bit, a, b)
            }
            Kind::Empty => 0,
        };
        self.masks.insert(map, id);
        id
    }
    pub(super) fn merge(&mut self, op: Op, a: Map, b: Map) -> Map {
        if !budget::charge(1) {
            return 0;
        }
        if a == b {
            return if op == Op::Difference { 0 } else { a };
        }
        if a == 0 {
            return if matches!(op, Op::Union | Op::Properties) {
                b
            } else {
                0
            };
        }
        if b == 0 {
            return if op == Op::Intersection { 0 } else { a };
        }
        if let Some(id) = self.memo.get(&(op, a, b)) {
            return *id;
        }
        let na = self.nodes[a];
        let nb = self.nodes[b];
        let ba = match na.kind {
            Kind::Fork(bit, _, _) => bit as i32,
            _ => -1,
        };
        let bb = match nb.kind {
            Kind::Fork(bit, _, _) => bit as i32,
            _ => -1,
        };
        let disjoint =
            na.sample != nb.sample && Self::split(na.sample, nb.sample) as i32 > ba.max(bb);
        let result = if disjoint {
            match op {
                Op::Union | Op::Properties => self.join(a, b),
                Op::Intersection => 0,
                Op::Difference => a,
            }
        } else if let (Kind::Leaf(ka, va), Kind::Leaf(kb, vb)) = (na.kind, nb.kind) {
            debug_assert_eq!(ka, kb);
            match op {
                Op::Union => a,
                Op::Properties => {
                    let group = self.merge(Op::Union, va, vb);
                    self.singleton(ka, group)
                }
                Op::Intersection => self.singleton(ka, 1),
                Op::Difference => 0,
            }
        } else if ba == bb {
            let Kind::Fork(bit, al, ar) = na.kind else {
                unreachable!()
            };
            let Kind::Fork(_, bl, br) = nb.kind else {
                unreachable!()
            };
            let l = self.merge(op, al, bl);
            let r = self.merge(op, ar, br);
            self.fork(bit, l, r)
        } else if ba > bb {
            let Kind::Fork(bit, al, ar) = na.kind else {
                unreachable!()
            };
            let (l, r) = if nb.sample & (1usize << bit) == 0 {
                (
                    self.merge(op, al, b),
                    if op == Op::Intersection { 0 } else { ar },
                )
            } else {
                (
                    if op == Op::Intersection { 0 } else { al },
                    self.merge(op, ar, b),
                )
            };
            self.fork(bit, l, r)
        } else {
            let Kind::Fork(bit, bl, br) = nb.kind else {
                unreachable!()
            };
            let keep = matches!(op, Op::Union | Op::Properties);
            let (l, r) = if na.sample & (1usize << bit) == 0 {
                (self.merge(op, a, bl), if keep { br } else { 0 })
            } else {
                (if keep { bl } else { 0 }, self.merge(op, a, br))
            };
            self.fork(bit, l, r)
        };
        self.memo.insert((op, a, b), result);
        result
    }
}
#[derive(Clone, Copy, Default, Debug)]
pub(super) struct Keys {
    pub(super) default: bool,
    pub(super) exceptions: Map,
}
impl Keys {
    pub(super) const NONE: Self = Self {
        default: false,
        exceptions: 0,
    };
    pub(super) const ALL: Self = Self {
        default: true,
        exceptions: 0,
    };
    pub(super) fn contains(self, maps: &Maps, key: usize) -> bool {
        self.default ^ maps.get(self.exceptions, key).is_some()
    }
    pub(super) fn not(self) -> Self {
        Self {
            default: !self.default,
            ..self
        }
    }
    pub(super) fn or(self, other: Self, maps: &mut Maps) -> Self {
        let exceptions = match (self.default, other.default) {
            (false, false) => maps.merge(Op::Union, self.exceptions, other.exceptions),
            (true, true) => maps.merge(Op::Intersection, self.exceptions, other.exceptions),
            (true, false) => maps.merge(Op::Difference, self.exceptions, other.exceptions),
            (false, true) => maps.merge(Op::Difference, other.exceptions, self.exceptions),
        };
        Self {
            default: self.default || other.default,
            exceptions,
        }
    }
    pub(super) fn and(self, other: Self, maps: &mut Maps) -> Self {
        self.not().or(other.not(), maps).not()
    }
}
