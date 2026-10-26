//! Parsed occurrence budgets, physical owners and ordered protection witnesses.
use super::{Node, unmarshal};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Part {
    Key(String, String),
    Index(usize),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Side {
    Document,
    Root,
    Key,
    Value,
    Item,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    Head,
    Line,
    Foot,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Owner {
    path: Vec<Part>,
    side: Side,
    slot: Slot,
}
impl Owner {
    fn json(&self) -> Value {
        json!({"path":self.path.iter().map(|p|match p{Part::Key(tag,key)=>json!({"key":key,"tag":tag}),Part::Index(i)=>json!({"index":i})}).collect::<Vec<_>>(),"node":match self.side{Side::Document=>"document",Side::Root=>"root",Side::Key=>"key",Side::Value=>"value",Side::Item=>"item"},"slot":match self.slot{Slot::Head=>"head",Slot::Line=>"line",Slot::Foot=>"foot"}})
    }
}
#[derive(Clone, Debug)]
struct Occurrence {
    text: String,
    owner: Owner,
    entry_line: bool,
    /// An index is a source witness only while its enclosing sequences agree.
    sequences: Vec<Value>,
    /// Exact source identity destinations bypass only sequence-index stability.
    relocated: bool,
    /// A fallback has no observed transfer separating it from Go-kept text.
    fallback_relocated: bool,
}
fn inventory(text: &str, expand: bool) -> Option<Vec<Occurrence>> {
    if !super::single_document(text) {
        return None;
    }
    let doc = unmarshal(text.as_bytes()).ok()?;
    let root = doc.content.first()?;
    let expanded;
    let root = if expand {
        expanded = super::super::expand_config_aliases(root).ok()?;
        &expanded
    } else {
        root
    };
    fn visit(
        n: &Node,
        path: &mut Vec<Part>,
        side: Side,
        key_line: usize,
        out: &mut Vec<Occurrence>,
        sequences: &mut Vec<Value>,
    ) -> Option<()> {
        for (slot, text) in [
            (Slot::Head, &n.head_comment),
            (Slot::Line, &n.line_comment),
            (Slot::Foot, &n.foot_comment),
        ] {
            for line in text.lines() {
                let line = line.trim_start_matches([' ', '\t']);
                if line.is_empty() {
                    continue;
                }
                if !line.starts_with('#') {
                    return None;
                }
                out.push(Occurrence {
                    text: line.into(),
                    owner: Owner {
                        path: path.clone(),
                        side,
                        slot,
                    },
                    entry_line: slot == Slot::Line
                        && matches!(side, Side::Key | Side::Value)
                        && key_line > 0
                        && n.line == key_line
                        && n.line_comment_line == key_line,
                    sequences: sequences.clone(),
                    relocated: false,
                    fallback_relocated: false,
                });
            }
        }
        match n.kind {
            super::super::Kind::Mapping => {
                let mut seen = BTreeSet::new();
                for (k, v) in n.pairs() {
                    if k.kind != super::super::Kind::Scalar {
                        return None;
                    }
                    let p = Part::Key(k.tag.clone(), k.value.clone());
                    if !seen.insert(p.clone()) {
                        return None;
                    }
                    path.push(p);
                    visit(k, path, Side::Key, k.line, out, sequences)?;
                    visit(v, path, Side::Value, k.line, out, sequences)?;
                    path.pop();
                }
            }
            super::super::Kind::Sequence => {
                fn identity(n: &Node) -> Value {
                    json!([
                        format!("{:?}", n.kind),
                        n.short_tag(),
                        n.value.as_str(),
                        n.content.iter().map(identity).collect::<Vec<_>>()
                    ])
                }
                // Inventories retain physical output occurrences. Only the
                // value identity is alias-expanded on every side, matching
                // the source basis without inventing output comments.
                let expanded = super::super::expand_config_aliases(n).ok()?;
                sequences.push(identity(&expanded));
                for (i, v) in n.content.iter().enumerate() {
                    path.push(Part::Index(i));
                    visit(v, path, Side::Item, 0, out, sequences)?;
                    path.pop();
                }
                sequences.pop();
            }
            _ => {}
        }
        Some(())
    }
    let mut out = vec![];
    visit(&doc, &mut vec![], Side::Document, 0, &mut out, &mut vec![])?;
    visit(root, &mut vec![], Side::Root, 0, &mut out, &mut vec![])?;
    Some(out)
}
fn counts(items: &[Occurrence]) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for c in items {
        *out.entry(c.text.clone()).or_default() += 1;
    }
    out
}
fn groups(items: &[Occurrence]) -> BTreeMap<Owner, Vec<String>> {
    let mut out = BTreeMap::<_, Vec<_>>::new();
    for c in items {
        out.entry(c.owner.clone()).or_default().push(c.text.clone());
    }
    out
}
fn subsequence(needle: &[String], haystack: &[String]) -> bool {
    let mut at = 0;
    for s in haystack {
        if needle.get(at) == Some(s) {
            at += 1;
        }
    }
    at == needle.len()
}
fn first_unmatched<'a>(needle: &'a [String], haystack: &[String]) -> Option<&'a str> {
    let mut at = 0;
    for text in needle {
        let Some(index) = haystack[at..].iter().position(|other| other == text) else {
            return Some(text);
        };
        at += index + 1;
    }
    None
}
fn owner_failure(s: &[Occurrence], g: &[Occurrence], r: &[Occurrence]) -> Option<Value> {
    let relocated = groups(
        &s.iter()
            .filter(|c| c.relocated)
            .cloned()
            .collect::<Vec<_>>(),
    );
    let stable: Vec<_> = s
        .iter()
        .filter(|source| {
            source.relocated
                || r.iter()
                    .find(|target| target.owner == source.owner)
                    .is_none_or(|target| target.sequences == source.sequences)
        })
        .cloned()
        .collect();
    let (s, g, r) = (groups(&stable), groups(g), groups(r));
    for (owner, items) in &r {
        let source = s.get(owner).cloned().unwrap_or_default();
        let go = g.get(owner).cloned().unwrap_or_default();
        let mut used = BTreeMap::<&str, usize>::new();
        let (mut source_order, mut go_order) = (vec![], vec![]);
        for text in items {
            let sc = source.iter().filter(|x| *x == text).count();
            let gc = go.iter().filter(|x| *x == text).count();
            let n = used.entry(text).or_default();
            *n += 1;
            // Existing source owners consume source slots; only an absent (text,
            // owner) may consume a Go slot. The same Go occurrence is never added twice.
            let budget = if sc > 0 { sc } else { gc };
            if *n > budget {
                return Some(
                    json!({"reason":"owner_budget","text":text,"owner":owner.json(),"source":sc,"go":gc,"protected":*n}),
                );
            }
            if sc > 0 {
                source_order.push(text.clone());
            } else {
                go_order.push(text.clone());
            }
        }
        // A moved Go-only occurrence cannot cross comments Go also kept at
        // this owner. Checking the two disjoint projections alone would lose
        // those shared ordering anchors.
        let go_with_shared: Vec<_> = items
            .iter()
            .filter(|text| go.contains(text))
            .cloned()
            .collect();
        if !subsequence(&source_order, &source)
            || !subsequence(&go_order, &go)
            || (!go_order.is_empty() && !subsequence(&go_with_shared, &go))
        {
            let text = first_unmatched(&source_order, &source)
                .or_else(|| first_unmatched(&go_order, &go))
                .or_else(|| first_unmatched(&go_with_shared, &go));
            return Some(json!({"reason":"owner_order","owner":owner.json(),"text":text}));
        }
    }
    // These source lines have an independently proved destination. After any
    // permitted promotions, require every line there, in source order. An
    // upper count bound alone would accept a partly dropped relocated block.
    for (owner, required) in &relocated {
        let actual = r.get(owner).cloned().unwrap_or_default();
        if !subsequence(required, &actual) {
            return Some(
                json!({"reason":"relocated_source_witness","owner":owner.json(),"text":first_unmatched(required,&actual)}),
            );
        }
    }
    // A global text count cannot substitute an equal comment at another owner.
    for (owner, kept) in &g {
        let actual = r.get(owner).cloned().unwrap_or_default();
        if !subsequence(kept, &actual) {
            return Some(
                json!({"reason":"go_owner_witness","owner":owner.json(),"text":first_unmatched(kept,&actual)}),
            );
        }
    }
    None
}

/// A named rule is reported only when the strict check needs it.
pub struct CommentOwnership {
    pub failure: Option<Value>,
    pub rules: Vec<&'static str>,
}
impl CommentOwnership {
    pub fn accepted(&self) -> bool {
        self.failure.is_none()
    }
}

fn promotion(
    s: &[Occurrence],
    g: Option<&[Occurrence]>,
    r: &[Occurrence],
    line_to_head: bool,
) -> Vec<(Owner, Owner)> {
    let sg = groups(s);
    let rg = groups(r);
    let gg = g.map(groups).unwrap_or_default();
    let mut out = vec![];
    for (owner, donor) in &sg {
        let eligible = if line_to_head {
            g.is_none()
                && owner.slot == Slot::Line
                && matches!(owner.side, Side::Key | Side::Value)
                && s.iter().filter(|c| &c.owner == owner).all(|c| c.entry_line)
        } else {
            owner.slot == Slot::Head
                && owner.side == Side::Value
                && owner.path
                    == [Part::Key(
                        super::super::STR_TAG.into(),
                        "config-version".into(),
                    )]
                && gg
                    .get(owner)
                    .is_none_or(|kept| donor.iter().all(|c| !kept.contains(c)))
        };
        if !eligible || rg.get(owner).is_some_and(|c| !c.is_empty()) {
            continue;
        }
        let target = Owner {
            path: owner.path.clone(),
            side: Side::Key,
            slot: Slot::Head,
        };
        let mut expected = sg.get(&target).cloned().unwrap_or_default();
        expected.extend(donor.clone());
        // These protections append a complete comment block, preserving its order.
        if rg.get(&target) == Some(&expected) {
            out.push((owner.clone(), target));
        }
    }
    out
}
fn promoted(s: &[Occurrence], moves: &[(Owner, Owner)]) -> Vec<Occurrence> {
    let mut result = s.to_vec();
    for c in &mut result {
        if let Some((_, to)) = moves.iter().find(|(from, _)| from == &c.owner) {
            c.owner = to.clone();
        }
    }
    // Group order must be old key-head followed by its promoted source block,
    // even when the donor value is visited after other slots of the key.
    let mut out = vec![];
    for owner in groups(&result).keys() {
        out.extend(s.iter().filter(|c| &c.owner == owner).cloned());
        for (from, to) in moves {
            if to == owner {
                out.extend(s.iter().filter(|c| &c.owner == from).cloned().map(|mut c| {
                    c.owner = to.clone();
                    c
                }));
            }
        }
    }
    out.retain(|c| !moves.iter().any(|(from, _)| from == &c.owner));
    out
}

/// yaml.v3 splits a first root key's multipart head at its final blank
/// paragraph. The identical bytes may therefore reload partly as document
/// head. Preserve a real source document head verbatim; only the additional
/// prefix of this key's exact, ordered paragraph block is equivalent.
fn first_head_projection(
    source: &str,
    target: &str,
    items: &[Occurrence],
    version_value: bool,
) -> Option<Vec<Occurrence>> {
    let s = unmarshal(source.as_bytes()).ok()?;
    let t = unmarshal(target.as_bytes()).ok()?;
    let sr = s.content.first()?;
    let tr = t.content.first()?;
    if sr.kind != super::super::Kind::Mapping || tr.kind != sr.kind {
        return None;
    }
    let key = tr.content.first()?;
    let (sk, sv) = sr
        .pairs()
        .find(|(k, _)| k.tag == key.tag && k.value == key.value)?;
    let mut expected = sk.head_comment.trim_start_matches('\n').to_owned();
    if version_value
        && key.value == "config-version"
        && key.tag == super::super::STR_TAG
        && !sv.head_comment.is_empty()
    {
        if !expected.is_empty() {
            expected.push('\n');
        }
        expected.push_str(&sv.head_comment);
    }
    if !expected.contains("\n\n") {
        return None;
    }
    let extra = if s.head_comment.is_empty() {
        t.head_comment.as_str()
    } else {
        t.head_comment
            .strip_prefix(&s.head_comment)?
            .strip_prefix("\n\n")?
    };
    if extra.is_empty() || format!("{extra}\n\n{}", key.head_comment) != expected {
        return None;
    }
    let owner = Owner {
        path: vec![Part::Key(key.tag.clone(), key.value.clone())],
        side: Side::Key,
        slot: Slot::Head,
    };
    let mut source_document_lines = s
        .head_comment
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    let mut out = items.to_vec();
    for c in &mut out {
        if c.owner.side == Side::Document && c.owner.slot == Slot::Head {
            if source_document_lines > 0 {
                source_document_lines -= 1;
            } else {
                c.owner = owner.clone();
            }
        }
    }
    Some(out)
}

/// Full check-3 evidence. `None` Go means an unloadable step: callers must
/// independently bind the supplied trace bytes before using its counts.
pub fn comment_ownership_analysis(
    source: &str,
    go: Option<&str>,
    protected: &str,
    written: &[String],
) -> CommentOwnership {
    analyze_comments(source, go, protected, written, go.is_none(), None)
}

/// No Go snapshot or independently bound rejected emission exists at this
/// step. Source ownership still applies, but rule (a) has no witness.
pub fn unobserved_comment_ownership(source: &str, protected: &str) -> CommentOwnership {
    analyze_comments(source, None, protected, &[], false, None)
}

fn analyze_comments(
    source: &str,
    go: Option<&str>,
    protected: &str,
    written: &[String],
    observed_unloadable: bool,
    provenance: Option<(&Value, &Value)>,
) -> CommentOwnership {
    let failed = |failure| CommentOwnership {
        failure: Some(failure),
        rules: vec![],
    };
    let Some(mut s) = inventory(source, true) else {
        return failed(json!({"reason":"source_comments_unparseable"}));
    };
    let mut relocated = false;
    let mut archives = Vec::new();
    if let Some((operation, evidence)) = provenance {
        let Some(mapped) = relocated_source(source, operation, evidence, &s) else {
            return failed(json!({"reason":"source_identity_binding"}));
        };
        relocated = mapped.iter().any(|c| c.relocated);
        s = mapped;
        let Some(generated) = archive_inventory(source, evidence) else {
            return failed(json!({"reason":"source_archive_binding"}));
        };
        archives = generated;
    }
    let Some(r) = inventory(protected, false) else {
        return failed(json!({"reason":"protected_comments_unparseable"}));
    };
    let g = match go {
        Some(text) => match inventory(text, false) {
            Some(g) => Some(g),
            None => return failed(json!({"reason":"go_comments_unparseable"})),
        },
        None => None,
    };
    let original_counts = counts(&s);
    let archive_counts = counts(&archives);
    let actual_owners = groups(&r);
    for (owner, expected) in groups(&archives) {
        if !subsequence(
            &expected,
            actual_owners.get(&owner).map_or(&[], Vec::as_slice),
        ) {
            return failed(json!({"reason":"source_archive_owner"}));
        }
    }
    s.extend(archives.clone());
    let (sc, rc) = (counts(&s), counts(&r));
    // A computed archive line is new data, never a substitute for an equal
    // source comment. Require the entire computed multiplicity in addition
    // to every original occurrence of that text; owners are checked below.
    for (text, n) in &archive_counts {
        if rc.get(text).copied().unwrap_or_default()
            != original_counts.get(text).copied().unwrap_or_default() + n
        {
            return failed(json!({"reason":"source_archive_budget"}));
        }
    }
    let gc = if let Some(g) = &g {
        counts(g)
    } else {
        let mut c = BTreeMap::new();
        for text in written {
            *c.entry(text.clone()).or_default() += 1;
        }
        c
    };
    for c in &r {
        if rc[&c.text] > sc.get(&c.text).copied().unwrap_or_default() {
            return failed(
                json!({"reason":"source_budget","text":c.text,"owner":c.owner.json(),"source":sc.get(&c.text).copied().unwrap_or_default(),"protected":rc[&c.text]}),
            );
        }
    }
    for (text, n) in gc {
        if n > rc.get(&text).copied().unwrap_or_default() {
            return failed(
                json!({"reason":"go_budget","text":text,"owner":g.as_deref().unwrap_or(&s).iter().find(|c| c.text==text).map(|c|c.owner.json()),"go":n,"protected":rc.get(&text).copied().unwrap_or_default()}),
            );
        }
    }
    // An alias that remains serialized has only its own physical comments.
    // Source expansion may copy an anchor's comment into its value inventory;
    // that copy cannot license a new comment written on the alias itself.
    if let Some(physical) = physical_alias_owners(source, protected, &s) {
        s = physical;
    } else {
        return failed(json!({"reason":"source_comments_unparseable"}));
    }
    // Without a real Go result, keep every source line at the place computed
    // by replaying Go's edit on our previous output (or its original place
    // when no move was proved). There is no Go output to cross-check it.
    if go.is_none() && !observed_unloadable {
        if let Some(failure) = owner_failure(&s, &[], &r) {
            return failed(failure);
        }
        let actual = groups(&r);
        for (owner, kept) in groups(&s) {
            let at_owner = actual.get(&owner).cloned().unwrap_or_default();
            if !subsequence(&kept, &at_owner) {
                return failed(
                    json!({"reason":"source_owner_witness","owner":owner.json(),"text":first_unmatched(&kept,&at_owner)}),
                );
            }
        }
        return CommentOwnership {
            failure: None,
            rules: provenance_rules(relocated, !archives.is_empty()),
        };
    }
    let empty = vec![];
    let gitems = g.as_deref().unwrap_or(&empty);
    let Some(original_failure) = owner_failure(&s, gitems, &r) else {
        return CommentOwnership {
            failure: None,
            rules: provenance_rules(relocated, !archives.is_empty()),
        };
    };
    let mut remaining_failure = original_failure;
    for mask in 1..16 {
        let (mut rmodel, mut gmodel) = (r.clone(), g.clone());
        if mask & 4 != 0 {
            let rp = first_head_projection(source, protected, &r, mask & 2 != 0);
            let gp = go.and_then(|text| first_head_projection(source, text, gitems, mask & 2 != 0));
            if rp.is_none() && gp.is_none() {
                continue;
            }
            if let Some(projected) = rp {
                rmodel = projected;
            }
            if let Some(projected) = gp {
                gmodel = Some(projected);
            }
        }
        let a = if observed_unloadable {
            promotion(&s, gmodel.as_deref(), &rmodel, true)
        } else {
            vec![]
        };
        let b = promotion(&s, gmodel.as_deref(), &rmodel, false);
        // The dropped-key protection places an explicit key-line block at
        // its relocated key's head. The logical destination is independently
        // fixed by Go's surviving value identity; the complete ordered block
        // must match. An unmoved key receives no loadable-step promotion.
        let relocated_heads: Vec<_> = if gmodel.is_some() {
            promotion(&s, None, &rmodel, true)
                .into_iter()
                .filter(|(from, _)| {
                    from.side == Side::Key
                        && s.iter()
                            .filter(|c| c.owner == *from)
                            .all(|c| c.relocated && c.entry_line)
                })
                .collect()
        } else {
            vec![]
        };
        if (mask & 1 != 0 && a.is_empty())
            || (mask & 2 != 0 && b.is_empty())
            || (mask & 8 != 0 && relocated_heads.is_empty())
        {
            continue;
        }
        let mut moves = vec![];
        let mut rules = provenance_rules(relocated, !archives.is_empty());
        if mask & 1 != 0 {
            moves.extend(a.clone());
            rules.push("unloadable-entry-line-to-own-head");
        }
        if mask & 2 != 0 {
            moves.extend(b.clone());
            rules.push("canonical-version-value-head-to-key-head");
        }
        if mask & 4 != 0 {
            rules.push("first-root-key-head-document-split");
        }
        if mask & 8 != 0 {
            moves.extend(relocated_heads);
            rules.push("relocated-entry-key-line-to-own-head");
        }
        if let Some(failure) = owner_failure(
            &promoted(&s, &moves),
            gmodel.as_deref().unwrap_or(&empty),
            &rmodel,
        ) {
            // The last attempted model includes every locally eligible rule.
            // Name its remaining failure, rather than an already licensed move.
            remaining_failure = failure;
        } else {
            return CommentOwnership {
                failure: None,
                rules,
            };
        }
    }
    failed(remaining_failure)
}

fn physical_alias_owners(
    source: &str,
    protected: &str,
    items: &[Occurrence],
) -> Option<Vec<Occurrence>> {
    fn visit(node: &Node, path: &mut Vec<Part>, aliases: &mut BTreeSet<Vec<Part>>) {
        match node.kind {
            super::super::Kind::Alias => {
                aliases.insert(path.clone());
            }
            super::super::Kind::Mapping => {
                for (k, v) in node.pairs() {
                    path.push(Part::Key(k.tag.clone(), k.value.clone()));
                    visit(v, path, aliases);
                    path.pop();
                }
            }
            super::super::Kind::Sequence => {
                for (i, n) in node.content.iter().enumerate() {
                    path.push(Part::Index(i));
                    visit(n, path, aliases);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let doc = unmarshal(protected.as_bytes()).ok()?;
    let mut aliases = BTreeSet::new();
    visit(doc.content.first()?, &mut Vec::new(), &mut aliases);
    if aliases.is_empty() {
        return Some(items.to_vec());
    }
    let mut out: Vec<_> = items
        .iter()
        .filter(|c| !aliases.contains(&c.owner.path))
        .cloned()
        .collect();
    out.extend(
        inventory(source, false)?
            .into_iter()
            .filter(|c| aliases.contains(&c.owner.path)),
    );
    Some(out)
}

fn provenance_rules(relocated: bool, archived: bool) -> Vec<&'static str> {
    let mut rules = Vec::new();
    if relocated {
        rules.push("upstream-source-identity-relocation");
    }
    if archived {
        rules.push("upstream-generated-archive");
    }
    rules
}

fn entry_signature(node: &Node) -> Value {
    json!([
        format!("{:?}", node.kind),
        node.short_tag(),
        node.value,
        node.head_comment,
        node.line_comment,
        node.foot_comment,
        node.content.iter().map(entry_signature).collect::<Vec<_>>()
    ])
}

fn archive_binding(root: &Node, path: &[Part]) -> Option<Value> {
    let (last, parents) = path.split_last()?;
    let mut node = root;
    for part in parents {
        node = match part {
            Part::Index(i) if node.kind == super::super::Kind::Sequence => node.content.get(*i)?,
            Part::Key(tag, key) if node.kind == super::super::Kind::Mapping => {
                node.pairs()
                    .find(|(k, _)| &k.tag == tag && &k.value == key)?
                    .1
            }
            _ => return None,
        };
    }
    let Part::Key(tag, key) = last else {
        return None;
    };
    let (k, v) = node
        .pairs()
        .find(|(k, _)| &k.tag == tag && &k.value == key)?;
    Some(json!([entry_signature(k), entry_signature(v)]))
}

fn archive_inventory(source: &str, evidence: &Value) -> Option<Vec<Occurrence>> {
    let Some(slots) = evidence.get("archives") else {
        return Some(Vec::new());
    };
    let doc = unmarshal(source.as_bytes()).ok()?;
    let root = super::super::expand_config_aliases(doc.content.first()?).ok()?;
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for slot in slots.as_array()? {
        let source_owner =
            parse_owner(&json!({"path":slot["source_key"],"node":"key","slot":"head"}))?;
        let binding =
            json!({"path":slot["source_key"],"entry":archive_binding(&root,&source_owner.path)?});
        if slot["binding"] != binding {
            return None;
        }
        let ordinal = slot["ordinal"].as_u64()?;
        if !seen.insert((slot["source_key"].to_string(), ordinal)) {
            return None;
        }
        let text = slot["text"].as_str()?;
        if !text.starts_with('#') || text.contains(['\n', '\r']) {
            return None;
        }
        out.push(Occurrence {
            text: text.into(),
            owner: parse_owner(&slot["owner"])?,
            entry_line: false,
            sequences: vec![],
            relocated: false,
            fallback_relocated: false,
        });
    }
    Some(out)
}

// Evidence comes from a separate Go source-tree replay. The full source and
// operation bind it to this step. Slots name a parsed source owner and its
// occurrence ordinal, never an equal comment found elsewhere. Only a unique
// surviving destination can replace a slot; deleted/unmoved identities get no
// new budget. All count, order, Go-owner and existing promotion checks remain.
fn relocated_source(
    source: &str,
    operation: &Value,
    evidence: &Value,
    items: &[Occurrence],
) -> Option<Vec<Occurrence>> {
    if evidence["source"] != source
        || evidence["operation"] != *operation
        || evidence.get("unavailable").is_some()
    {
        return None;
    }
    let moves = evidence["moves"].as_array()?;
    let mut ordinals = BTreeMap::<Owner, usize>::new();
    let mut result = items.to_vec();
    for c in &mut result {
        let ordinal = ordinals.entry(c.owner.clone()).or_default();
        let matching: Vec<_> = moves
            .iter()
            .filter(|m| {
                m["source"] == c.owner.json() && m["ordinal"].as_u64() == Some(*ordinal as u64)
            })
            .collect();
        // The source position and ordinal select the move. Text only checks
        // that both parsers meant the same line; it never selects a donor.
        if matching
            .iter()
            .any(|m| m["text"].as_str() != Some(c.text.as_str()))
        {
            return None;
        }
        let destinations: BTreeSet<_> = matching
            .iter()
            .map(|m| m["destination"].to_string())
            .collect();
        *ordinal += 1;
        if destinations.len() != 1 {
            continue;
        }
        let destination: Value = serde_json::from_str(destinations.first()?).ok()?;
        let owner = parse_owner(&destination)?;
        if owner.path != c.owner.path {
            c.owner = owner;
            c.relocated = true;
            c.fallback_relocated = matching.iter().any(|m| m["via"] == "fallback");
        }
    }
    Some(result)
}

fn parse_owner(value: &Value) -> Option<Owner> {
    Some(Owner {
        path: value["path"]
            .as_array()?
            .iter()
            .map(|p| {
                if let Some(i) = p["index"].as_u64() {
                    Some(Part::Index(usize::try_from(i).ok()?))
                } else {
                    Some(Part::Key(
                        p["tag"].as_str()?.into(),
                        p["key"].as_str()?.into(),
                    ))
                }
            })
            .collect::<Option<_>>()?,
        side: match value["node"].as_str()? {
            "document" => Side::Document,
            "root" => Side::Root,
            "key" => Side::Key,
            "value" => Side::Value,
            "item" => Side::Item,
            _ => return None,
        },
        slot: match value["slot"].as_str()? {
            "head" => Slot::Head,
            "line" => Slot::Line,
            "foot" => Slot::Foot,
            _ => return None,
        },
    })
}

/// Supplement the strict check with independently replayed Go edits. If the
/// real Go run never reached this step, replay it on our previous output and
/// require every source comment at its computed place, without a Go cross-check.
pub fn comment_ownership_with_provenance(
    source: &str,
    go: Option<&str>,
    protected: &str,
    written: &[String],
    operation: &Value,
    evidence: &Value,
    observed: bool,
) -> CommentOwnership {
    let strict = analyze_comments(source, go, protected, written, observed, None);
    if strict.accepted() {
        return strict;
    }
    if evidence["source"] != source
        || evidence["operation"] != *operation
        || evidence.get("unavailable").is_some()
        || !evidence["moves"].is_array()
    {
        return strict;
    }
    let Some(source_items) = inventory(source, true) else {
        return strict;
    };
    let Some(relocated) = relocated_source(source, operation, evidence, &source_items) else {
        return strict;
    };
    let go_items = match go {
        Some(text) => match inventory(text, false) {
            Some(items) => Some(items),
            None => return strict,
        },
        None => None,
    };
    // A fallback line and a separate Go-kept line must not share one output
    // occurrence. Their equal text cannot prove two copies survived. Without
    // parsed Go owners, any equal written line is ambiguous; keep strict.
    if relocated.iter().any(|c| {
        c.fallback_relocated
            && match &go_items {
                Some(items) => items.iter().any(|g| g.owner == c.owner && g.text == c.text),
                None => written.contains(&c.text),
            }
    }) {
        return strict;
    }
    let refined = analyze_comments(
        source,
        go,
        protected,
        written,
        observed,
        Some((operation, evidence)),
    );
    // A failed supplementary proof retains the original diagnostic kind. Only
    // a complete proof changes a verdict; partial provenance never reclassifies
    // a still-unproved case or obscures its original failure.
    if refined.accepted() { refined } else { strict }
}
pub fn comment_occurrences(text: &str, comment: &str, expand: bool) -> Option<usize> {
    Some(
        inventory(text, expand)?
            .iter()
            .filter(|c| c.text == comment)
            .count(),
    )
}
pub fn retained_comment_ownership(source: &str, ablated: &str, protected: &str) -> bool {
    comment_ownership_analysis(source, Some(ablated), protected, &[]).accepted()
}
pub fn retained_unloadable_comments(source: &str, protected: &str, ablated: &[String]) -> bool {
    comment_ownership_analysis(source, None, protected, ablated).accepted()
}

/// Independent source-owner proof. The five conditions deliberately remain
/// separate so no count, value or stability witness can stand in for another.
pub struct SourceOwnerProof {
    pub conditions: [bool; 5],
}
impl SourceOwnerProof {
    pub fn accepted(&self) -> bool {
        self.conditions[0]
            && self.conditions[1]
            && self.conditions[2]
            && self.conditions[3]
            && self.conditions[4]
    }
}

// This inventory and its alias/value signatures do not call save/tree.rs or
// inspect writer receipts. Only the parser's physical owners are counted.
fn source_owner_signature(node: &Node, values: bool) -> Option<Value> {
    use crate::config::yaml3::{AliasTarget, Kind};
    fn resolve(mut n: &Node) -> Option<&Node> {
        for _ in 0..128 {
            if n.kind != Kind::Alias {
                return Some(n);
            }
            let Some(AliasTarget::Node(target)) = &n.alias else {
                return None;
            };
            n = target;
        }
        None
    }
    fn fields(
        n: &Node,
        values: bool,
        budget: &mut usize,
        depth: usize,
    ) -> Option<BTreeMap<(String, String), Value>> {
        *budget = budget.checked_sub(1)?;
        let n = resolve(n)?;
        if n.kind != Kind::Mapping || !n.content.len().is_multiple_of(2) || depth > 128 {
            return None;
        }
        let (mut inherited, mut explicit) = (BTreeMap::new(), BTreeMap::new());
        for (key, value) in n.pairs() {
            if key.short_tag() == "!!merge" {
                let merged = resolve(value)?;
                let maps: Vec<_> = if merged.kind == Kind::Sequence {
                    merged.content.iter().collect()
                } else {
                    vec![merged]
                };
                for map in maps {
                    for (key, value) in fields(map, values, budget, depth + 1)? {
                        inherited.entry(key).or_insert(value);
                    }
                }
            } else {
                if key.kind != Kind::Scalar {
                    return None;
                }
                let key = (key.short_tag(), key.value.clone());
                if explicit
                    .insert(key, signature(value, values, budget, depth + 1)?)
                    .is_some()
                {
                    return None;
                }
            }
        }
        inherited.extend(explicit);
        Some(inherited)
    }
    fn signature(n: &Node, values: bool, budget: &mut usize, depth: usize) -> Option<Value> {
        *budget = budget.checked_sub(1)?;
        if depth > 128 {
            return None;
        }
        let n = resolve(n)?;
        Some(match n.kind {
            Kind::Scalar => json!(["scalar", n.short_tag(), values.then_some(&n.value)]),
            Kind::Sequence => json!([
                "sequence",
                n.short_tag(),
                n.content
                    .iter()
                    .map(|n| signature(n, values, budget, depth + 1))
                    .collect::<Option<Vec<_>>>()?
            ]),
            Kind::Mapping => json!([
                "mapping",
                n.short_tag(),
                fields(n, values, budget, depth)?
                    .into_iter()
                    .map(|((tag, key), value)| json!([tag, key, value]))
                    .collect::<Vec<_>>()
            ]),
            Kind::Document => signature(n.content.first()?, values, budget, depth + 1)?,
            _ => return None,
        })
    }
    signature(node, values, &mut 30_000, 0)
}

fn source_owner_destination_matches(
    source: &Node,
    go: &Node,
    original: &Owner,
    destination: &Owner,
) -> bool {
    fn resolve(mut node: &Node) -> Option<&Node> {
        for _ in 0..128 {
            if node.kind != super::super::Kind::Alias {
                return Some(node);
            }
            let Some(crate::config::yaml3::AliasTarget::Node(target)) = &node.alias else {
                return None;
            };
            node = target;
        }
        None
    }
    fn at<'a>(doc: &'a Node, owner: &Owner) -> Option<(&'a Node, Option<&'a Node>)> {
        if owner.side == Side::Document {
            return owner.path.is_empty().then_some((doc, None));
        }
        let mut node = doc.content.first()?;
        let mut key_node = None;
        for part in &owner.path {
            node = resolve(node)?;
            key_node = None;
            match part {
                Part::Key(tag, name) => {
                    if node.kind != super::super::Kind::Mapping {
                        return None;
                    }
                    let mut selected = node.pairs().filter(|(key, _)| {
                        key.kind == super::super::Kind::Scalar
                            && key.tag == *tag
                            && key.value == *name
                    });
                    let (key, value) = selected.next()?;
                    if selected.next().is_some() {
                        return None;
                    }
                    key_node = Some(key);
                    node = value;
                }
                Part::Index(index) => {
                    if node.kind != super::super::Kind::Sequence {
                        return None;
                    }
                    node = node.content.get(*index)?;
                }
            }
        }
        if owner.side == Side::Key {
            Some((key_node?, Some(node)))
        } else {
            Some((node, None))
        }
    }
    fn signature(doc: &Node, owner: &Owner) -> Option<Value> {
        let (node, value) = at(doc, owner)?;
        let node = source_owner_signature(node, true)?;
        Some(if let Some(value) = value {
            // A key name alone cannot distinguish equal-named keys under
            // different nodes. Keep its associated value in the comparison.
            json!(["key-value", node, source_owner_signature(value, true)?])
        } else {
            node
        })
    }
    signature(source, original)
        .zip(signature(go, destination))
        .is_some_and(|(source, destination)| source == destination)
}

fn source_owner_inventory(text: &str) -> Option<Vec<Occurrence>> {
    if !super::single_document(text) {
        return None;
    }
    let doc = unmarshal(text.as_bytes()).ok()?;
    let root = doc.content.first()?;
    fn visit(
        n: &Node,
        path: &mut Vec<Part>,
        side: Side,
        sequences: &mut Vec<Value>,
        out: &mut Vec<Occurrence>,
    ) -> Option<()> {
        for (slot, text) in [
            (Slot::Head, &n.head_comment),
            (Slot::Line, &n.line_comment),
            (Slot::Foot, &n.foot_comment),
        ] {
            for line in text
                .lines()
                .map(|s| s.trim_start_matches([' ', '\t']))
                .filter(|s| !s.is_empty())
            {
                if !line.starts_with('#') {
                    return None;
                }
                out.push(Occurrence {
                    text: line.into(),
                    owner: Owner {
                        path: path.clone(),
                        side,
                        slot,
                    },
                    entry_line: false,
                    sequences: sequences.clone(),
                    relocated: false,
                    fallback_relocated: false,
                });
            }
        }
        match n.kind {
            super::super::Kind::Mapping => {
                let mut seen = BTreeSet::new();
                for (key, value) in n.pairs() {
                    if key.kind != super::super::Kind::Scalar {
                        return None;
                    }
                    let part = Part::Key(key.tag.clone(), key.value.clone());
                    if !seen.insert(part.clone()) {
                        return None;
                    }
                    path.push(part);
                    visit(key, path, Side::Key, sequences, out)?;
                    visit(value, path, Side::Value, sequences, out)?;
                    path.pop();
                }
            }
            super::super::Kind::Sequence => {
                sequences.push(source_owner_signature(n, true)?);
                for (index, item) in n.content.iter().enumerate() {
                    path.push(Part::Index(index));
                    visit(item, path, Side::Item, sequences, out)?;
                    path.pop();
                }
                sequences.pop();
            }
            _ => {}
        }
        Some(())
    }
    let mut out = vec![];
    visit(&doc, &mut vec![], Side::Document, &mut vec![], &mut out)?;
    visit(root, &mut vec![], Side::Root, &mut vec![], &mut out)?;
    Some(out)
}

/// Cheap eligibility only; this never grants a proof or selects a donor.
pub fn source_owner_candidate(source: &str, protected: &str) -> bool {
    let (Some(source), Some(protected)) = (
        source_owner_expanded_inventory(source),
        inventory(protected, false),
    ) else {
        return false;
    };
    !source.is_empty() && !protected.is_empty()
}

fn source_owner_values(a: &str, b: &str) -> bool {
    let tags = |text: &str| source_owner_signature(&unmarshal(text.as_bytes()).ok()?, false);
    super::same_values(a, b) == Some(true) && tags(a).zip(tags(b)).is_some_and(|(a, b)| a == b)
}

fn source_owner_native_inventory(value: &Value, text: &str) -> Option<Vec<Occurrence>> {
    if value["source"] != text || value.get("unavailable").is_some() || value["values"].is_null() {
        return None;
    }
    let mut ordinals = BTreeMap::<Owner, usize>::new();
    let mut result = vec![];
    for item in value["comments"].as_array()? {
        let owner = parse_owner(&item["source"])?;
        let next = ordinals.entry(owner.clone()).or_default();
        if item["ordinal"].as_u64() != Some(*next as u64) {
            return None;
        }
        *next += 1;
        let text = item["text"].as_str()?;
        if !text.starts_with('#') || text.contains(['\r', '\n']) {
            return None;
        }
        result.push(Occurrence {
            text: text.into(),
            owner,
            entry_line: false,
            sequences: vec![],
            relocated: false,
            fallback_relocated: false,
        });
    }
    Some(result)
}

// Generated archive slots keep their existing full-entry binding. Alias/merge
// archives need the existing rule; this supplementary proof refuses that shape
// instead of borrowing the writer's expansion or identity code.
fn source_owner_archives(source: &str, evidence: &Value) -> Option<Vec<Occurrence>> {
    let Some(value) = evidence.get("archives") else {
        return Some(vec![]);
    };
    let slots = value.as_array()?;
    if slots.is_empty() {
        return Some(vec![]);
    }
    let doc = unmarshal(source.as_bytes()).ok()?;
    fn plain(n: &Node) -> bool {
        n.kind != super::super::Kind::Alias
            && n.short_tag() != "!!merge"
            && n.content.iter().all(plain)
    }
    if !plain(&doc) {
        return None;
    }
    let root = doc.content.first()?;
    let mut seen = BTreeSet::new();
    let mut out = vec![];
    for slot in slots {
        let source = parse_owner(&json!({"path":slot["source_key"],"node":"key","slot":"head"}))?;
        if slot["binding"]
            != json!({"path":slot["source_key"],"entry":archive_binding(root,&source.path)?})
            || !seen.insert((source.path, slot["ordinal"].as_u64()?))
        {
            return None;
        }
        let text = slot["text"].as_str()?;
        if !text.starts_with('#') || text.contains(['\r', '\n']) {
            return None;
        }
        out.push(Occurrence {
            text: text.into(),
            owner: parse_owner(&slot["owner"])?,
            entry_line: false,
            sequences: vec![],
            relocated: false,
            fallback_relocated: false,
        });
    }
    Some(out)
}

type LayoutAnchor = (String, usize);
type LayoutLine = (Option<LayoutAnchor>, Option<LayoutAnchor>, String, usize);
type LayoutGap = (Option<LayoutAnchor>, Option<LayoutAnchor>);
type LayoutBlankRun = (Option<LayoutLine>, Option<LayoutLine>, Vec<String>);

#[derive(Default)]
struct SourceLayout {
    trailing: BTreeMap<LayoutLine, String>,
    blanks: BTreeMap<LayoutGap, Vec<String>>,
    blank_runs: Vec<LayoutBlankRun>,
}

struct PhysicalLayout {
    codes: Vec<String>,
    trailing: Vec<Option<String>>,
    gaps: Vec<Vec<String>>,
}

fn physical_layout(text: &str) -> Option<PhysicalLayout> {
    use crate::config::yaml3::parser::Parser;
    use crate::config::yaml3::types::{ScalarStyle, TokenType};

    // A top-level key remains a region boundary when its anchor, scalar value
    // or explicit-key spelling changes. NUL cannot occur in physical YAML, so
    // these virtual boundaries cannot collide with a real code line.
    let doc = unmarshal(text.as_bytes()).ok()?;
    let mut top_keys = BTreeMap::<usize, Option<String>>::new();
    if let Some(root) = doc
        .content
        .first()
        .filter(|n| n.kind == super::super::Kind::Mapping)
    {
        for (key, _) in root
            .pairs()
            .filter(|(key, _)| key.kind == super::super::Kind::Scalar)
        {
            let marker = format!("\0top-key\0{}\0{}", key.tag, key.value);
            top_keys
                .entry(key.line)
                .and_modify(|slot| *slot = None)
                .or_insert(Some(marker));
        }
    }
    // Scanner marks count Unicode characters, not UTF-8 bytes. Protect scalar
    // and property tokens so a quoted/plain '#' or block data is never erased
    // as a comment. A block scalar's header comment is outside its data.
    let chars: Vec<_> = text.char_indices().collect();
    let mut opaque = vec![false; chars.len()];
    let mut parser = Parser::new(text.as_bytes());
    loop {
        let token = parser.scan().ok()?;
        if token.typ == TokenType::StreamEnd {
            break;
        }
        if token.typ == TokenType::No {
            return None;
        }
        if !matches!(
            token.typ,
            TokenType::Scalar | TokenType::Tag | TokenType::Anchor | TokenType::Alias
        ) {
            continue;
        }
        let mut start = token.start_mark.index;
        let end = token.end_mark.index;
        if start > end || end > chars.len() {
            return None;
        }
        if token.typ == TokenType::Scalar
            && matches!(token.style, ScalarStyle::Literal | ScalarStyle::Folded)
        {
            start += chars[start..end]
                .iter()
                .position(|(_, c)| matches!(c, '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}'))
                .unwrap_or(end - start);
        }
        opaque[start..end].fill(true);
    }

    let mut codes = Vec::<String>::new();
    let mut trailing = Vec::<Option<String>>::new();
    let mut gaps = vec![Vec::<String>::new()];
    let mut first = 0;
    let mut line_number = 1;
    while first < chars.len() {
        let mut end = first;
        while end < chars.len()
            && !matches!(
                chars[end].1,
                '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}'
            )
        {
            end += 1;
        }
        let byte_end = chars.get(end).map_or(text.len(), |(byte, _)| *byte);
        let content_first = first + usize::from(first == 0 && chars[first].1 == '\u{feff}');
        let byte_first = chars
            .get(content_first)
            .map_or(byte_end, |(byte, _)| *byte)
            .min(byte_end);
        let line = &text[byte_first..byte_end];
        let comment = (content_first..end).find(|i| {
            chars[*i].1 == '#'
                && !opaque[*i]
                && (*i == content_first || matches!(chars[*i - 1].1, ' ' | '\t'))
        });
        let code = comment.map_or(line, |i| &text[byte_first..chars[i].0]);
        let code = code.trim_end_matches([' ', '\t']);
        if code.is_empty() {
            // A comment-only line is a position for neighbouring blanks, not
            // a shared boundary or a layout defect. Trim its text for identity.
            if comment.is_none() {
                gaps.last_mut()?.push(line.to_owned());
            } else {
                codes.push(format!("\0comment\0{}", line.trim()));
                trailing.push(None);
                gaps.push(vec![]);
            }
        } else {
            if let Some(Some(marker)) = top_keys.get(&line_number) {
                codes.push(marker.clone());
                trailing.push(None);
                gaps.push(vec![]);
            }
            codes.push(code.to_owned());
            trailing.push(
                (comment.is_none() && line.len() != code.len())
                    .then(|| line[code.len()..].to_owned()),
            );
            gaps.push(vec![]);
        }
        first = end;
        if first < chars.len() {
            let cr = chars[first].1 == '\r';
            first += 1;
            if cr && first < chars.len() && chars[first].1 == '\n' {
                first += 1;
            }
        }
        line_number += 1;
    }

    Some(PhysicalLayout {
        codes,
        trailing,
        gaps,
    })
}

fn layout_regions(physical: PhysicalLayout, anchors: &BTreeSet<String>) -> SourceLayout {
    let PhysicalLayout {
        codes,
        trailing,
        gaps,
    } = physical;
    let mut occurrences = BTreeMap::new();
    let ids: Vec<_> = codes
        .iter()
        .map(|code| {
            let ordinal = occurrences.entry(code).or_insert(0);
            let id = anchors.contains(code).then(|| (code.clone(), *ordinal));
            *ordinal += 1;
            id
        })
        .collect();
    let mut next = vec![None; codes.len() + 1];
    for i in (0..codes.len()).rev() {
        next[i] = ids[i].clone().or_else(|| next[i + 1].clone());
    }
    let mut previous = None;
    let mut layout = SourceLayout::default();
    let mut ordinals = BTreeMap::new();
    let mut blank_ordinals = BTreeMap::new();
    let mut blank_keys = Vec::new();
    for (i, blanks) in gaps.iter().enumerate() {
        if !blanks.is_empty() {
            layout
                .blanks
                .entry((previous.clone(), next[i].clone()))
                .or_default()
                .extend(blanks.iter().cloned());
        }
        let Some(code) = codes.get(i) else { break };
        let region = if ids[i].is_some() {
            (ids[i].clone(), ids[i].clone())
        } else {
            (previous.clone(), next[i].clone())
        };
        // Indentation and explicit-key spelling may change while a source
        // blank remains between the same neighbouring code positions.
        let normalized = code.trim_start_matches([' ', '\t']);
        let normalized = normalized.strip_prefix("? ").unwrap_or(normalized);
        let blank_site = (region.0.clone(), region.1.clone(), normalized.to_owned());
        let blank_ordinal = blank_ordinals.entry(blank_site.clone()).or_insert(0);
        blank_keys.push((blank_site.0, blank_site.1, blank_site.2, *blank_ordinal));
        *blank_ordinal += 1;
        let site = (region.0, region.1, code.clone());
        let ordinal = ordinals.entry(site.clone()).or_insert(0);
        if let Some(suffix) = &trailing[i] {
            layout
                .trailing
                .insert((site.0, site.1, site.2, *ordinal), suffix.clone());
        }
        // Count every line, not only whitespace defects. Equal repeated lines
        // therefore cannot trade a removed suffix for one at another site.
        *ordinal += 1;
        if ids[i].is_some() {
            previous = ids[i].clone();
        }
    }
    layout.blank_runs = gaps
        .into_iter()
        .enumerate()
        .filter(|(_, blanks)| !blanks.is_empty())
        .map(|(i, blanks)| {
            let before = i.checked_sub(1).map(|j| blank_keys[j].clone());
            let after = blank_keys.get(i).cloned();
            (before, after, blanks)
        })
        .collect();
    layout
}

/// Check physical blank lines and trailing whitespace without re-encoding.
/// Byte-identical Go output passes directly. Otherwise regions lie between
/// shared code lines and top-level keys. Blank runs need a source or Go run
/// with the same previous or next normalized position and occurrence, including
/// comment-only positions. Trailing suffixes also need equal raw code text.
/// Each run uses one input run; different runs may use different inputs.
/// Adding or removing an equal line earlier can shift a blank's occurrence;
/// equal-text comments share that limit. The source-owner proof and the common
/// ownership check both require this guard.
pub fn source_owner_layout(source: &str, go: &str, protected: &str) -> bool {
    if protected == go {
        return true;
    }
    let Some(protected) = physical_layout(protected) else {
        return false;
    };
    // There is no added blank or trailing suffix to account for. In particular,
    // an unloadable Go attempt need not parse when our output has neither.
    if !protected.trailing.iter().any(Option::is_some) && protected.gaps.iter().all(Vec::is_empty) {
        return true;
    }
    let (Some(source), Some(go)) = (physical_layout(source), physical_layout(go)) else {
        return false;
    };
    physical_layout_matches(source, go, protected)
}

fn physical_layout_matches(
    source: PhysicalLayout,
    go: PhysicalLayout,
    protected: PhysicalLayout,
) -> bool {
    let counts = |physical: &PhysicalLayout| {
        let mut counts = BTreeMap::<String, usize>::new();
        for code in &physical.codes {
            if !code.starts_with("\0comment") {
                *counts.entry(code.clone()).or_default() += 1;
            }
        }
        counts
    };
    let sc = counts(&source);
    let gc = counts(&go);
    let rc = counts(&protected);
    let anchors: BTreeSet<_> = sc
        .iter()
        .filter(|(code, n)| gc.get(*code) == Some(*n) && rc.get(*code) == Some(*n))
        .map(|(code, _)| code.clone())
        .collect();
    if anchors.is_empty()
        && (protected.trailing.iter().any(Option::is_some)
            || protected.gaps.iter().any(|gap| !gap.is_empty()))
    {
        return false;
    }
    let source = layout_regions(source, &anchors);
    let go = layout_regions(go, &anchors);
    let protected = layout_regions(protected, &anchors);
    protected.trailing.iter().all(|(site, suffix)| {
        source.trailing.get(site) == Some(suffix) || go.trailing.get(site) == Some(suffix)
    }) && protected.blanks.iter().all(|(site, blanks)| {
        let within = |layout: &SourceLayout| {
            let mut wanted = blanks.iter().peekable();
            for line in layout.blanks.get(site).into_iter().flatten() {
                if wanted.peek().is_some_and(|next| *next == line) {
                    wanted.next();
                }
            }
            wanted.next().is_none()
        };
        within(&source) || within(&go)
    }) && protected.blank_runs.iter().all(|(before, after, blanks)| {
        [&source, &go].iter().any(|other| {
            other
                .blank_runs
                .iter()
                .any(|(b, a, have)| (b == before || a == after) && subsequence(blanks, have))
        })
    })
}

/// Prove retention at independently bound source nodes, with original sides and
/// slots. Opaque IDs travel through the actual pinned Go operation and reload;
/// unmasking must reproduce its complete uninstrumented bytes. Equal-text
/// occurrences, a mismatched input/operation, or incomplete evidence fail closed.
pub fn retained_source_owner(
    source: &str,
    protected: &str,
    operation: &Value,
    evidence: &Value,
) -> SourceOwnerProof {
    let original = retained_unique_source_owner(source, protected, operation, evidence);
    if original.accepted() {
        return original;
    }
    let repaired = retained_expanded_source_owner(source, protected, operation, evidence);
    if repaired.accepted()
        || evidence["retained_source_owner"]
            .get("expanded_source_inventory")
            .is_some()
    {
        repaired
    } else {
        original
    }
}

fn retained_unique_source_owner(
    source: &str,
    protected: &str,
    operation: &Value,
    evidence: &Value,
) -> SourceOwnerProof {
    let refused = || SourceOwnerProof {
        conditions: [false; 5],
    };
    let Some(frame) = evidence.get("retained_source_owner") else {
        return refused();
    };
    let identity = &frame["identity"];
    if evidence["source"] != source
        || evidence["operation"] != *operation
        || evidence.get("unavailable").is_some()
        || identity["source"] != source
        || identity["operation"] != *operation
        || identity.get("unavailable").is_some()
        || frame["protected"] != protected
        || (operation["op"] == "write" && operation["body"] != source)
    {
        return refused();
    }
    let (Some(s), Some(r), Some(masked), Some(records), Some(moves)) = (
        source_owner_inventory(source),
        source_owner_inventory(protected),
        identity["masked_source"].as_str(),
        identity["occurrences"].as_array(),
        evidence["moves"].as_array(),
    ) else {
        return refused();
    };
    let sc = counts(&s);
    if s.is_empty() || sc.values().any(|n| *n != 1) || records.len() != s.len() {
        return refused();
    }
    let mut ids = BTreeSet::new();
    let mut ordinals = BTreeMap::<Owner, usize>::new();
    let mut bindings = vec![];
    let mut modeled = vec![];
    for item in &s {
        let ordinal = ordinals.entry(item.owner.clone()).or_default();
        let selected: Vec<_> = records
            .iter()
            .filter(|record| {
                record["source"] == item.owner.json()
                    && record["ordinal"].as_u64() == Some(*ordinal as u64)
            })
            .collect();
        if selected.len() != 1 || selected[0]["text"] != item.text {
            return refused();
        }
        let Some(id) = selected[0]["id"].as_str() else {
            return refused();
        };
        if !id.starts_with('#')
            || id.contains(['\n', '\r'])
            || source.contains(id)
            || !ids.insert(id.to_owned())
        {
            return refused();
        }
        let matching: Vec<_> = moves
            .iter()
            .filter(|m| {
                m["source"] == item.owner.json() && m["ordinal"].as_u64() == Some(*ordinal as u64)
            })
            .collect();
        // Unique mapping keys name a stationary owner, including a canonical
        // version key whose value Go recreates. An index cannot identify an
        // item after edits: every such owner needs a surviving Go node witness.
        if matching.is_empty()
            && item
                .owner
                .path
                .iter()
                .any(|part| matches!(part, Part::Index(_)))
        {
            return refused();
        }
        let mut paths = BTreeSet::new();
        for m in &matching {
            if m["text"] != item.text || !matches!(m["via"].as_str(), Some("observed" | "fallback"))
            {
                return refused();
            }
            let Some(owner) = parse_owner(&m["destination"]) else {
                return refused();
            };
            paths.insert(owner.path);
        }
        if paths.len() > 1 {
            return refused();
        }
        let mut own = item.clone();
        if let Some(path) = paths.into_iter().next() {
            own.owner.path = path
        }
        modeled.push((own, !matching.is_empty()));
        bindings.push((
            item.owner.clone(),
            *ordinal,
            id.to_owned(),
            item.text.clone(),
        ));
        *ordinal += 1;
    }
    if ids
        .iter()
        .any(|id| ids.iter().any(|other| id != other && id.contains(other)))
    {
        return refused();
    }
    let unmask = |text: &str| {
        bindings
            .iter()
            .fold(text.to_owned(), |text, (_, _, id, original)| {
                text.replace(id, original)
            })
    };
    let Some(mi) = source_owner_inventory(masked) else {
        return refused();
    };
    let mut expected_mask = s.clone();
    for (item, (_, _, id, _)) in expected_mask.iter_mut().zip(&bindings) {
        item.text = id.clone()
    }
    if groups(&mi) != groups(&expected_mask)
        || unmask(masked) != source
        || !source_owner_values(source, masked)
    {
        return refused();
    }
    let one = |reply: &Value| -> Option<String> {
        if reply.as_object()?.len() != 1 {
            return None;
        }
        let files = reply["files"].as_array()?;
        (files.len() == 1)
            .then(|| files[0].as_str().map(str::to_owned))
            .flatten()
    };
    let (Some(go), Some(marked_go)) = (one(&frame["go"]), one(&frame["marked_go"])) else {
        return refused();
    };
    if unmask(&marked_go) != go {
        return refused();
    }
    let (Ok(source_doc), Ok(go_doc)) = (unmarshal(source.as_bytes()), unmarshal(go.as_bytes()))
    else {
        return refused();
    };
    let (Some(g), Some(marked), Some(native_r)) = (
        source_owner_native_inventory(&frame["go_inventory"], &go),
        source_owner_native_inventory(&frame["marked_go_inventory"], &marked_go),
        source_owner_native_inventory(&frame["protected_inventory"], protected),
    ) else {
        return refused();
    };
    let rc = counts(&r);
    let gc = counts(&g);
    let r_groups = groups(&r);
    let projected: Vec<_> = modeled.iter().map(|(c, _)| c.clone()).collect();
    // Populate every exemption independently of validation's short circuit.
    // Invalid Go witnesses still fail condition two, but must not hide a later
    // destination mismatch in the recorded condition zero.
    let excused: BTreeSet<_> = bindings
        .iter()
        .zip(&modeled)
        .filter_map(|((_, _, id, text), (own, _))| {
            let at: Vec<_> = marked.iter().filter(|c| c.text == *id).collect();
            if at.len() != 1 || at[0].owner != own.owner {
                Some(text.clone())
            } else {
                None
            }
        })
        .collect();
    let go_owners = bindings.iter().all(|(_, _, id, text)| {
        let at: Vec<_> = marked.iter().filter(|c| c.text == *id).collect();
        let original: Vec<_> = g.iter().filter(|c| c.text == *text).collect();
        if at.len() != original.len()
            || at.len() > 1
            || at.iter().zip(original).any(|(a, b)| a.owner != b.owner)
        {
            return false;
        }
        true
    }) && !excused.is_empty()
        && frame["go_inventory"]["values"] == frame["marked_go_inventory"]["values"];
    let source_owners = groups(&native_r) == groups(&r)
        && modeled
            .iter()
            .zip(&bindings)
            .all(|((item, tracked), (original, _, _, _))| {
                rc.get(&item.text) == Some(&1)
                    && r.iter()
                        .any(|actual| actual.text == item.text && actual.owner == item.owner)
                    && (!*tracked
                        || !excused.contains(&item.text)
                        || source_owner_destination_matches(
                            &source_doc,
                            &go_doc,
                            original,
                            &item.owner,
                        ))
            })
        && groups(&projected).iter().all(|(owner, wanted)| {
            groups(&r)
                .get(owner)
                .is_some_and(|actual| subsequence(wanted, actual))
        })
        && source_owner_layout(source, &go, protected);
    let values = source_owner_values(&go, protected)
        && frame["go_inventory"]["values"] == frame["protected_inventory"]["values"];
    let stable = crate::config::Config::load_bytes(protected.as_bytes()).is_ok()
        && frame["repeat_input"] == protected
        && frame["repeat_operation"] == *operation
        && frame["repeat_ok"] == true
        && frame["repeated"] == protected
        && (operation["op"] != "write" || frame["normalized"] == protected);
    let generated = source_owner_archives(source, evidence).is_some_and(|archives| {
        let ac = counts(&archives);
        let mut expected = sc.clone();
        for (text, n) in &ac {
            if sc.contains_key(text) {
                return false;
            }
            *expected.entry(text.clone()).or_default() += n
        }
        let kept_go: Vec<_> = g
            .iter()
            .filter(|c| !excused.contains(&c.text))
            .cloned()
            .collect();
        rc == expected
            && groups(&kept_go).iter().all(|(owner, wanted)| {
                r_groups
                    .get(owner)
                    .is_some_and(|actual| subsequence(wanted, actual))
            })
            && gc
                .iter()
                .all(|(text, n)| sc.contains_key(text) || ac.get(text).is_some_and(|a| n <= a))
            && groups(&archives).iter().all(|(owner, wanted)| {
                r_groups
                    .get(owner)
                    .is_some_and(|actual| subsequence(wanted, actual))
            })
    });
    SourceOwnerProof {
        conditions: [source_owners, go_owners, values, stable, generated],
    }
}

/// A new separator is licensed only by an exact single-line removal which
/// makes pinned yaml.v3 move a source-owned foot. Remove all such licensed
/// lines before applying the unchanged physical-layout guard.
fn source_owner_repair_layout(
    source: &str,
    go: &str,
    protected: &str,
    expected: &[Occurrence],
    native: &[Occurrence],
    frame: &Value,
) -> bool {
    if source_owner_layout(source, go, protected) {
        return true;
    }
    let Some(counterexamples) = frame["foot_separators"].as_array() else {
        return false;
    };
    let lines: Vec<_> = protected.split_inclusive('\n').collect();
    let mut removed = BTreeSet::new();
    for example in counterexamples {
        let Some(line) = example["line"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
        else {
            return false;
        };
        if line == 0
            || lines.get(line) != Some(&"\n")
            || !lines[line - 1].trim_start().starts_with('#')
        {
            return false;
        }
        let without: String = lines
            .iter()
            .enumerate()
            .filter_map(|(i, s)| (i != line).then_some(*s))
            .collect();
        if example["without"] != without {
            return false;
        }
        let Some(reduced) = source_owner_native_inventory(&example["inventory"], &without) else {
            continue;
        };
        let before = groups(native);
        let after = groups(&reduced);
        let necessary = counts(native) == counts(&reduced)
            && example["inventory"]["values"] == frame["protected_inventory"]["values"]
            && expected.iter().any(|item| {
                item.owner.slot == Slot::Foot
                    && before
                        .get(&item.owner)
                        .is_some_and(|at| at.contains(&item.text))
                    && after
                        .get(&item.owner)
                        .map_or(0, |at| at.iter().filter(|t| **t == item.text).count())
                        < before
                            .get(&item.owner)
                            .map_or(0, |at| at.iter().filter(|t| **t == item.text).count())
            });
        if necessary {
            removed.insert(line);
        }
    }
    let reduced: String = lines
        .iter()
        .enumerate()
        .filter_map(|(i, s)| (!removed.contains(&i)).then_some(*s))
        .collect();
    !removed.is_empty() && source_owner_layout(source, go, &reduced)
}

/// Extend the same source-owner proof to repeated text and alias-expanded
/// copies. The source position and ordinal bind each copy independently;
/// text never chooses a donor. Only the original side/slot is restored.
fn retained_expanded_source_owner(
    source: &str,
    protected: &str,
    operation: &Value,
    evidence: &Value,
) -> SourceOwnerProof {
    let refused = || SourceOwnerProof {
        conditions: [false; 5],
    };
    let frame = &evidence["retained_source_owner"];
    if evidence["source"] != source
        || evidence["operation"] != *operation
        || evidence.get("unavailable").is_some()
        || frame["protected"] != protected
        || (operation["op"] == "write" && operation["body"] != source)
    {
        return refused();
    }
    let Some(files) = frame["exact_go"]["files"].as_array() else {
        return refused();
    };
    if frame["exact_go"].as_object().is_none_or(|o| o.len() != 1) || files.len() != 1 {
        return refused();
    }
    let Some(go) = files[0].as_str() else {
        return refused();
    };
    let (Some(s), Some(r), Some(native_s), Some(native_r), Some(g), Some(moves)) = (
        source_owner_expanded_inventory(source),
        inventory(protected, false),
        source_owner_native_inventory(&frame["expanded_source_inventory"], source),
        source_owner_native_inventory(&frame["protected_inventory"], protected),
        source_owner_native_inventory(&frame["exact_go_inventory"], go),
        evidence["moves"].as_array(),
    ) else {
        return refused();
    };
    // Source agreement is required too: alias expansion must not invent owners
    // or occurrence budgets relative to the independent pinned parser.
    if groups(&s) != groups(&native_s) || s.is_empty() {
        return refused();
    }
    let (Ok(sd), Ok(gd)) = (unmarshal(source.as_bytes()), unmarshal(go.as_bytes())) else {
        return refused();
    };
    let mut projected = Vec::new();
    let mut ordinals = BTreeMap::<Owner, usize>::new();
    for item in &s {
        let ordinal = ordinals.entry(item.owner.clone()).or_default();
        let matching: Vec<_> = moves
            .iter()
            .filter(|m| {
                m["source"] == item.owner.json() && m["ordinal"].as_u64() == Some(*ordinal as u64)
            })
            .collect();
        *ordinal += 1;
        if matching.iter().any(|m| {
            m["text"] != item.text || !matches!(m["via"].as_str(), Some("observed" | "fallback"))
        }) {
            return refused();
        }
        let paths: Option<BTreeSet<_>> = matching
            .iter()
            .map(|m| Some(parse_owner(&m["destination"])?.path))
            .collect();
        let Some(paths) = paths else { return refused() };
        if paths.len() > 1 {
            return refused();
        }
        let mut own = item.clone();
        if let Some(path) = paths.into_iter().next() {
            own.owner.path = path;
        }
        if (own.owner.path != item.owner.path
            && !source_owner_destination_matches(&sd, &gd, &item.owner, &own.owner))
            || (item.owner.path.iter().any(|p| matches!(p, Part::Index(_))) && matching.is_empty())
        {
            return refused();
        }
        projected.push(own);
    }
    let Some(archives) = archive_inventory(source, evidence) else {
        return refused();
    };
    projected.extend(archives);
    let expected = groups(&projected);
    let actual = groups(&r);
    let go_groups = groups(&g);
    let by_text = |items: &[Occurrence], text: &str| {
        groups(
            &items
                .iter()
                .filter(|c| c.text == text)
                .cloned()
                .collect::<Vec<_>>(),
        )
    };
    let texts: BTreeSet<_> = r.iter().chain(&g).map(|c| c.text.as_str()).collect();
    let repaired: BTreeSet<_> = texts
        .iter()
        .copied()
        .filter(|text| by_text(&r, text) != by_text(&g, text))
        .collect();
    let source_owners = groups(&native_r) == actual
        && repaired.iter().all(|text| {
            !by_text(&r, text).is_empty() && by_text(&r, text) == by_text(&projected, text)
        })
        && actual.iter().all(|(owner, at)| {
            let wanted = expected.get(owner).cloned().unwrap_or_default();
            let source_order: Vec<_> = at
                .iter()
                .filter(|text| wanted.contains(text))
                .cloned()
                .collect();
            let has_repair = at.iter().any(|text| repaired.contains(text.as_str()));
            let untouched: Vec<_> = at
                .iter()
                .filter(|text| !repaired.contains(text.as_str()))
                .cloned()
                .collect();
            let go_untouched: Vec<_> = go_groups
                .get(owner)
                .into_iter()
                .flatten()
                .filter(|text| !repaired.contains(text.as_str()))
                .cloned()
                .collect();
            (!has_repair || subsequence(&source_order, &wanted)) && untouched == go_untouched
        })
        && source_owner_repair_layout(source, go, protected, &projected, &native_r, frame);
    // A Go-kept source occurrence is mandatory at that same owner. A Go-only
    // occurrence is excused only by its complete source-owner counterpart.
    let go_owners = go_groups.iter().all(|(owner, kept)| {
        let wanted = expected.get(owner).cloned().unwrap_or_default();
        let mut used = BTreeMap::new();
        let shared: Vec<_> = kept
            .iter()
            .filter(|text| {
                let n = used.entry(*text).or_insert(0usize);
                *n += 1;
                *n <= wanted.iter().filter(|t| *t == *text).count()
            })
            .cloned()
            .collect();
        actual.get(owner).is_some_and(|at| subsequence(&shared, at)) || shared.is_empty()
    }) && !repaired.is_empty();
    let values = source_owner_values(go, protected)
        && frame["exact_go_inventory"]["values"] == frame["protected_inventory"]["values"]
        && super::faithful_without_comments(go)
            .zip(super::faithful_without_comments(protected))
            .is_some_and(|(g, r)| g == r);
    let stable = crate::config::Config::load_bytes(protected.as_bytes()).is_ok()
        && frame["repeat_input"] == protected
        && frame["repeat_operation"] == *operation
        && frame["repeat_ok"] == true
        && frame["repeated"] == protected
        && (operation["op"] != "write" || frame["normalized"] == protected);
    let source_counts = counts(&projected);
    let budgets = counts(&r)
        .iter()
        .all(|(text, n)| *n <= source_counts.get(text).copied().unwrap_or_default())
        && r.iter().all(|c| {
            projected
                .iter()
                .chain(&g)
                .any(|allowed| allowed.text == c.text && allowed.owner == c.owner)
        });
    SourceOwnerProof {
        conditions: [source_owners, go_owners, values, stable, budgets],
    }
}

// Expansion copies an anchor's comments but can erase an alias node's own
// physical comments. Those still have source owners and budgets. Take the
// maximum multiplicity at each (owner,text), never add two equal inventories.
fn source_owner_expanded_inventory(source: &str) -> Option<Vec<Occurrence>> {
    let mut expanded = inventory(source, true)?;
    let physical = inventory(source, false)?;
    let mut have = BTreeMap::new();
    for item in &expanded {
        *have
            .entry((item.owner.clone(), item.text.clone()))
            .or_insert(0usize) += 1;
    }
    let mut seen = BTreeMap::new();
    for item in physical {
        let key = (item.owner.clone(), item.text.clone());
        let n = seen.entry(key.clone()).or_insert(0usize);
        *n += 1;
        if *n > have.get(&key).copied().unwrap_or_default() {
            expanded.push(item);
        }
    }
    Some(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_owner_layout_ignores_comment_text_and_accepts_explicit_keys() {
        let source = "x: one # source text  \ny: two\n";
        let go = "x: one # Go text\ny: two\n";
        let protected = "? x # retained text   \n: one\ny: two\n";
        assert!(source_owner_layout(source, go, protected));
        let block = "x: | # header text  \n  # scalar data\n";
        assert!(source_owner_layout(block, block, block));
    }

    #[test]
    fn source_owner_layout_refuses_added_trailing_and_blank_lines() {
        let text = "a: one\nb: two\nc: three\n";
        for protected in [
            "a: one \nb: two\nc: three\n",
            "a: one\n\nb: two\nc: three\n",
        ] {
            assert!(!source_owner_layout(text, text, protected));
        }
    }

    #[test]
    fn source_owner_layout_requires_a_counterpart_and_exact_suffixes() {
        let plain = "a: one\nb: two\n";
        let spaces = "a: one \n\nb: two\n";
        assert!(source_owner_layout(spaces, spaces, spaces));
        assert!(source_owner_layout(spaces, spaces, plain));
        assert!(source_owner_layout(spaces, plain, spaces));
        assert!(source_owner_layout(plain, spaces, spaces));
        assert!(!source_owner_layout(spaces, spaces, "a: one\t\n\nb: two\n"));
        // The inputs' differently indented blanks cannot combine into two.
        assert!(!source_owner_layout(
            "a: one\n\nb: two\n",
            "a: one\n \nb: two\n",
            "a: one\n\n \nb: two\n"
        ));
    }

    #[test]
    fn source_owner_layout_keys_blanks_by_unshared_neighbours() {
        let source = "top: 1\nm:\n  b: x\n\n  c: y\nend: 2\n";
        let go = "top: 1\nm:\n    b: x\n    c: y\nend: 2\n";
        let moved = "top: 1\nm:\n\n    b: x\n    c: y\nend: 2\n";
        let kept = "top: 1\nm:\n    b: x\n\n    c: y\nend: 2\n";
        assert!(!source_owner_layout(source, go, moved));
        assert!(source_owner_layout(source, go, kept));
    }

    #[test]
    fn source_owner_layout_a2_refuses_blank_after_a_different_moved_comment() {
        let source = "top: 1\nm:\n  a: 1\n  # c1\n  b: 2\n  # c2\n\n  c: 3\nend: 2\n";
        let go = "# c1\ntop: 1\nm:\n    a: 1\n    b: 2\n    # c2\n\n    c: 3\nend: 2\n";
        let protected = "top: 1\nm:\n  a: 1\n  # c1\n\n  b: 2\n  # c2\n  c: 3\nend: 2\n";
        assert!(!source_owner_layout(source, go, protected));
    }

    #[test]
    fn source_owner_layout_a3_refuses_blank_after_a_different_remaining_comment() {
        let source = "top: 1\nm:\n  # c1\n\n  a: 1\n  # c2\n  b: 2\nend: 2\n";
        let go = "top: 1\nm:\n    a: 1\n    # c2\n    b: 2\nend: 2\n";
        let protected = "top: 1\nm:\n  a: 1\n  # c2\n\n  b: 2\nend: 2\n";
        assert!(!source_owner_layout(source, go, protected));
    }

    #[test]
    fn source_owner_layout_w1_keeps_blank_after_its_comment_when_another_moves() {
        let source = "top: 1\nm:\n  # c0\n  a: 1\n  # c1\n\n  b: 2\nend: 2\n";
        let go = "# c0\ntop: 1\nm:\n    a: 1\n    # c1\n    b: 3\nend: 2\n";
        let protected = "# c0\ntop: 1\nm:\n  a: 1\n  # c1\n\n  b: 3\nend: 2\n";
        assert!(source_owner_layout(source, go, protected));
    }

    #[test]
    fn source_owner_layout_w2_keeps_blank_before_its_comment_when_another_is_inline() {
        let source = "top: 1\nm:\n  a: 1\n  # c1\n\n  # c2\n  b: 2\nend: 2\n";
        let go = "top: 1\nm:\n    a: 1 # c1\n    b: 3\nend: 2\n";
        let protected = "top: 1\nm:\n  a: 1 # c1\n\n  # c2\n  b: 3\nend: 2\n";
        assert!(source_owner_layout(source, go, protected));
    }

    #[test]
    fn source_owner_layout_counts_comment_only_positions_for_blanks() {
        let source = "top: 1\nm:\n  b: x\n  # note\n\n  c: y\nend: 2\n";
        let go = "top: 1\nm:\n    b: x\n    # note\n    c: y\nend: 2\n";
        let moved = "top: 1\nm:\n    b: x\n\n    # note\n    c: y\nend: 2\n";
        let kept = "top: 1\nm:\n    b: x\n    # different text\n\n    c: y\nend: 2\n";
        assert!(!source_owner_layout(source, go, moved));
        assert!(source_owner_layout(source, go, kept));
    }

    #[test]
    fn source_owner_layout_counts_unshared_repeated_line_ordinals() {
        let source = "top: 1\nm:\n  - x \n  - x\nend: 2\n";
        let go = "top: 1\nm:\n    - x\n    - x\nend: 2\n";
        let moved = "top: 1\nm:\n  - x\n  - x \nend: 2\n";
        assert!(!source_owner_layout(source, go, moved));
    }

    #[test]
    fn source_owner_layout_counts_unshared_blank_neighbour_ordinals() {
        let source = "top: 1\nm:\n  - x\n\n  - x\nend: 2\n";
        let go = "top: 1\nm:\n    - x\n    - x\nend: 2\n";
        let moved = "top: 1\nm:\n  - x\n  - x\n\nend: 2\n";
        assert!(!source_owner_layout(source, go, moved));
    }

    #[test]
    fn source_owner_layout_refuses_defects_without_shared_lines() {
        assert!(!source_owner_layout("- x \n", "- y\n", "- x \n"));
    }

    #[test]
    fn source_owner_layout_accepts_exact_go_without_shared_source_lines() {
        assert!(source_owner_layout("- x\n", "- y \n\n", "- y \n\n"));
    }

    #[test]
    fn source_owner_layout_cannot_pay_for_a_defect_elsewhere() {
        let source = "a: one \nb: two\nc: three\n";
        let moved = "a: one\nb: two \nc: three\n";
        assert!(!source_owner_layout(source, source, moved));
        let source = "a: one\n\nb: two\nc: three\n";
        let moved = "a: one\nb: two\n\nc: three\n";
        assert!(!source_owner_layout(source, source, moved));
        // The last two x lines have identical adjacent code. Their ordinal
        // counts every site, including the one that originally had no suffix.
        let source = "items:\n  - x\n  - y\n  - x \n  - y\n  - x\n  - y\n";
        let moved = "items:\n  - x\n  - y\n  - x\n  - y\n  - x \n  - y\n";
        assert!(!source_owner_layout(source, source, moved));
        let source = "items:\n  - x\n\n  - y\n  - x\n  - y\n";
        let moved = "items:\n  - x\n  - y\n  - x\n\n  - y\n";
        assert!(!source_owner_layout(source, source, moved));
    }

    #[test]
    fn source_owner_layout_does_not_mask_hashes_in_scalar_data() {
        let source = "x: \"literal # not a comment\"\ny: end\n";
        let protected = "x: \"literal # not a comment\" \ny: end\n";
        assert!(!source_owner_layout(source, source, protected));
        let source = "x: | # header\n  # literal data\ny: end\n";
        let protected = "x: | # header\n  # literal data \ny: end\n";
        assert!(!source_owner_layout(source, source, protected));
    }

    #[test]
    fn source_owner_layout_handles_unicode_bom_and_yaml_line_breaks() {
        for newline in ["\n", "\r\n", "\r", "\u{85}", "\u{2028}", "\u{2029}"] {
            let source =
                format!("\u{feff}# source  {newline}x: \"é # data\"{newline}y: end{newline}");
            let protected =
                format!("# different comment{newline}x: \"é # data\"{newline}y: end{newline}");
            assert!(source_owner_layout(&source, &source, &protected));
            let protected = protected.replace("y: end", "y: end ");
            assert!(!source_owner_layout(&source, &source, &protected));
        }
    }

    fn source_owner_test_snapshot(text: &str) -> Value {
        let snapshot = (|| {
            let items = source_owner_inventory(text)?;
            let values = source_owner_signature(&unmarshal(text.as_bytes()).ok()?, true)?;
            let mut ordinals = BTreeMap::<Owner, usize>::new();
            let comments: Vec<_> = items
                .iter()
                .map(|c| {
                    let next = ordinals.entry(c.owner.clone()).or_default();
                    let row = json!({"source":c.owner.json(),"ordinal":*next,"text":c.text});
                    *next += 1;
                    row
                })
                .collect();
            Some(json!({"source":text,"comments":comments,"values":values}))
        })();
        snapshot.unwrap_or_else(|| json!({"source":text,"unavailable":true}))
    }
    fn refresh_source_owner_test_inventories(evidence: &mut Value) {
        let frame = &mut evidence["retained_source_owner"];
        for name in ["go", "marked_go"] {
            let text = frame[name]["files"][0].as_str().unwrap().to_owned();
            frame[format!("{name}_inventory")] = source_owner_test_snapshot(&text);
        }
        frame["protected_inventory"] =
            source_owner_test_snapshot(frame["protected"].as_str().unwrap());
    }

    fn source_owner_fixture() -> (String, String, Value, Value) {
        let source = "auth-dir: fixture # retained\nnext: fixture\n".to_owned();
        let go = "auth-dir: fixture\n# retained\nnext: fixture\n".to_owned();
        let operation = json!({"op":"write","body":source});
        let id = "# parity-source-fixture-00000000";
        let owner = source_owner_inventory(&source).unwrap()[0].owner.json();
        let mut evidence = json!({"source":source,"operation":operation,"moves":[{"source":owner,"ordinal":0,"text":"# retained","via":"fallback","destination":owner}],"archives":[],"retained_source_owner":{
            "identity":{"source":source,"operation":operation,"masked_source":source.replace("# retained",id),"occurrences":[{"source":owner,"ordinal":0,"text":"# retained","id":id}]},
            "go":{"files":[go]},"marked_go":{"files":[go.replace("# retained",id)]},
            "protected":source,"repeat_input":source,"repeat_operation":operation,"repeat_ok":true,"repeated":source,"normalized":source
        }});
        refresh_source_owner_test_inventories(&mut evidence);
        (source, go, operation, evidence)
    }
    fn replace_source_owner_output(evidence: &mut Value, operation: &Value, output: &str) {
        let frame = &mut evidence["retained_source_owner"];
        frame["protected"] = output.into();
        frame["repeat_input"] = output.into();
        frame["repeated"] = output.into();
        frame["repeat_operation"] = operation.clone();
        frame["normalized"] = output.into();
        refresh_source_owner_test_inventories(evidence);
    }

    fn expanded_owner_fixture() -> (String, String, Value, Value) {
        let (source, go, op, mut evidence) = source_owner_fixture();
        let frame = &mut evidence["retained_source_owner"];
        frame["exact_go"] = json!({"files":[go]});
        frame["exact_go_inventory"] = source_owner_test_snapshot(&go);
        frame["expanded_source_inventory"] = source_owner_test_snapshot(&source);
        frame["foot_separators"] = json!([]);
        (source, go, op, evidence)
    }

    #[test]
    fn expanded_owner_repair_requires_every_independent_condition() {
        let (source, go, op, evidence) = expanded_owner_fixture();
        assert_eq!(
            retained_expanded_source_owner(&source, &source, &op, &evidence).conditions,
            [true; 5]
        );
        for (mutation, index) in [
            ("agreement", 0),
            ("go-kept", 1),
            ("third-owner", 4),
            ("duplicate", 4),
            ("value", 2),
            ("style", 2),
            ("tag", 2),
            ("blank", 0),
            ("unstable", 3),
        ] {
            let mut changed = evidence.clone();
            let mut output = source.clone();
            match mutation {
                "agreement" => {
                    changed["retained_source_owner"]["protected_inventory"]["comments"][0]["source"]
                        ["slot"] = "head".into();
                }
                "go-kept" => {
                    changed["retained_source_owner"]["exact_go"] = json!({"files":[source]});
                    changed["retained_source_owner"]["exact_go_inventory"] =
                        source_owner_test_snapshot(&source);
                }
                "third-owner" => {
                    output = source
                        .replace(" # retained", "")
                        .replace("next: fixture", "next: fixture # retained");
                }
                "duplicate" => {
                    output = source.replace("next:", "# retained\nnext:");
                }
                "value" => {
                    output = source.replacen("fixture", "changed", 1);
                }
                "style" => {
                    output = source.replacen("fixture", "'fixture'", 1);
                }
                "tag" => {
                    output = source.replacen("fixture", "!custom fixture", 1);
                }
                "blank" => {
                    output = source.replace("next:", "\nnext:");
                }
                "unstable" => {
                    changed["retained_source_owner"]["repeated"] = go.clone().into();
                }
                _ => unreachable!(),
            }
            if output != source {
                replace_source_owner_output(&mut changed, &op, &output);
            }
            let proof = retained_expanded_source_owner(&source, &output, &op, &changed);
            assert!(!proof.accepted(), "{mutation}");
            assert!(
                !proof.conditions[index],
                "{mutation}: {:?}",
                proof.conditions
            );
        }
    }

    #[test]
    fn expanded_inventory_keeps_alias_local_comments_and_counts_copies() {
        let source = "auth-dir: &a fixture # inherited\nproxy-url: *a # local\nnext: *a\n";
        let items = source_owner_expanded_inventory(source).unwrap();
        let count = counts(&items);
        assert_eq!(count["# inherited"], 3);
        assert_eq!(count["# local"], 1);
    }

    #[test]
    fn expanded_owner_native_repairs_and_foot_counterexamples() {
        let fixtures: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tools/parity/src/config_save/testdata/owners-repairs.json"
        )))
        .unwrap();
        for fixture in fixtures.as_array().unwrap() {
            let source = fixture["source"].as_str().unwrap();
            let output = fixture["protected"].as_str().unwrap();
            let op = &fixture["operation"];
            let witness = &fixture["evidence"];
            let proof = retained_expanded_source_owner(source, output, op, witness);
            assert_eq!(proof.conditions, [true; 5], "{}", fixture["name"]);
            if fixture["name"] == "plugins-foot-comment" {
                let go = witness["retained_source_owner"]["exact_go"]["files"][0]
                    .as_str()
                    .unwrap();
                assert!(!source_owner_layout(source, go, output));
                let mut changed = witness.clone();
                let frame = &mut changed["retained_source_owner"];
                let inventory = frame["protected_inventory"].clone();
                for example in frame["foot_separators"].as_array_mut().unwrap() {
                    let without = example["without"].clone();
                    example["inventory"] = inventory.clone();
                    example["inventory"]["source"] = without;
                }
                let proof = retained_expanded_source_owner(source, output, op, &changed);
                assert!(!proof.accepted());
                assert!(!proof.conditions[0]);
            }
        }
    }
    #[test]
    fn source_owner_proof_accepts_a_uniquely_bound_moved_or_dropped_comment() {
        let (source, go, operation, mut evidence) = source_owner_fixture();
        assert_eq!(
            retained_source_owner(&source, &source, &operation, &evidence).conditions,
            [true; 5]
        );
        evidence["retained_source_owner"]["go"] = json!({"files":[go.replace("# retained\n","")]});
        evidence["retained_source_owner"]["marked_go"] =
            evidence["retained_source_owner"]["go"].clone();
        refresh_source_owner_test_inventories(&mut evidence);
        assert!(retained_source_owner(&source, &source, &operation, &evidence).accepted());
    }
    #[test]
    fn source_owner_condition_one_rejects_a_third_owner() {
        let (source, _, operation, mut evidence) = source_owner_fixture();
        // Source: the first value's line. Go: the next key's head. This
        // candidate chooses a third owner, the next value's line.
        let third = "auth-dir: fixture\nnext: fixture # retained\n";
        replace_source_owner_output(&mut evidence, &operation, third);
        let proof = retained_source_owner(&source, third, &operation, &evidence);
        assert_eq!(proof.conditions, [false, true, true, true, true]);
        assert!(!proof.accepted(), "third-owner comment was certified");
    }
    #[test]
    fn source_owner_key_destination_includes_its_value() {
        let doc = unmarshal(b"a:\n  k: 1 # c\nb:\n  k: 2\n").unwrap();
        let owner = |parent: &str| {
            parse_owner(&json!({
                "path":[{"key":parent,"tag":"!!str"},{"key":"k","tag":"!!str"}],
                "node":"key","slot":"line"
            }))
            .unwrap()
        };
        assert!(source_owner_destination_matches(
            &doc,
            &doc,
            &owner("a"),
            &owner("a")
        ));
        assert!(
            !source_owner_destination_matches(&doc, &doc, &owner("a"), &owner("b")),
            "equal key names hid different destination values"
        );
    }
    #[test]
    fn source_owner_destination_matches_the_real_go_node() {
        let (source, go, operation, mut evidence) = source_owner_fixture();
        let text = source_owner_inventory(&source).unwrap()[0].text.clone();
        let extra = "destination: [different]\n";
        let go = format!("{go}{extra}");
        let marked = evidence["retained_source_owner"]["marked_go"]["files"][0]
            .as_str()
            .unwrap()
            .to_owned();
        evidence["retained_source_owner"]["go"] = json!({"files":[go]});
        evidence["retained_source_owner"]["marked_go"] =
            json!({"files":[format!("{marked}{extra}")]});
        let third = go
            .replace(&text, "")
            .replace(extra, &format!("destination: [different] {text}\n"));
        replace_source_owner_output(&mut evidence, &operation, &third);
        let destination = source_owner_inventory(&third).unwrap()[0].owner.clone();
        let original = source_owner_inventory(&source).unwrap()[0].owner.clone();
        assert_eq!(destination.side, original.side);
        assert_eq!(destination.slot, original.slot);
        assert!(!source_owner_destination_matches(
            &unmarshal(source.as_bytes()).unwrap(),
            &unmarshal(go.as_bytes()).unwrap(),
            &original,
            &destination,
        ));
        evidence["moves"][0]["destination"] = destination.json();
        let proof = retained_source_owner(&source, &third, &operation, &evidence);
        assert!(
            !proof.accepted(),
            "a replay destination with a different real Go value was certified"
        );
        assert_eq!(proof.conditions, [false, true, true, true, true]);
    }

    #[test]
    fn source_owner_condition_two_requires_a_go_move_or_loss() {
        let (source, _, operation, mut evidence) = source_owner_fixture();
        evidence["retained_source_owner"]["go"] = json!({"files":[source]});
        evidence["retained_source_owner"]["marked_go"] =
            json!({"files":[evidence["retained_source_owner"]["identity"]["masked_source"]]});
        refresh_source_owner_test_inventories(&mut evidence);
        let proof = retained_source_owner(&source, &source, &operation, &evidence);
        assert_eq!(proof.conditions, [true, false, true, true, true]);
        assert!(!proof.accepted(), "a source-correct Go owner was excused");
    }
    #[test]
    fn source_owner_condition_three_checks_decoded_values_and_all_tags() {
        let (source, _, operation, evidence) = source_owner_fixture();
        for output in [
            source.replace("next: fixture", "next: changed"),
            source.replace("next: fixture", "next: !kept fixture"),
        ] {
            let mut evidence = evidence.clone();
            replace_source_owner_output(&mut evidence, &operation, &output);
            let proof = retained_source_owner(&source, &output, &operation, &evidence);
            assert_eq!(proof.conditions, [true, true, false, true, true]);
            assert!(!proof.accepted(), "changed tagged value was certified");
        }
        assert!(source_owner_values(
            "a: &v {x: 1}\nb: *v\n",
            "a: {x: 1}\nb: {x: 1}\n"
        ));
        assert!(!source_owner_values("x: !one value\n", "x: !two value\n"));
    }
    #[test]
    fn source_owner_condition_four_requires_loadable_stable_output() {
        let (source, _, operation, mut evidence) = source_owner_fixture();
        evidence["retained_source_owner"]["repeated"] = format!("{source}\n").into();
        let proof = retained_source_owner(&source, &source, &operation, &evidence);
        assert_eq!(proof.conditions, [true, true, true, false, true]);
        assert!(
            !proof.accepted(),
            "unstable source-owner output was certified"
        );
    }
    #[test]
    fn source_owner_condition_five_rejects_a_forged_comment() {
        let (source, _, operation, mut evidence) = source_owner_fixture();
        let output = format!("{source}# forged\n");
        replace_source_owner_output(&mut evidence, &operation, &output);
        let proof = retained_source_owner(&source, &output, &operation, &evidence);
        assert_eq!(proof.conditions, [true, true, true, true, false]);
        assert!(
            !proof.accepted(),
            "a comment without a source was certified"
        );
    }
    #[test]
    fn source_owner_proof_binds_input_operation_identity_and_real_go_bytes() {
        let (source, _, operation, evidence) = source_owner_fixture();
        for field in ["source", "operation", "moves"] {
            let mut bad = evidence.clone();
            bad[field] = Value::Null;
            assert!(!retained_source_owner(&source, &source, &operation, &bad).accepted());
        }
        let mut bad = evidence.clone();
        bad["retained_source_owner"]["identity"]["occurrences"][0]["ordinal"] = 1.into();
        assert!(!retained_source_owner(&source, &source, &operation, &bad).accepted());
        let mut bad = evidence.clone();
        bad["retained_source_owner"]["marked_go"]["files"][0] =
            "auth-dir: changed\nnext: fixture\n".into();
        assert!(!retained_source_owner(&source, &source, &operation, &bad).accepted());
        for output in [
            source.replace("# retained", ""),
            source.replace("# retained", "# retained\n# retained"),
            "x: [\n".to_owned(),
        ] {
            let mut bad = evidence.clone();
            replace_source_owner_output(&mut bad, &operation, &output);
            assert!(!retained_source_owner(&source, &output, &operation, &bad).accepted());
        }
    }

    #[test]
    fn source_owner_proof_refuses_equal_text_and_preserves_go_kept_owners() {
        let (source, _, operation, evidence) = source_owner_fixture();
        let ambiguous = format!("{source}other: fixture # retained\n");
        assert!(!source_owner_candidate(&ambiguous, &ambiguous));
        assert!(!retained_source_owner(&ambiguous, &ambiguous, &operation, &evidence).accepted());

        let source = format!("{source}kept: fixture # still here\n");
        let operation = json!({"op":"write","body":source});
        let mut evidence = evidence;
        evidence["source"] = source.clone().into();
        evidence["operation"] = operation.clone();
        let kept_owner = source_owner_inventory(&source).unwrap()[1].owner.json();
        evidence["moves"].as_array_mut().unwrap().push(json!({
            "source":kept_owner,"ordinal":0,"text":"# still here","via":"observed","destination":kept_owner
        }));
        let frame = &mut evidence["retained_source_owner"];
        frame["identity"]["source"] = source.clone().into();
        frame["identity"]["operation"] = operation.clone();
        frame["identity"]["masked_source"] = source
            .replace("# retained", "# parity-source-fixture-00000000")
            .replace("# still here", "# parity-source-fixture-00000001")
            .into();
        frame["identity"]["occurrences"].as_array_mut().unwrap().push(json!({
            "source":kept_owner,"ordinal":0,"text":"# still here","id":"# parity-source-fixture-00000001"
        }));
        let go = "auth-dir: fixture\n# retained\nnext: fixture\nkept: fixture # still here\n";
        frame["go"] = json!({"files":[go]});
        frame["marked_go"] = json!({"files":[go
            .replace("# retained", "# parity-source-fixture-00000000")
            .replace("# still here", "# parity-source-fixture-00000001")]});
        replace_source_owner_output(&mut evidence, &operation, &source);
        assert!(retained_source_owner(&source, &source, &operation, &evidence).accepted());
        let wrong = source.replace("kept: fixture # still here", "# still here\nkept: fixture");
        replace_source_owner_output(&mut evidence, &operation, &wrong);
        let proof = retained_source_owner(&source, &wrong, &operation, &evidence);
        assert!(!proof.conditions[0]);
        assert!(!proof.conditions[4]);
        assert!(!proof.accepted());
    }
    fn owner(keys: &[&str], node: &str, slot: &str) -> Value {
        json!({"path":keys.iter().map(|k|json!({"key":k,"tag":"!!str"})).collect::<Vec<_>>(),"node":node,"slot":slot})
    }
    #[test]
    fn alias_sequence_identities_use_the_same_basis_without_moving_comments() {
        let source = "xs:\n  - &a foo # ca\n  - *a\nk: 1\n";
        let go = "xs:\n  - &a foo\n  - *a\nk: 2\n";
        let protected = "xs:\n  - &a foo # ca\n  - *a\nk: 2\n";
        assert!(retained_comment_ownership(source, go, protected));
        assert!(retained_comment_ownership(
            source,
            "xs:\n  - foo\n  - foo\nk: 2\n",
            "xs:\n  - foo # ca\n  - foo\nk: 2\n"
        ));
        assert!(!retained_comment_ownership(
            source,
            go,
            "xs:\n  - &a foo\n  - *a # ca\nk: 2\n"
        ));
        assert!(!retained_comment_ownership(
            source,
            "xs:\n  - &a other\n  - *a\nk: 2\n",
            "xs:\n  - &a other # ca\n  - *a\nk: 2\n"
        ));
        assert_eq!(comment_occurrences(protected, "# ca", false), Some(1));
    }
    #[test]
    fn generated_archive_slots_bind_source_keys_text_owner_and_each_occurrence() {
        let source = "x-note: fixture\ndebug: false\nconfig-version: 8\n";
        let operation = json!({"op":"save","migrate":true});
        let protected = "debug: false\nconfig-version: 8\n\n# x-note: fixture\n";
        let archive_owner = inventory(protected, false)
            .unwrap()
            .into_iter()
            .find(|c| c.text == "# x-note: fixture")
            .unwrap()
            .owner
            .json();
        let key = owner(&["x-note"], "key", "head")["path"].clone();
        let binding_for = |s: &str| {
            let doc = unmarshal(s.as_bytes()).unwrap();
            json!({"path":key,"entry":archive_binding(doc.content.first().unwrap(),&[Part::Key("!!str".into(),"x-note".into())]).unwrap()})
        };
        let mut evidence = json!({"source":source,"operation":operation,"moves":[],"archives":[{"source_key":key,"binding":binding_for(source),"ordinal":0,"owner":archive_owner,"text":"# x-note: fixture"}]});
        let proves = |s: &str, r: &str, e: &Value| {
            comment_ownership_with_provenance(
                s,
                None,
                r,
                &["# x-note: fixture".into()],
                &operation,
                e,
                true,
            )
            .accepted()
        };
        assert!(proves(source, protected, &evidence));
        for bad in [
            "debug: false\nconfig-version: 8\n\n# x-note: altered\n",
            "debug: false\nconfig-version: 8\n\n# x-note: fixture\n# x-note: fixture\n",
            "debug: false # x-note: fixture\nconfig-version: 8\n",
        ] {
            assert!(!proves(source, bad, &evidence));
        }
        let wrong_owner = "debug: false # x-note: fixture\nconfig-version: 8\n";
        assert!(
            !comment_ownership_with_provenance(
                source,
                Some(wrong_owner),
                wrong_owner,
                &[],
                &operation,
                &evidence,
                true
            )
            .accepted()
        );
        let mut wrong_key = evidence.clone();
        wrong_key["archives"][0]["source_key"] = owner(&["debug"], "key", "head")["path"].clone();
        assert!(!proves(source, protected, &wrong_key));
        let mut not_archived = evidence.clone();
        not_archived["archives"] = json!([]);
        assert!(!proves(source, protected, &not_archived));
        // An existing source comment cannot stand in for a newly generated line.
        let disguised = "x-note: fixture\n# x-note: fixture\ndebug: false\nconfig-version: 8\n";
        evidence["source"] = disguised.into();
        evidence["archives"][0]["binding"] = binding_for(disguised);
        assert!(!proves(disguised, protected, &evidence));
    }
    #[test]
    fn upstream_relocations_require_exact_source_operation_destination_and_multiplicity() {
        let source = "disable-cooling: false # own\ndebug: false # stay\n";
        let operation = json!({"op":"save","migrate":true});
        let evidence = json!({"source":source,"operation":operation,"moves":[{"source":owner(&["disable-cooling"],"value","line"),"ordinal":0,"text":"# own","via":"observed","destination":owner(&["routing","cooldown","disable-cooling"],"value","line")}]});
        let protected =
            "routing:\n  cooldown:\n    disable-cooling: false # own\ndebug: false # stay\n";
        let proves = |s: &str, r: &str, op: &Value, proof: &Value| {
            comment_ownership_with_provenance(s, None, r, &[], op, proof, true).accepted()
        };
        assert!(proves(source, protected, &operation, &evidence));
        for wrong in [
            "routing:\n  cooldown:\n    disable-cooling: false\ndebug: false # own\n# stay\n",
            "routing:\n  cooldown:\n    disable-cooling: false # own\n    # own\ndebug: false # stay\n",
            "routing:\n  cooldown:\n    disable-cooling: false # stay\ndebug: false # own\n",
        ] {
            assert!(!proves(source, wrong, &operation, &evidence));
        }
        assert!(!proves(
            &source.replace("false # own", "true # own"),
            protected,
            &operation,
            &evidence
        ));
        assert!(!proves(source, protected, &json!({"op":"save"}), &evidence));
        let mut wrong_text = evidence.clone();
        wrong_text["moves"][0]["text"] = "# stay".into();
        assert!(!proves(source, protected, &operation, &wrong_text));
        // No destination for a deleted identity, including equal-text donors.
        let deleted = "disable-cooling: false # own\ngone: x # own\n";
        let mut proof = evidence.clone();
        proof["source"] = deleted.into();
        let duplicated = "routing:\n  cooldown:\n    disable-cooling: false # own\n    # own\n";
        assert!(!proves(deleted, duplicated, &operation, &proof));
        // A slot-only change on an unmoved source keeps the strict rule.
        let unchanged = "# own\nx: 1\n";
        let proof = json!({"source":unchanged,"operation":operation,"moves":[{"source":owner(&["x"],"key","head"),"ordinal":0,"text":"# own","via":"observed","destination":owner(&["x"],"value","line")}]});
        assert!(!proves(unchanged, "x: 1 # own\n", &operation, &proof));
    }

    #[test]
    fn relocated_blocks_require_every_source_line_at_the_final_owner_in_order() {
        let source = "config-version: 8\nopenai-compatibility:\n  - name: fixture\n    base-url: https://example.invalid/v1\n    # first\n    # second\n    api-key-entries:\n      - api-key: fixture\n";
        let go = "config-version: 8\napi-keys:\n    openai-compatibility:\n        - name: fixture\n          base-url: https://example.invalid/v1\n          keys:\n            - api-key: fixture\n";
        // Keep the review's physical indentation: both comments parse as the
        // new keys entry's head even though they start in column zero.
        let complete = go.replace("          keys:", "# first\n# second\n          keys:");
        let operation = json!({"op":"write","body":source});
        let from = json!({"path":[{"key":"openai-compatibility","tag":"!!str"},{"index":0},{"key":"api-key-entries","tag":"!!str"}],"node":"key","slot":"head"});
        let to = json!({"path":[{"key":"api-keys","tag":"!!str"},{"key":"openai-compatibility","tag":"!!str"},{"index":0},{"key":"keys","tag":"!!str"}],"node":"key","slot":"head"});
        let evidence = json!({"source":source,"operation":operation,"moves":[
            {"source":from,"ordinal":0,"text":"# first","via":"fallback","destination":to},
            {"source":from,"ordinal":1,"text":"# second","via":"fallback","destination":to}
        ]});
        let proves = |protected: &str| {
            comment_ownership_with_provenance(
                source,
                Some(go),
                protected,
                &[],
                &operation,
                &evidence,
                false,
            )
            .accepted()
        };
        assert!(proves(&complete));
        assert!(!proves(&complete.replace("# second\n", "")));
        assert!(!proves(
            &complete.replace("# first\n# second", "# second\n# first")
        ));
    }

    fn fallback_collision_fixture() -> (&'static str, &'static str, Value, Value) {
        let source = "config-version: 8\nopenai-compatibility:\n  - name: fixture\n    base-url: https://example.invalid/v1\n    # first\n    keys: old\n    # first\n    # second\n    api-key-entries:\n      - api-key: fixture\n";
        let go = "config-version: 8\napi-keys:\n    openai-compatibility:\n        - name: fixture\n          base-url: https://example.invalid/v1\n          # first\n          keys:\n            - api-key: fixture\n";
        let operation = json!({"op":"write","body":source});
        let from = json!({"path":[{"key":"openai-compatibility","tag":"!!str"},{"index":0},{"key":"api-key-entries","tag":"!!str"}],"node":"key","slot":"head"});
        let to = json!({"path":[{"key":"api-keys","tag":"!!str"},{"key":"openai-compatibility","tag":"!!str"},{"index":0},{"key":"keys","tag":"!!str"}],"node":"key","slot":"head"});
        let evidence = json!({"source":source,"operation":operation,"moves":[
            {"source":from,"ordinal":0,"text":"# first","via":"fallback","destination":to},
            {"source":from,"ordinal":1,"text":"# second","via":"fallback","destination":to}
        ]});
        (source, go, operation, evidence)
    }

    #[test]
    fn fallback_equal_text_at_go_destination_keeps_strict_result() {
        let (source, go, operation, evidence) = fallback_collision_fixture();
        for block in [
            "# first\n          # second",
            "# first\n          # first\n          # second",
        ] {
            let protected = go.replace("# first", block);
            let strict = comment_ownership_analysis(source, Some(go), &protected, &[]);
            let actual = comment_ownership_with_provenance(
                source,
                Some(go),
                &protected,
                &[],
                &operation,
                &evidence,
                false,
            );
            assert!(
                !actual.accepted(),
                "fallback collision accepted a shared Go occurrence"
            );
            assert_eq!(actual.failure, strict.failure);
            assert!(actual.rules.is_empty());
        }
    }

    #[test]
    fn fallback_equal_text_in_written_trace_keeps_strict_result() {
        let (source, go, operation, evidence) = fallback_collision_fixture();
        let written = vec!["# first".to_owned()];
        for block in [
            "# first\n          # second",
            "# first\n          # first\n          # second",
        ] {
            let protected = go.replace("# first", block);
            let strict = comment_ownership_analysis(source, None, &protected, &written);
            let actual = comment_ownership_with_provenance(
                source, None, &protected, &written, &operation, &evidence, true,
            );
            assert!(
                !actual.accepted(),
                "fallback collision accepted a shared written occurrence"
            );
            assert_eq!(actual.failure, strict.failure);
            assert!(actual.rules.is_empty());
        }
    }

    #[test]
    fn fallback_distinct_text_keeps_complete_block_provable() {
        let (source, go, _, mut evidence) = fallback_collision_fixture();
        let source = source.replacen("# first", "# kept", 1);
        let go = go.replace("# first", "# kept");
        let operation = json!({"op":"write","body":source});
        evidence["source"] = source.clone().into();
        evidence["operation"] = operation.clone();
        let protected = go.replace("# kept", "# kept\n          # first\n          # second");
        let actual = comment_ownership_with_provenance(
            &source,
            Some(&go),
            &protected,
            &[],
            &operation,
            &evidence,
            false,
        );
        assert!(actual.accepted());
        assert!(
            actual
                .rules
                .contains(&"upstream-source-identity-relocation")
        );
    }

    #[test]
    fn relocated_explicit_key_lines_use_only_the_same_surviving_key_head() {
        let source = "? old # own\n: 1\n";
        let operation = json!({"op":"save","migrate":true});
        let evidence = json!({"source":source,"operation":operation,"moves":[{"source":owner(&["old"],"key","line"),"ordinal":0,"text":"# own","via":"fallback","destination":owner(&["section","old"],"key","line")}]});
        let go = "section:\n  old: 1\n";
        let protected = "section:\n  # own\n  old: 1\n";
        let result = comment_ownership_with_provenance(
            source,
            Some(go),
            protected,
            &[],
            &operation,
            &evidence,
            true,
        );
        assert!(result.accepted());
        assert!(
            result
                .rules
                .contains(&"relocated-entry-key-line-to-own-head")
        );
        for wrong in [
            "section:\n  old: 1\n  # own\n  other: 2\n",
            "section:\n  # own\n  # own\n  old: 1\n",
        ] {
            assert!(
                !comment_ownership_with_provenance(
                    source,
                    Some(go),
                    wrong,
                    &[],
                    &operation,
                    &evidence,
                    true
                )
                .accepted()
            );
        }
        assert!(!retained_comment_ownership(
            source,
            "old: 1\n",
            "# own\nold: 1\n"
        ));
    }
    #[test]
    fn unloadable_output_never_supplies_a_comment_owner() {
        let source = "auth-dir: /old # own\ndebug: false\n";
        let protected = "auth-dir: /x\ndebug: false # own\n";
        // Even an exact trace of invalid Go output may supply multiplicity
        // only. This placement isn't in the parsed source.
        assert!(!retained_unloadable_comments(
            source,
            protected,
            &["# own".into()]
        ));
        assert!(retained_unloadable_comments(
            source,
            "# own\nauth-dir: /x\ndebug: false\n",
            &["# own".into()]
        ));
        assert!(!retained_unloadable_comments(
            source,
            "auth-dir: /x # own\n",
            &["# own".into(), "# own".into()]
        ));
    }

    #[test]
    fn budgets_are_multisets_and_scalar_data_is_not_a_comment() {
        assert_eq!(
            comment_occurrences("x: |\n  data # c\ny: '# c'\nz: value # c\n", "# c", false),
            Some(1)
        );
        let source = "a: 1 # c\nb: 2 # c\nconfig-version: 8 # cv\n";
        assert!(!retained_comment_ownership(
            source,
            "a: 1 # c\nb: 2 # c\n",
            "a: 1 # c\nb: 2\nconfig-version: 8 # cv\n"
        ));
        assert!(!retained_comment_ownership(
            source,
            "a: 1\nb: 2\n",
            "a: 1 # invented\nb: 2\n"
        ));
        assert!(!retained_comment_ownership(
            source,
            "a: 1\nb: 2\n",
            "a: 1 # c\nb: 2 # c\nc: 3 # c\n"
        ));
    }
    #[test]
    fn owners_must_be_source_or_ablated_owners_with_occurrence_budgets() {
        let source = "debug: false\nmodels: &m # k\n  catalog: '' # v\nconfig-version: 8 # cv\n";
        let go = "models:\n  catalog: '' # v\nconfig-version: 8 # k\nobservability:\n  logs:\n    debug: false\n";
        let wrong = "models:\n  catalog: '' # v\nconfig-version: 8 # cv\nobservability: # k\n  logs:\n    debug: false\n";
        let fixed = "models:\n  catalog: '' # v\n# k\nconfig-version: 8 # cv\nobservability:\n  logs:\n    debug: false\n";
        assert!(!retained_comment_ownership(source, go, wrong));
        // The former coarse owner ignored this unlicensed value-line -> key-head
        // move on a different key. Slot-aware proof must now reject it.
        assert!(!retained_comment_ownership(source, go, fixed));
        let go_head = go.replace("config-version: 8 # k", "# k\nconfig-version: 8");
        assert!(retained_comment_ownership(source, &go_head, fixed));
        let v4 = "debug: false\nconfig-version:\n  # hv\n  8 # cv\nport: 1\n";
        assert!(!retained_comment_ownership(
            v4,
            "config-version: 8\nserver:\n  port: 1\n",
            "config-version: 8 # cv\n# hv\nserver:\n  port: 1\n"
        ));
        assert!(retained_comment_ownership(
            v4,
            "config-version: 8\nserver:\n  port: 1\n",
            "# hv\nconfig-version: 8 # cv\nserver:\n  port: 1\n"
        ));
    }
}

#[cfg(test)]
mod review_r10 {
    use super::*;
    #[test]
    fn review_negatives_reject_budgets_slots_nodes_and_order() {
        for (name, s, g, r) in [
            (
                "N1",
                "a: 1 # c\nb: 2 # c\n",
                "a: 1 # c\nb: 2\n",
                "# c\na: 1 # c\nb: 2\n",
            ),
            (
                "N1b",
                "a: 1 # c\nb: 2 # c\n",
                "a: 1 # c\nb: 2 # c\n",
                "# c\na: 1 # c\nb: 2\n",
            ),
            (
                "N2",
                "# header\n\na: 1\nb: 2\n",
                "# header\n\na: 1\nb: 2\n",
                "a: 1\nb: 2\n\n# header\n",
            ),
            (
                "N3",
                "# h\na: 1\nb: 2\n",
                "# h\na: 1\nb: 2\n",
                "a: 1 # h\nb: 2\n",
            ),
            (
                "N4",
                "# one\n# two\na: 1\nb: 2\n",
                "# one\n# two\na: 1\nb: 2\n",
                "# two\n# one\na: 1\nb: 2\n",
            ),
            (
                "N5",
                "a:\n  # h\n  x: 1\nb: 2\n",
                "a:\n  # h\n  x: 1\nb: 2\n",
                "a:\n  x: 1\n  # h\nb: 2\n",
            ),
        ] {
            let failure = comment_ownership_analysis(s, Some(g), r, &[])
                .failure
                .expect(name);
            eprintln!("{name}: {failure}");
            assert_eq!(
                failure["reason"],
                if name == "N4" {
                    "owner_order"
                } else {
                    "owner_budget"
                },
                "{name}"
            );
            let (keys, side, slot) = match name {
                "N2" => (vec![], "document", "foot"),
                "N3" => (vec!["a"], "value", "line"),
                "N5" => (vec!["a", "x"], "key", "foot"),
                _ => (vec!["a"], "key", "head"),
            };
            assert_eq!(
                failure["owner"]["path"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p["key"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                keys,
                "{name}"
            );
            assert_eq!(failure["owner"]["node"], side, "{name}");
            assert_eq!(failure["owner"]["slot"], slot, "{name}");
            assert!(!retained_unloadable_comments(s, r, &[]), "{name}");
        }
    }
    #[test]
    fn source_owned_occurrences_cannot_borrow_an_added_go_budget() {
        let s = "# c\na: 1\n# c\nb: 2\n";
        let r = "# c\n# c\na: 1\nb: 2\n";
        let analysis = comment_ownership_analysis(s, Some(s), r, &[]);
        assert_eq!(analysis.failure.unwrap()["reason"], "owner_budget");
    }
    #[test]
    fn go_only_occurrences_keep_their_order_against_shared_comments() {
        let source = "# one\na: 1\n# moved\nb: 2\n";
        let go = "# one\n# moved\na: 1\nb: 2\n";
        let wrong = "# moved\n# one\na: 1\nb: 2\n";
        assert!(retained_comment_ownership(source, go, go));
        assert!(!retained_comment_ownership(source, go, wrong));
    }
    #[test]
    fn diagnostics_report_the_remaining_failure_after_a_licensed_move() {
        let source = "a: 1 # valid\n# wrong\nb: 2\n";
        let protected = "# valid\na: 1\nb: 2 # wrong\n";
        let analysis = comment_ownership_analysis(source, None, protected, &[]);
        assert!(!analysis.accepted());
        assert!(
            analysis.rules.is_empty(),
            "failed proofs do not count rule use"
        );
        let failure = analysis.failure.unwrap();
        assert_eq!(failure["text"], "# wrong");
        assert_eq!(failure["owner"]["path"][0]["key"], "b");
    }
    #[test]
    fn unloadable_physical_entry_line_to_own_head_is_named_and_minimal() {
        for source in [
            "auth-dir: /old # own\ndebug: false\n",
            "? auth-dir # own\n: /old\ndebug: false\n",
        ] {
            let target = "# own\nauth-dir: /x\ndebug: false\n";
            let result = comment_ownership_analysis(source, None, target, &["# own".into()]);
            assert!(result.accepted(), "{:?}", result.failure);
            assert_eq!(result.rules, ["unloadable-entry-line-to-own-head"]);
            // A loadable Go step which kept its slot cannot license this move.
            assert!(!retained_comment_ownership(source, source, target));
            // The same line on another key is never the source owner.
            assert!(!retained_unloadable_comments(
                source,
                "auth-dir: /x\n# own\ndebug: false\n",
                &[]
            ));
        }
        for (source, target) in [
            (
                "# own\nauth-dir: /old\ndebug: false\n",
                "auth-dir: /x # own\ndebug: false\n",
            ),
            (
                "parent:\n  child: old # own\n",
                "# own\nparent:\n  child: new\n",
            ),
            ("parent:\n  - old # own\n", "# own\nparent:\n  - new\n"),
            ("auth-dir:\n  /old # own\n", "# own\nauth-dir: /x\n"),
            ("auth-dir: '/old\n  later' # own\n", "# own\nauth-dir: /x\n"),
        ] {
            assert!(
                !retained_unloadable_comments(source, target, &[]),
                "{source}"
            );
        }
        let unchanged = "auth-dir: /old # own\n";
        assert!(
            comment_ownership_analysis(unchanged, None, unchanged, &[])
                .rules
                .is_empty()
        );
    }
    #[test]
    fn version_value_head_to_own_key_head_is_named_and_minimal() {
        let s = "debug: false\nconfig-version:\n  # one\n\n  # two\n  8\nport: 1\n";
        let r = "debug: false\n\n# one\n\n# two\nconfig-version: 8\nport: 1\n";
        let g = "debug: false\nconfig-version: 8\nport: 1\n";
        let result = comment_ownership_analysis(s, Some(g), r, &[]);
        assert!(result.accepted(), "{:?}", result.failure);
        assert_eq!(result.rules, ["canonical-version-value-head-to-key-head"]);
        assert!(!retained_comment_ownership(s, s, r));
        assert!(!retained_unloadable_comments(
            &s.replace("config-version", "other"),
            &r.replace("config-version", "other"),
            &[]
        ));
        assert!(!retained_unloadable_comments(r, s, &[]));
        assert!(!retained_unloadable_comments(
            s,
            &r.replace("# one\n\n# two", "# two\n\n# one"),
            &[]
        ));
        assert!(!retained_unloadable_comments(
            s,
            "debug: false\nconfig-version: 8\n\n# one\n\n# two\nport: 1\n",
            &[]
        ));
    }
}
