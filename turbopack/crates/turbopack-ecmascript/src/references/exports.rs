use anyhow::{Result, bail};
use rustc_hash::FxHashMap;
use swc_core::{
    common::errors::Handler,
    ecma::ast::{Expr, Ident, ImportDecl, MemberProp, Program, Stmt},
};
use tracing::Instrument;
use turbo_rcstr::RcStr;
use turbo_tasks::{ResolvedVc, Vc};
use turbopack_core::{
    issue::{IssueExt, IssueSource},
    reference::ModuleReference,
    resolve::{ImportUsage, ModulePart},
};
use turbopack_swc_utils::emitter::IssueEmitter;

use crate::{
    EcmascriptModuleAsset, EcmascriptParsable, ModuleTypeResult, SpecifiedModuleType,
    TreeShakingMode,
    analyzer::{
        graph::{DeclUsage, create_graph},
        imports::{ImportAnnotations, ImportedSymbol},
    },
    chunk::EcmascriptExports,
    parse::ParseResult,
    references::{
        TURBOPACK_HELPER_WTF8,
        esm::{EsmAssetReference, EsmExports},
        set_handler_and_globals,
        type_issue::SpecifiedModuleTypeIssue,
    },
    runtime_functions::{TURBOPACK_EXPORT_NAMESPACE, TURBOPACK_EXPORT_VALUE},
    tree_shake::{part_of_module, split_module},
};

#[turbo_tasks::value]
pub struct EcmascriptExportsAnalysis {
    pub exports: ResolvedVc<EcmascriptExports>,
    pub import_references: Vec<ResolvedVc<EsmAssetReference>>,
    pub esm_reexport_reference_idxs: Vec<usize>,
    pub esm_evaluation_reference_idxs: Vec<usize>,
}

#[turbo_tasks::function]
pub async fn compute_ecmascript_module_exports(
    module: ResolvedVc<EcmascriptModuleAsset>,
    part: Option<ModulePart>,
) -> Result<Vc<EcmascriptExportsAnalysis>> {
    let raw_module = module.await?;
    let source = raw_module.source;
    let options = raw_module.options.await?;
    let import_externals = options.import_externals;
    let analyze_mode = options.analyze_mode;

    let parsed = if let Some(part) = part {
        let split_data = split_module(*module);
        part_of_module(split_data, part)
    } else {
        module.failsafe_parse()
    };

    let ModuleTypeResult {
        module_type: specified_type,
        ..
    } = *module.determine_module_type().await?;

    let parsed = parsed.await?;
    let ParseResult::Ok {
        program,
        eval_context,
        source_map,
        globals,
        ..
    } = &*parsed
    else {
        return Ok(EcmascriptExportsAnalysis {
            exports: EcmascriptExports::Unknown.resolved_cell(),
            import_references: Vec::new(),
            esm_reexport_reference_idxs: Vec::new(),
            esm_evaluation_reference_idxs: Vec::new(),
        }
        .cell());
    };

    let (emitter, _) = IssueEmitter::new(source, source_map.clone(), None);
    let handler = Handler::with_emitter(true, false, Box::new(emitter));

    let supports_block_scoping = *raw_module
        .compile_time_info
        .environment()
        .runtime_versions()
        .supports_block_scoping()
        .await?;
    let var_graph = {
        let _span = tracing::trace_span!("analyze variable values").entered();
        set_handler_and_globals(&handler, globals, || {
            // TODO don't create_graph twice (exports + references)
            create_graph(program, eval_context, analyze_mode, supports_block_scoping)
        })
    };

    let mut import_usage =
        FxHashMap::with_capacity_and_hasher(var_graph.import_usages.len(), Default::default());
    for (reference, usage) in &var_graph.import_usages {
        // TODO make this more efficient, i.e. cache the result?
        if let DeclUsage::Bindings(ids) = usage {
            // compute transitive closure of `ids` over `top_level_mappings`
            let mut visited = ids.clone();
            let mut stack = ids.iter().collect::<Vec<_>>();
            let mut has_global_usage = false;
            while let Some(id) = stack.pop() {
                match var_graph.decl_usages.get(id) {
                    Some(DeclUsage::SideEffects) => {
                        has_global_usage = true;
                        break;
                    }
                    Some(DeclUsage::Bindings(callers)) => {
                        for caller in callers {
                            if visited.insert(caller.clone()) {
                                stack.push(caller);
                            }
                        }
                    }
                    _ => {}
                }
            }

            // Collect all `visited` declarations which are exported
            import_usage.insert(
                *reference,
                if has_global_usage {
                    ImportUsage::TopLevel
                } else {
                    ImportUsage::Exports(
                        var_graph
                            .exports
                            .iter()
                            .filter(|(_, id)| visited.contains(*id))
                            .map(|(exported, _)| exported.as_str().into())
                            .collect(),
                    )
                },
            );
        }
    }

    let mut esm_reexport_reference_idxs: Vec<usize> = vec![];
    let mut esm_evaluation_reference_idxs: Vec<usize> = vec![];

    let span = tracing::trace_span!("esm import references");
    let import_references = async {
        let mut import_references = Vec::with_capacity(eval_context.imports.references().len());
        for (i, r) in eval_context.imports.references().enumerate() {
            let mut should_add_evaluation = false;
            let reference = EsmAssetReference::new(
                module,
                ResolvedVc::upcast(module),
                RcStr::from(&*r.module_path.to_string_lossy()),
                r.issue_source
                    .unwrap_or_else(|| IssueSource::from_source_only(source)),
                r.annotations.as_ref().map(|a| (**a).clone()),
                match &r.imported_symbol {
                    &ImportedSymbol::ModuleEvaluation => {
                        should_add_evaluation = true;
                        Some(ModulePart::evaluation())
                    }
                    ImportedSymbol::Symbol(name) => Some(ModulePart::export((&**name).into())),
                    ImportedSymbol::PartEvaluation(part_id) | ImportedSymbol::Part(part_id) => {
                        if !matches!(
                            options.tree_shaking_mode,
                            Some(TreeShakingMode::ModuleFragments)
                        ) {
                            bail!(
                                "Internal imports only exist in reexports only mode when \
                                 importing {:?} from {}",
                                r.imported_symbol,
                                r.module_path.to_string_lossy()
                            );
                        }
                        if matches!(&r.imported_symbol, ImportedSymbol::PartEvaluation(_)) {
                            should_add_evaluation = true;
                        }
                        Some(ModulePart::internal(*part_id))
                    }
                    ImportedSymbol::Exports => matches!(
                        options.tree_shaking_mode,
                        Some(TreeShakingMode::ModuleFragments)
                    )
                    .then(ModulePart::exports),
                },
                import_usage.get(&i).cloned().unwrap_or_default(),
                import_externals,
                options.tree_shaking_mode,
            )
            .resolved_cell();

            import_references.push(reference);
            if should_add_evaluation {
                esm_evaluation_reference_idxs.push(i);
            }
        }
        anyhow::Ok(import_references)
    }
    .instrument(span)
    .await?;

    let span = tracing::trace_span!("exports");
    let exports = async {
        let esm_star_exports: Vec<ResolvedVc<Box<dyn ModuleReference>>> = eval_context
            .imports
            .reexport_namespaces()
            .map(|i| ResolvedVc::upcast(import_references[i]))
            .collect();
        let esm_exports = eval_context
            .imports
            .as_esm_exports(&import_references, &var_graph)?;

        for idx in eval_context.imports.reexports_reference_idxs() {
            esm_reexport_reference_idxs.push(idx);
        }

        anyhow::Ok(
            if !esm_exports.is_empty() || !esm_star_exports.is_empty() {
                if specified_type == SpecifiedModuleType::CommonJs {
                    SpecifiedModuleTypeIssue {
                        // TODO(PACK-4879): this should point at one of the exports
                        source: IssueSource::from_source_only(source),
                        specified_type,
                    }
                    .resolved_cell()
                    .emit();
                }

                let esm_exports = EsmExports {
                    exports: esm_exports,
                    star_exports: esm_star_exports,
                }
                .cell();

                EcmascriptExports::EsmExports(esm_exports.to_resolved().await?)
            } else if specified_type == SpecifiedModuleType::EcmaScript {
                match detect_dynamic_export(program) {
                    DetectedDynamicExportType::CommonJs => {
                        SpecifiedModuleTypeIssue {
                            // TODO(PACK-4879): this should point at the source location of the
                            // commonjs export
                            source: IssueSource::from_source_only(source),
                            specified_type,
                        }
                        .resolved_cell()
                        .emit();

                        EcmascriptExports::EsmExports(
                            EsmExports {
                                exports: Default::default(),
                                star_exports: Default::default(),
                            }
                            .resolved_cell(),
                        )
                    }
                    DetectedDynamicExportType::Namespace => EcmascriptExports::DynamicNamespace,
                    DetectedDynamicExportType::Value => EcmascriptExports::Value,
                    DetectedDynamicExportType::UsingModuleDeclarations
                    | DetectedDynamicExportType::None => EcmascriptExports::EsmExports(
                        EsmExports {
                            exports: Default::default(),
                            star_exports: Default::default(),
                        }
                        .resolved_cell(),
                    ),
                }
            } else {
                match detect_dynamic_export(program) {
                    DetectedDynamicExportType::CommonJs => EcmascriptExports::CommonJs,
                    DetectedDynamicExportType::Namespace => EcmascriptExports::DynamicNamespace,
                    DetectedDynamicExportType::Value => EcmascriptExports::Value,
                    DetectedDynamicExportType::UsingModuleDeclarations => {
                        EcmascriptExports::EsmExports(
                            EsmExports {
                                exports: Default::default(),
                                star_exports: Default::default(),
                            }
                            .resolved_cell(),
                        )
                    }
                    DetectedDynamicExportType::None => EcmascriptExports::EmptyCommonJs,
                }
            }
            .resolved_cell(),
        )
    }
    .instrument(span)
    .await?;

    Ok(EcmascriptExportsAnalysis {
        exports,
        import_references,
        esm_reexport_reference_idxs,
        esm_evaluation_reference_idxs,
    }
    .cell())
}

#[derive(Debug)]
enum DetectedDynamicExportType {
    CommonJs,
    Namespace,
    Value,
    None,
    UsingModuleDeclarations,
}

fn detect_dynamic_export(p: &Program) -> DetectedDynamicExportType {
    use swc_core::ecma::visit::{Visit, VisitWith, visit_obj_and_computed};

    if let Program::Module(m) = p {
        // Check for imports/exports
        if m.body.iter().any(|item| {
            item.as_module_decl().is_some_and(|module_decl| {
                module_decl.as_import().is_none_or(|import| {
                    !is_turbopack_helper_import(import) && !is_swc_helper_import(import)
                })
            })
        }) {
            return DetectedDynamicExportType::UsingModuleDeclarations;
        }
    }

    struct Visitor {
        cjs: bool,
        value: bool,
        namespace: bool,
        found: bool,
    }

    impl Visit for Visitor {
        visit_obj_and_computed!();

        fn visit_ident(&mut self, i: &Ident) {
            // The detection is not perfect, it might have some false positives, e. g. in
            // cases where `module` is used in some other way. e. g. `const module = 42;`.
            // But a false positive doesn't break anything, it only opts out of some
            // optimizations, which is acceptable.
            if &*i.sym == "module" || &*i.sym == "exports" {
                self.cjs = true;
                self.found = true;
            }
            if &*i.sym == "__turbopack_export_value__" {
                self.value = true;
                self.found = true;
            }
            if &*i.sym == "__turbopack_export_namespace__" {
                self.namespace = true;
                self.found = true;
            }
        }

        fn visit_expr(&mut self, n: &Expr) {
            if self.found {
                return;
            }

            if let Expr::Member(member) = n
                && member.obj.is_ident_ref_to("__turbopack_context__")
                && let MemberProp::Ident(prop) = &member.prop
            {
                const TURBOPACK_EXPORT_VALUE_SHORTCUT: &str = TURBOPACK_EXPORT_VALUE.shortcut;
                const TURBOPACK_EXPORT_NAMESPACE_SHORTCUT: &str =
                    TURBOPACK_EXPORT_NAMESPACE.shortcut;
                match &*prop.sym {
                    TURBOPACK_EXPORT_VALUE_SHORTCUT => {
                        self.value = true;
                        self.found = true;
                    }
                    TURBOPACK_EXPORT_NAMESPACE_SHORTCUT => {
                        self.namespace = true;
                        self.found = true;
                    }
                    _ => {}
                }
            }

            n.visit_children_with(self);
        }

        fn visit_stmt(&mut self, n: &Stmt) {
            if self.found {
                return;
            }
            n.visit_children_with(self);
        }
    }

    let mut v = Visitor {
        cjs: false,
        value: false,
        namespace: false,
        found: false,
    };
    p.visit_with(&mut v);
    if v.cjs {
        DetectedDynamicExportType::CommonJs
    } else if v.value {
        DetectedDynamicExportType::Value
    } else if v.namespace {
        DetectedDynamicExportType::Namespace
    } else {
        DetectedDynamicExportType::None
    }
}

pub fn is_turbopack_helper_import(import: &ImportDecl) -> bool {
    let annotations = ImportAnnotations::parse(import.with.as_deref());

    annotations.is_some_and(|a| a.get(&TURBOPACK_HELPER_WTF8).is_some())
}

pub fn is_swc_helper_import(import: &ImportDecl) -> bool {
    import.src.value.starts_with("@swc/helpers/")
}
