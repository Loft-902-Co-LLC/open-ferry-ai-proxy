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
                .map(|(key, schemas)| (key, schemas.into_iter().map(Value::clone).collect()))
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
        let result = Arc::new(children.into_iter().map(Value::clone).collect());
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
            Some(Arc::new(values.into_iter().map(Value::clone).collect()))
        })();
        self.array_items.insert(key, result.clone());
        result
    }

    pub(super) fn resolve(&mut self, schemas: &[&Value], root: &Value) -> Arc<AnalysisSet> {
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
    #[test]
    fn root_analyses_reuse_owned_results() {
        let schema = json!({"type":"object","properties":{"value":{"type":"string"}}});
        let mut cache = CallAnalysis::default();
        let first = cache.resolve(&[&schema], &schema);
        assert!(Arc::ptr_eq(&first, &cache.resolve(&[&schema], &schema)));
    }
}
