//! One whole-call invariant, including preparation and the final clean.
use super::*;
use crate::common::vertex_partial_args::{StreamingCall, WorkBudget};

const K: usize = 512;
const K2: usize = 1;

#[derive(Clone)]
enum Step {
    Key(String),
    Index(usize),
}
use Step::{Index, Key};
struct Case {
    name: String,
    schema: Value,
    translated: bool,
    paths: Vec<(Vec<Step>, Value)>,
}
struct Run {
    total: usize,
    predicates: usize,
    allowance: usize,
    complete: bool,
    output: String,
}

fn path(steps: &[Step]) -> String {
    let mut out = "$".to_owned();
    for step in steps {
        match step {
            Key(key)
                if !key.is_empty()
                    && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') =>
            {
                out.push('.');
                out.push_str(key);
            }
            Key(key) => out.push_str(&format!("[{}]", serde_json::to_string(key).unwrap())),
            Index(at) => out.push_str(&format!("[{at}]")),
        }
    }
    out
}
fn set(value: &mut Value, steps: &[Step], incoming: Value) {
    let Some((first, rest)) = steps.split_first() else {
        *value = incoming;
        return;
    };
    match first {
        Key(key) => {
            if !value.is_object() {
                *value = json!({});
            }
            let child = value
                .as_object_mut()
                .unwrap()
                .entry(key.clone())
                .or_insert(Value::Null);
            set(child, rest, incoming);
        }
        Index(at) => {
            if !value.is_array() {
                *value = json!([]);
            }
            let values = value.as_array_mut().unwrap();
            values.resize(values.len().max(at + 1), Value::Null);
            set(&mut values[*at], rest, incoming);
        }
    }
}
fn input_sizes(case: &Case) -> (usize, usize, usize) {
    let schema = case.schema.to_string().len() * if case.translated { 2 } else { 1 };
    let paths = case.paths.iter().map(|(steps, _)| path(steps).len()).sum();
    let mut args = Value::Null;
    for (steps, value) in &case.paths {
        set(&mut args, steps, value.clone());
    }
    (schema, paths, args.to_string().len())
}
fn run(case: &Case) -> Run {
    let request = json!({"tools":[{"name":"tool","input_schema":case.schema}]});
    let sent = if case.translated {
        json!({"tools":[{"functionDeclarations":[{"name":"tool","parametersJsonSchema":case.schema}]}]})
    } else {
        Value::Null
    };
    let mut call = StreamingCall::new("tool".into(), "call".into(), "call".into(), 0);
    let work = call.assembler.work_budget().clone();
    let schemas = CallSchemas::resolve("tool", &request, &sent, &work);
    let _ = schemas.parameterless(&work);
    let clean = |value: &Value, work: &WorkBudget| schemas.clean(value, work);
    let probe = |state: usize,
                 step: crate::common::vertex_partial_args::ProbeStep<'_>,
                 work: &WorkBudget| schemas.probe(state, step, work);
    let mut complete = false;
    let mut output = String::new();
    for (index, (steps, value)) in case.paths.iter().enumerate() {
        let chunk = json!({"name":"tool","id":"call","willContinue":true,"partialArgs":[{"jsonPath":path(steps),"stringValue":value}]});
        if index > 0 && !call.can_continue(&chunk) {
            break;
        }
        match call
            .assembler
            .push_indexed(&chunk, Some(&clean), Some(&probe))
        {
            Ok((delta, _)) => output.push_str(&delta),
            Err(_) => break,
        }
    }
    if work.check().is_ok() {
        if let Ok((delta, done)) =
            call.assembler
                .push_indexed(&json!({}), Some(&clean), Some(&probe))
        {
            output.push_str(&delta);
            complete = done;
        }
    }
    Run {
        total: work.used(),
        predicates: work.predicate_used(),
        allowance: work.predicate_allowance(),
        complete,
        output,
    }
}

fn wide(width: usize, nested: bool) -> Value {
    let props: serde_json::Map<_, _> = (0..width)
        .map(|i| {
            (
                format!("k{i:03}"),
                if nested {
                    json!({"type":"object","properties":{"value":{"type":"string"}}})
                } else {
                    json!({"type":"string"})
                },
            )
        })
        .collect();
    json!({"type":"object","properties":props})
}
fn plain(name: &str, width: usize, depth: usize, mixed: bool, translated: bool) -> Case {
    let nested = depth >= 2;
    let mut schema = wide(width, nested);
    let mut prefix = Vec::new();
    for at in 0..depth.saturating_sub(if nested { 2 } else { 1 }) {
        if mixed && at % 2 == 0 {
            schema = json!({"type":"array","items":schema});
            prefix.insert(0, Index(0));
        } else {
            schema = json!({"type":"object","properties":{"n":schema}});
            prefix.insert(0, Key("n".into()));
        }
    }
    let paths = (0..width)
        .map(|i| {
            let mut steps = prefix.clone();
            steps.push(Key(format!("k{i:03}")));
            if nested {
                steps.push(Key("value".into()));
            }
            (steps, json!("x"))
        })
        .collect();
    Case {
        name: format!("{name}-d{depth}-w{width}-t{translated}"),
        schema,
        translated,
        paths,
    }
}
fn reference(width: usize, target_width: usize, chain: usize, translated: bool) -> Case {
    let target_props: serde_json::Map<_, _> = (0..target_width)
        .map(|i| (format!("f{i:02}"), json!({"type":"string"})))
        .collect();
    let mut defs = serde_json::Map::new();
    defs.insert(
        "Big".into(),
        json!({"type":"object","properties":target_props}),
    );
    let mut target = "Big".to_owned();
    for i in 0..chain {
        let name = format!("Link{i}");
        defs.insert(name.clone(), json!({"$ref":format!("#/$defs/{target}")}));
        target = name;
    }
    let props: serde_json::Map<_, _> = (0..width)
        .map(|i| {
            (
                format!("k{i:03}"),
                json!({"$ref":format!("#/$defs/{target}")}),
            )
        })
        .collect();
    Case {
        name: format!("ref-w{width}-b{target_width}-c{chain}-t{translated}"),
        schema: json!({"type":"object","properties":props,"$defs":defs}),
        translated,
        paths: (0..width)
            .map(|i| (vec![Key(format!("k{i:03}")), Key("f00".into())], json!("x")))
            .collect(),
    }
}
fn union(width: usize, kind: &str, closed: bool, translated: bool) -> Case {
    let branches: Vec<_> = (0..width)
        .map(|i| {
            json!({"type":"object","additionalProperties":!closed,"properties":{
                format!("k{i:03}"):{"type":"object","properties":{"value":{"type":"string"}}}
            }})
        })
        .collect();
    Case {
        name: format!("{kind}-w{width}-closed{closed}-t{translated}"),
        schema: json!({kind:branches}),
        translated,
        paths: (0..width)
            .map(|i| {
                (
                    vec![Key(format!("k{i:03}")), Key("value".into())],
                    json!("x"),
                )
            })
            .collect(),
    }
}
fn long_key() -> Case {
    Case {
        name: "opus-long-key".into(),
        schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        translated: false,
        paths: vec![(
            vec![Key("\"".repeat(2_100_000)), Key("value".into())],
            json!("x"),
        )],
    }
}
fn predicates() -> Case {
    let mut base = wide(500, true);
    for i in 0..128 {
        base["properties"][format!("r{i:03}")] = json!({"type":"string"});
    }
    for i in 0..500 {
        base["properties"][format!("k{i:03}")] =
            json!({"type":"object","additionalProperties":false,"properties":{}});
    }
    base["additionalProperties"] = json!(false);
    let required: Vec<_> = (0..128).map(|i| format!("r{i:03}")).collect();
    let mut branches = vec![base];
    branches.extend((0..32).map(|i| {
        let mut list = required.clone();
        list.push(format!("s{i:03}"));
        json!({"type":"object","required":list})
    }));
    let mut paths: Vec<_> = required
        .iter()
        .map(|key| (vec![Key(key.clone())], json!("x")))
        .collect();
    paths.extend((0..500).map(|i| {
        (
            vec![Key(format!("k{i:03}")), Key("reason".into())],
            json!("x"),
        )
    }));
    Case {
        name: "predicate-lists".into(),
        schema: json!({"anyOf":branches}),
        translated: true,
        paths,
    }
}
fn family() -> Vec<Case> {
    let mut cases = vec![
        reference(500, 100, 0, false),
        plain("opus-depth", 1000, 34, false, true),
        union(1000, "anyOf", true, true),
        long_key(),
        predicates(),
    ];
    for translated in [false, true] {
        for depth in [1, 2, 8, 32, 64] {
            for width in [1, 100, 1000] {
                cases.push(plain("matrix", width, depth, false, translated));
            }
        }
        for depth in [2, 16, 64] {
            for width in [1, 1000] {
                cases.push(plain("mixed", width, depth, true, translated));
            }
        }
        for chain in [0, 1, 8, 16] {
            for width in [1, 64, 500] {
                cases.push(reference(width, 100, chain, translated));
            }
        }
        for kind in ["anyOf", "oneOf"] {
            for closed in [false, true] {
                for width in [1, 16, 1000] {
                    cases.push(union(width, kind, closed, translated));
                }
            }
        }
        for shape in ["root", "object", "array", "array_array", "object_object"] {
            let mut case = plain(shape, 500, 2, false, translated);
            let prefix = match shape {
                "root" => vec![],
                "object" => vec![Key("opts".into())],
                "array" => vec![Key("rows".into()), Index(0)],
                "array_array" => vec![Key("rows".into()), Index(0), Index(0)],
                "object_object" => vec![Key("opts".into()), Key("inner".into())],
                _ => unreachable!(),
            };
            for step in prefix.iter().rev() {
                case.schema = match step {
                    Key(key) => json!({"type":"object","properties":{key:case.schema}}),
                    Index(_) => json!({"type":"array","items":case.schema}),
                };
            }
            for (steps, _) in &mut case.paths {
                steps.splice(0..0, prefix.clone());
            }
            cases.push(case);
        }
    }
    for translated in [false, true] {
        let mut composite = reference(500, 100, 0, translated);
        composite.name = format!("ref-with-siblings-t{translated}");
        for i in 0..500 {
            composite.schema["properties"][format!("k{i:03}")]["properties"] =
                json!({format!("own{i}"):{"type":"string"}});
        }
        cases.push(composite);
        let mut conjunction = reference(500, 100, 0, translated);
        let big = conjunction.schema["$defs"]["Big"].take();
        conjunction.schema["$defs"]["Big"] = json!({"allOf":[big,{"additionalProperties":false}]});
        conjunction.name = format!("shared-allof-t{translated}");
        cases.push(conjunction);
        for mutual in [false, true] {
            let mut cycle = reference(4, 2, 0, translated);
            cycle.schema["$defs"]["Big"]["$ref"] = json!(if mutual {
                "#/$defs/Other"
            } else {
                "#/$defs/Big"
            });
            if mutual {
                cycle.schema["$defs"]["Other"] = json!({"$ref":"#/$defs/Big"});
            }
            cycle.name = format!("cycle-mutual{mutual}-t{translated}");
            cases.push(cycle);
        }
        for depth in [2, 8, 64] {
            let mut long = long_key();
            long.name = format!("long-key-d{depth}-t{translated}");
            long.translated = translated;
            long.paths[0].0 = std::iter::once(Key("\"".repeat(2_100_000)))
                .chain((1..depth).map(|_| Key("n".into())))
                .collect();
            cases.push(long);
        }
        let mut cleanup = plain("destructive", 100, 3, false, translated);
        for i in 0..100 {
            cleanup.schema["properties"]["n"]["properties"][format!("k{i:03}")] =
                json!({"type":"object","properties":{},"additionalProperties":false});
        }
        for (steps, _) in &mut cleanup.paths {
            *steps.last_mut().unwrap() = Key("reason".into());
        }
        cases.push(cleanup);
    }
    for translated in [false, true] {
        let mut spelled = reference(500, 100, 0, translated);
        spelled.name = format!("alias-fanout-t{translated}");
        for i in 0..500 {
            spelled.schema["$defs"][format!("A{i}")] = json!({"$ref":"#/$defs/Big"});
            spelled.schema["properties"][format!("k{i:03}")]["$ref"] =
                json!(format!("#/$defs/A{i}"));
        }
        cases.push(spelled);
        cases.push(Case{name:format!("root-array-preserved-t{translated}"),translated,schema:json!({"type":"array","items":{"type":"object","properties":{},"additionalProperties":false}}),paths:vec![(vec![Index(2),Key("reason".into())],json!("x"))]});
        cases.push(Case{name:format!("empty-key-t{translated}"),translated,schema:json!({"type":"object","properties":{"":{"type":"object","properties":{},"additionalProperties":false}}}),paths:vec![(vec![Key("".into()),Key("reason".into())],json!("x"))]});
        for kind in ["self", "mutual", "deep"] {
            cases.push(Case {
                name: format!("cyclic-diamond-{kind}-t{translated}"),
                translated,
                schema: analysis_memo_tests::cyclic_reference_graph(6, 7, kind),
                paths: vec![
                    (vec![Key("p".into())], json!("x")),
                    (vec![Key("reason".into())], json!("x")),
                ],
            });
        }
    }
    let mut generator = analysis_memo_tests::Generator(0x680047);
    for i in 0..256 {
        cases.push(Case {
            name: format!("generated-{i}"),
            translated: i % 2 == 0,
            schema: generator.wide_schema(),
            paths: vec![
                (vec![Key("p".into())], json!("x")),
                (vec![Key("reason".into())], json!("x")),
                (vec![Key("_".into())], json!("x")),
            ],
        });
    }
    cases
}
#[test]
fn round6_whole_call_work_invariant() {
    std::thread::Builder::new().stack_size(8<<20).spawn(|| {
        let mut failures = Vec::new(); let mut maximum = 0f64;
        let opus_only=std::env::var_os("R6_OPUS_ONLY").is_some();
        let stop=std::env::var_os("R6_STOP_AFTER_FAILURE").is_some();
        for case in family().into_iter().take(if opus_only {4}else{usize::MAX}) {
            let (schema,paths,args) = input_sizes(&case); let size = schema+paths+args;
            let actual = WorkBudget::with_index_audit(||run(&case));
            let expected = WorkBudget::with_test_limit(usize::MAX, || run(&case));
            let index_probe = actual.total.saturating_sub(actual.predicates);
            let ratio = index_probe as f64 / size.max(1) as f64; maximum = maximum.max(ratio);
            let predicate_proof=case.name!="predicate-lists" || (actual.predicates==32*129 && actual.allowance==32*129);
            let ok = predicate_proof && actual.complete && expected.complete && actual.output==expected.output && index_probe<=K*size && actual.predicates<=K2*actual.allowance;
            eprintln!("R6_CASE name={} schema={schema} paths={paths} args={args} total={} index_probe={index_probe} predicate={} allowance={} ratio={ratio:.6} complete={} equal={} ok={ok}",case.name,actual.total,actual.predicates,actual.allowance,actual.complete,actual.output==expected.output);
            if !ok { failures.push(case.name); if stop {break;} }
        }
        eprintln!("R6_BOUND K={K} K2={K2} max_ratio={maximum:.6} failures={}",failures.len());
        assert!(failures.is_empty(), "failed family members: {}",failures.join(", "));
    }).unwrap().join().unwrap();
}
