//! Own analyses for one call, so probes borrow them instead of walking or copying
//! the entire schema again. Cached property schemas have stable heap addresses.

use super::*;

type OwnedInfo = SchemaInfo<Vec<Value>>;
type SchemaIds = Vec<usize>;
type AnalysisKey = (usize, SchemaIds);
type CachedSchemas = Arc<Vec<Value>>;

impl Info<'_> {
    fn into_owned(self) -> OwnedInfo {
        SchemaInfo {
            properties: self
                .properties
                .into_iter()
                .map(|(key, schemas)| (key, schemas.into_iter().map(budget::copy).collect()))
                .collect(),
            required: self.required,
            allows: self.allows,
            forbids: self.forbids,
            pattern: self.pattern,
            complex: self.complex,
            unsafe_complex: self.unsafe_complex,
            object: self.object,
            branches: self.branches.into_iter().map(Self::into_owned).collect(),
        }
    }
}

pub(super) struct AnalysisSet {
    pub(super) infos: Vec<Arc<OwnedInfo>>,
    pub(super) merged: Arc<OwnedInfo>,
}

#[derive(Default)]
pub(super) struct CallAnalysis {
    // Keys are identities only, never dereferenced. CallSchemas boxes the root;
    // translated roots and cached property schemas live in stable Vec buffers.
    sets: HashMap<AnalysisKey, Arc<AnalysisSet>>,
    // Own the selected schemas too: returning a fresh clone on every hit would
    // give descendants new identities and defeat the analysis cache below them.
    children: HashMap<(SchemaIds, String), CachedSchemas>,
    array_items: HashMap<AnalysisKey, Option<CachedSchemas>>,
}

impl CallAnalysis {
    fn identities(schemas: &[&Value]) -> SchemaIds {
        schemas
            .iter()
            .map(|schema| std::ptr::from_ref(*schema) as usize)
            .collect()
    }

    pub(super) fn translated_children(&mut self, schemas: &[&Value], name: &str) -> CachedSchemas {
        if !budget::charge(name.len().saturating_add(schemas.len().saturating_mul(32))) {
            return Arc::default();
        }
        let key = (Self::identities(schemas), name.to_owned());
        if let Some(cached) = self.children.get(&key) {
            return Arc::clone(cached);
        }
        let mut children = Schemas::default();
        let analyses: Vec<_> = schemas
            .iter()
            .map(|schema| self.resolve(&[schema], schema))
            .collect();
        for analysis in &analyses {
            if let Some(values) = analysis.merged.properties.get(name) {
                for value in values {
                    unique(&mut children, value);
                }
            }
        }
        let result = Arc::new(children.into_iter().map(budget::copy).collect());
        self.children.insert(key, Arc::clone(&result));
        result
    }

    // Some(root) follows original references and retains schemas without items;
    // None resolves each translated schema relative to itself, as before.
    pub(super) fn items(
        &mut self,
        schemas: &[&Value],
        root: Option<&Value>,
    ) -> Option<CachedSchemas> {
        if !budget::charge(schemas.len().saturating_add(1).saturating_mul(32)) {
            return None;
        }
        let key = (
            root.map_or(0, |root| std::ptr::from_ref(root) as usize),
            Self::identities(schemas),
        );
        if let Some(cached) = self.array_items.get(&key) {
            return cached.clone();
        }
        let result = (|| {
            let mut values = Schemas::default();
            for schema in schemas {
                let resolved =
                    super::items(schema, root.unwrap_or(schema), 0, &mut BTreeSet::new())?;
                if root.is_some() && resolved.is_empty() {
                    unique(&mut values, schema);
                }
                for item in resolved {
                    unique(&mut values, item);
                }
            }
            Some(Arc::new(values.into_iter().map(budget::copy).collect()))
        })();
        self.array_items.insert(key, result.clone());
        result
    }

    pub(super) fn resolve(&mut self, schemas: &[&Value], root: &Value) -> Arc<AnalysisSet> {
        if !budget::charge(schemas.len().saturating_add(1).saturating_mul(32)) {
            return Arc::new(AnalysisSet {
                infos: Vec::new(),
                merged: Arc::new(OwnedInfo::default()),
            });
        }
        let key = (
            std::ptr::from_ref(root) as usize,
            schemas
                .iter()
                .map(|schema| std::ptr::from_ref(*schema) as usize)
                .collect(),
        );
        if let Some(cached) = self.sets.get(&key) {
            return Arc::clone(cached);
        }
        let infos: Vec<_> = schemas.iter().map(|schema| analyze(schema, root)).collect();
        let merged = if infos.len() == 1 {
            None
        } else {
            let mut merged = Info::default();
            for info in &infos {
                merged.merge(info.clone());
            }
            Some(Arc::new(merged.into_owned()))
        };
        let infos: Vec<_> = infos
            .into_iter()
            .map(|info| Arc::new(info.into_owned()))
            .collect();
        let merged = merged.unwrap_or_else(|| Arc::clone(&infos[0]));
        let result = Arc::new(AnalysisSet { infos, merged });
        self.sets.insert(key, Arc::clone(&result));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::vertex_partial_args::WorkBudget;

    #[test]
    fn round5_warm_deep_probe_work_is_linear_in_path_length() {
        for depth in [8, 16, 32, 48] {
            let mut schema = json!({"type":"object","properties":{"value":{"type":"string"}}});
            let mut probe = json!({"__cpa_probe__":1});
            for _ in 0..depth {
                schema = json!({"type":"object","properties":{"next":schema}});
                probe = json!({"next":probe});
            }
            let request = json!({"tools":[{"name":"deep","input_schema":schema}]});
            let sent = json!({"tools":[{"functionDeclarations":[{"name":"deep","parametersJsonSchema":schema}]}]});
            let work = WorkBudget::default();
            let schemas = CallSchemas::resolve("deep", &request, &sent, &work);
            assert_eq!(schemas.clean(&probe, &work), probe);
            let before = work.used();
            assert_eq!(schemas.clean(&probe, &work), probe);
            let charged = work.used() - before;
            eprintln!("R5_DEPTH depth={depth} warm_probe={charged}");
            assert!(
                charged < 2048 * (depth + 1),
                "depth={depth}, charge={charged}"
            );
            assert!(work.check().is_ok());
        }
    }

    #[test]
    fn round4_root_and_translated_analyses_are_reused_without_copy_charges() {
        let properties: serde_json::Map<_, _> = (0..200)
            .map(|index| {
                (
                    format!("k{index}"),
                    json!({"type":"object","properties":{"value":{"type":"string"}}}),
                )
            })
            .collect();
        let original = json!({"type":"object","properties":properties});
        let mut translated = original.clone();
        translated["required"] = json!(["_"]);
        translated["properties"]["_"] = json!({"type":"boolean"});
        let work = WorkBudget::default();
        budget::with(&work, || {
            let mut cache = CallAnalysis::default();
            let root = cache.resolve(&[&original], &original);
            let sent = cache.resolve(&[&translated], &translated);
            let cold = work.used();
            let root_again = cache.resolve(&[&original], &original);
            let sent_again = cache.resolve(&[&translated], &translated);
            assert!(Arc::ptr_eq(&root, &root_again));
            assert!(Arc::ptr_eq(&sent, &sent_again));
            assert_eq!(
                work.used() - cold,
                128,
                "cache hits charge identities, not schema copies"
            );
            assert!(cold > 128);
            assert_eq!(cache.sets.len(), 2);
        });
        assert!(work.check().is_ok());
    }
}
