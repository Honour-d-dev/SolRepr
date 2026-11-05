
use crate::{ast::HasBases, hir::{analysis::Analysis, exprs::Name, resolution::Resolution, resolver::Resolver, semantic_errors::{DiagnosticSink, SemanticDiagnostic}}, ir::def_map::DefId, salsa::HirDb};

pub struct Linearizer<'db> {
    this: DefId,
    resolver: Resolver<'db>,
}

impl<'db> Linearizer<'db> {
    pub fn new(db: &'db dyn HirDb, this: DefId) -> Self {
        Self {
            this,
            resolver: Resolver::for_declaration(db, DefId::File(this.file())),
        }
    }

    pub fn c3_linearize(&self) -> Analysis<[DefId]> {
        let mut sink = DiagnosticSink::default();
        let resolver = &self.resolver;
        let this = self.this;
        
        let bases = match this {
            DefId::Contract(id) => resolver.db().ast_id_map(id.file).get(&resolver.db().root(id.file), id.id).unwrap().bases(),
            DefId::Interface(id) => resolver.db().ast_id_map(id.file).get(&resolver.db().root(id.file), id.id).unwrap().bases(),
            _ => return Analysis::new(Vec::new(), sink.finish()),
        };
        
        let mut base_lists = Vec::with_capacity(bases.len());
        let mut problems = Vec::new();
        for base in bases.iter().rev() {
            let base_def = match resolver.resolve_path(&base.segments) {
                Some(Resolution::Type(ty)) => ty.def_id(resolver.db()),
                _ => None,
            };
            let name = base.segments.iter().map(Name::as_str).collect::<Vec<_>>().join(".");
            match base_def {
                Some(def @ (DefId::Contract(_) | DefId::Interface(_))) => {
                    // `bases_analysis` runs even for `def == this`
                    // So salsa cycle can catch self inheritance
                    // `this` is only kept out of the merge inputs.
                    let analysis = resolver.db().bases_analysis(def);
                    sink.extend(analysis.diagnostics.iter());
                    if def != this {
                        base_lists.push(analysis.data.to_vec());
                    }
                }
                Some(_) => problems.push(format!("base `{name}` is not a contract")),
                None => problems.push(format!("unresolved base contract `{name}`")),
            }
        }

        problems.reverse();
        let mut linearized = vec![this];
        if !Self::c3_merge(base_lists, &mut linearized) {
            problems.push("inconsistent inheritance linearization".to_string());
        }
        if !problems.is_empty() {
            sink.push(SemanticDiagnostic::InheritanceFailure { contract: this, problems });
        }
        Analysis::new(linearized, sink.finish())
    }

    fn c3_merge(mut bases: Vec<Vec<DefId>>, linearized: &mut Vec<DefId>) -> bool {
        while !bases.is_empty() {
            let mut candidate: Option<DefId> = None;
            for list in bases.iter() {
                let head = list.first().unwrap();
                let appears_in_tail = bases
                    .iter()
                    .any(|list| list.iter().skip(1).any(|v| v == head));
                if !appears_in_tail {
                    candidate = Some(*head);
                    break;
                }
            }

            let Some(candidate) = candidate else {
                return false;
            };
            linearized.push(candidate);

            bases.retain_mut(|list| {
                list.retain(|item| *item != candidate);
                !list.is_empty()
            });
        }
        true
    }
}

