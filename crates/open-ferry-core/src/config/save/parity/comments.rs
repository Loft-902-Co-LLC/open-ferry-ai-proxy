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
    // A step Go never executed has no observational licence for either a
    // loss or a named owner change. Preserve every source occurrence exactly.
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
        let destinations: BTreeSet<_> = moves
            .iter()
            .filter(|m| {
                m["source"] == c.owner.json() && m["ordinal"].as_u64() == Some(*ordinal as u64)
            })
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

/// Supplement strict ownership only with independently replayed upstream node
/// destinations. An unobserved Go step still requires every source occurrence.
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

#[cfg(test)]
mod tests {
    use super::*;
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
        let evidence = json!({"source":source,"operation":operation,"moves":[{"source":owner(&["disable-cooling"],"value","line"),"ordinal":0,"destination":owner(&["routing","cooldown","disable-cooling"],"value","line")}]});
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
        // No destination for a deleted identity, including equal-text donors.
        let deleted = "disable-cooling: false # own\ngone: x # own\n";
        let mut proof = evidence.clone();
        proof["source"] = deleted.into();
        let duplicated = "routing:\n  cooldown:\n    disable-cooling: false # own\n    # own\n";
        assert!(!proves(deleted, duplicated, &operation, &proof));
        // A slot-only change on an unmoved source keeps the strict rule.
        let unchanged = "# own\nx: 1\n";
        let proof = json!({"source":unchanged,"operation":operation,"moves":[{"source":owner(&["x"],"key","head"),"ordinal":0,"destination":owner(&["x"],"value","line")}]});
        assert!(!proves(unchanged, "x: 1 # own\n", &operation, &proof));
    }

    #[test]
    fn relocated_explicit_key_lines_use_only_the_same_surviving_key_head() {
        let source = "? old # own\n: 1\n";
        let operation = json!({"op":"save","migrate":true});
        let evidence = json!({"source":source,"operation":operation,"moves":[{"source":owner(&["old"],"key","line"),"ordinal":0,"destination":owner(&["section","old"],"key","line")}]});
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
