use std::collections::HashSet;

use crate::ast::kinds::{FieldKind, NodeKind};
use crate::ast::{AstNode, NodeRange, StructId};
use crate::hir::body_map::{BodyOwnerId, Local, LocalId};
use crate::hir::builtins::Builtin;
use crate::hir::exprs::Name;
use crate::hir::item_data::SemanticId;
use crate::hir::syntax_store::SyntaxStore;
use crate::hir::resolution::{Callable, Resolution};
use crate::hir::resolver::Resolver;
use crate::hir::types::{Primitive, Ty, TyKind, EXTERNAL_CALL_OPTIONS, option_type};
use crate::ir::def_map::{DefId, DefNamespace, TypeDefId};
use crate::hir::syntax_store::{callee_at, semantic_at};
use crate::salsa::hir_db::owner_at;
use crate::salsa::{HirDb, Db};
use crate::utilities::{log_info, print_tree, to_utf8path};
use anyhow::{Context as _, Result};
use lsp_server::{Request, Response};
use lsp_types::{
    CompletionItem as LspCompletionItem, CompletionItemKind, CompletionItemLabelDetails,
    CompletionList, CompletionParams, CompletionResponse, CompletionTriggerKind, InsertTextFormat,
};
use rustc_hash::FxHashSet;

/// What kind of completion position we're at.
#[derive(Debug)]
pub(crate) enum CompletionContextKind {
    /// Plain identifier in expression position: `foo$0`
    Expr,
    /// After a dot: `obj.$0`
    DotAccess { receiver_node: AstNode },
    BracedGroup {
        kind: GroupKind,
        callee_node: AstNode,
        existing: FxHashSet<Name>,
        insert_separator: bool,
    },
}

/// The two braced forms a call can take.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GroupKind {
    /// `f({ … })` — the braced `call_argument`, which is the whole argument
    /// list. Introduced by the call's `(`.
    NamedArgs,
    /// `f{ … }(…)` — the `call_options` prefix of a callee.
    CallOptions,
}

/// What a callee denotes, as far as a key position needs to know.
enum CalleeKind {
    /// A callable — or an overload set, whose keys are the union of every
    /// candidate's parameter names.
    Callables(Vec<Callable>),
    /// `S({ … })` — a struct construction, whose keys are its fields.
    Struct(StructId),
    ExternalFnPtr,
}

pub fn completion(db: &Db, request: Request) -> Result<Response> {
    log_info("Completion triggered");
    let params: CompletionParams = serde_json::from_value(request.params)?;
    let path = to_utf8path(&params.text_document_position.text_document.uri)?;
    if db.is_ignored_path(&path) {
        return Ok(Response {
            id: request.id,
            result: Some(serde_json::to_value(CompletionResponse::Array(Vec::new()))?),
            error: None,
        });
    }
    let position = params.text_document_position.position;

    let trigger_kind = params.context.as_ref()
        .map(|c| c.trigger_kind)
        .unwrap_or(CompletionTriggerKind::INVOKED);

    let trigger_character = params.context.as_ref()
        .and_then(|c| c.trigger_character.as_deref());

    let (file, offset) = db.convert(&path, position).context("document is not loaded")?;
    let node = db.named_node_at(file, offset).context("No node at cursor")?;
    let owner = owner_at(db, file, offset).context("no owner at cursor")?;
    let resolver = Resolver::build(db, owner);
    log_info(format!(
        "completion request for {} : {}",
        node.node().kind(),
        print_tree(&mut node.node().walk(), node.ast().source(), 2)
    ));

    let kind = detect_ctx(&node, offset, trigger_kind, trigger_character).context("unable to detect completion context")?;

    let items = match kind {
        CompletionContextKind::DotAccess { receiver_node } => {
            complete_dot(db, &resolver, owner, &receiver_node)
        }
        CompletionContextKind::Expr => complete_expr(db, &resolver, owner, offset),
        CompletionContextKind::BracedGroup {
            kind,
            callee_node,
            existing,
            insert_separator,
        } => complete_braced_group(
            db,
            owner,
            kind,
            &callee_node,
            &existing,
            insert_separator,
        ),
    };

    let result = CompletionResponse::List(CompletionList {
        is_incomplete: false,
        items: dedupe_items(items),
    });

    Ok(Response {
        id: request.id,
        result: Some(serde_json::to_value(result)?),
        error: None,
    })
}

pub(crate) fn detect_ctx(
    node: &AstNode,
    offset: u32,
    trigger_kind: CompletionTriggerKind,
    trigger_character: Option<&str>,
) -> Option<CompletionContextKind> {
    if let Some(braced) = braced_group(node, offset) {
        return Some(braced);
    }
    let dot = extract_receiver(node)
        .map(|receiver_node| CompletionContextKind::DotAccess { receiver_node });
    match trigger_kind {
        CompletionTriggerKind::TRIGGER_CHARACTER if trigger_character == Some(".") => dot,
        _ => dot.or(Some(CompletionContextKind::Expr)),
    }
}

// MARK: Dot completions
pub(crate) fn complete_dot(
    db: &Db,
    resolver: &Resolver,
    owner: DefId,
    receiver_node: &AstNode,
) -> Vec<LspCompletionItem> {
    let Some(receiver) = infer_receiver(db, owner, receiver_node) else {
        return Vec::new();
    };
    resolver
        .members_of(receiver)
        .into_values()
        .flat_map(|result| result.completion_resolutions())
        .filter_map(|member| completion_item_for_resolution(db, member))
        .collect()
}

// MARK: Braced groups — named arguments and call options

/// The braced group (`f({ … })` or `f{ … }()`) the cursor sits in when at a
/// key position. `None` in value positions (after `:`) or outside braced groups.
fn braced_group(node: &AstNode, offset: u32) -> Option<CompletionContextKind> {
    let ast = node.ast();
    let (group, kind, callee, brace) = find_braced_group(node, offset)?;
    let (at_key, insert_separator, existing) =
        scan_group_entries(group, ast.source(), brace, offset);
    at_key.then_some(CompletionContextKind::BracedGroup {
        kind,
        callee_node: callee,
        existing,
        insert_separator,
    })
}

/// Walk from the cursor node (or an immediately adjacent `call_argument` /
/// `call_options` descendant when tree-sitter places a zero-width missing `}`
/// at `offset`) up to the enclosing braced group.
fn find_braced_group(
    node: &AstNode,
    offset: u32,
) -> Option<(tree_sitter::Node<'_>, GroupKind, AstNode, u32)> {
    let start = child_group_at(node.node(), offset).unwrap_or_else(|| node.node());
    let mut current = Some(start);
    while let Some(cand) = current {
        match cand.kind_id().into() {
            NodeKind::CALL_OPTIONS => {
                let brace = open_brace_before(cand, offset)?;
                return Some((cand, GroupKind::CallOptions, node.upcast(cand), brace));
            }
            NodeKind::CALL_ARGUMENT => {
                if let Some(brace) = open_brace_before(cand, offset) {
                    let call = named_arg_call_node(cand.parent()?)?;
                    return Some((cand, GroupKind::NamedArgs, node.upcast(call), brace));
                }
            }
            NodeKind::CALL_EXPRESSION
            | NodeKind::EMIT_STATEMENT
            | NodeKind::REVERT_ARGUMENTS
            | NodeKind::MODIFIER_INVOCATION
            | NodeKind::INHERITANCE_SPECIFIER
            | NodeKind::NEW_EXPRESSION => {
                if let Some(brace) = error_brace_in(cand, offset)
                    && let Some(call) = named_arg_call_node(cand)
                {
                    return Some((cand, GroupKind::NamedArgs, node.upcast(call), brace));
                }
            }
            _ if cand.is_error() => {
                if let Some((kind, callee, brace)) = error_braced_group(cand, offset) {
                    return Some((cand, kind, node.upcast(callee), brace));
                }
            }
            _ => {}
        }
        current = cand.parent();
    }
    None
}

fn child_group_at(node: tree_sitter::Node<'_>, offset: u32) -> Option<tree_sitter::Node<'_>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.start_byte() as u32 <= offset && offset <= child.end_byte() as u32 {
            if matches!(
                child.kind_id().into(),
                NodeKind::CALL_ARGUMENT | NodeKind::CALL_OPTIONS
            ) {
                return Some(child);
            }
            if let Some(found) = child_group_at(child, offset) {
                return Some(found);
            }
        }
    }
    None
}

fn error_brace_in(node: tree_sitter::Node<'_>, offset: u32) -> Option<u32> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|c| c.is_error())
        .find_map(|err| open_brace_before(err, offset))
}

fn open_brace_before(node: tree_sitter::Node<'_>, offset: u32) -> Option<u32> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == "{" && child.start_byte() as u32 <= offset)
        .map(|brace| brace.start_byte() as u32)
}

/// When an unclosed `{` folds into an `ERROR` node, locate the last unmatched
/// `"{"` child before `offset` and the callee node immediately preceding `"("`/`"{"`.
fn error_braced_group(
    error: tree_sitter::Node<'_>,
    offset: u32,
) -> Option<(GroupKind, tree_sitter::Node<'_>, u32)> {
    let mut cursor = error.walk();
    let mut prev = None;
    let mut prev2 = None;
    for child in error.children(&mut cursor) {
        if child.kind_id() == NodeKind::COMMENT || child.start_byte() as u32 > offset {
            continue;
        }
        if child.kind() == "{" {
            let brace = child.start_byte() as u32;
            let (kind, callee) = if prev.is_some_and(|n: tree_sitter::Node<'_>| n.kind() == "(") {
                (GroupKind::NamedArgs, prev2?)
            } else {
                (GroupKind::CallOptions, prev?)
            };
            if callee.is_named() {
                return Some((kind, callee, brace));
            }
        }
        if child.is_error()
            && let Some(found) = error_braced_group(child, offset)
        {
            return Some(found);
        }
        prev2 = prev;
        prev = Some(child);
    }
    None
}

fn named_arg_call_node(parent: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    match parent.kind_id().into() {
        NodeKind::CALL_EXPRESSION
        | NodeKind::EMIT_STATEMENT
        | NodeKind::MODIFIER_INVOCATION
        | NodeKind::INHERITANCE_SPECIFIER
        | NodeKind::NEW_EXPRESSION => Some(parent),
        NodeKind::REVERT_ARGUMENTS => parent.parent(),
        _ => None,
    }
}

/// Inspect direct children of the group between `{` and `offset` to determine:
/// 1. whether `offset` is in a key position (not after `:` inside an entry),
/// 2. whether completing here should append `: $0`,
/// 3. the set of keys already written in this group.
fn scan_group_entries(
    group: tree_sitter::Node<'_>,
    source: &str,
    brace: u32,
    offset: u32,
) -> (bool, bool, FxHashSet<Name>) {
    let mut existing = FxHashSet::default();
    let mut at_key = true;
    let mut insert_separator = true;
    let mut cursor = group.walk();

    for child in group.children(&mut cursor) {
        let start = child.start_byte() as u32;
        let end = child.end_byte() as u32;
        if start <= brace || child.kind_id() == NodeKind::COMMENT {
            continue;
        }
        if matches!(
            child.kind_id().into(),
            NodeKind::CALL_STRUCT_ARGUMENT | NodeKind::NAMED_ARGUMENT
        ) {
            let colon_end = child
                .children(&mut child.walk())
                .find(|c| c.kind() == ":")
                .map(|c| c.end_byte() as u32);
            if start <= offset && offset <= end {
                if colon_end.is_some_and(|colon| offset >= colon) {
                    return (false, true, existing);
                }
                insert_separator = colon_end.is_none();
            } else if end <= offset
                && let Some(name) = child.child_by_field_id(FieldKind::NAME.into())
            {
                existing.insert(source[name.byte_range()].trim().into());
            }
        } else if start < offset {
            match child.kind() {
                ":" => at_key = false,
                "," => {
                    at_key = true;
                    insert_separator = true;
                }
                _ => {}
            }
        }
    }
    (at_key, insert_separator, existing)
}

// MARK: Named-argument and call-option keys

pub(crate) fn complete_braced_group(
    db: &Db,
    owner: DefId,
    kind: GroupKind,
    callee_node: &AstNode,
    existing: &FxHashSet<Name>,
    insert_separator: bool,
) -> Vec<LspCompletionItem> {
    let Some(callee) = infer_callee(db, owner, callee_node) else {
        return Vec::new();
    };
    let (keys, item_kind) = match (kind, callee) {
        (GroupKind::NamedArgs, CalleeKind::Callables(callables)) => {
            (callable_keys(db, &callables), CompletionItemKind::FIELD)
        }
        (GroupKind::NamedArgs, CalleeKind::Struct(id)) => {
            let data = db.struct_data(id);
            let keys = data
                .fields
                .iter()
                .map(|(_, field)| {
                    (
                        field.name.clone(),
                        data.syntax_store.types[field.ty_ref].to_string(&data.syntax_store.types),
                    )
                })
                .collect();
            (keys, CompletionItemKind::FIELD)
        }
        (GroupKind::CallOptions, CalleeKind::Callables(callables)) => {
            let Some(options) = callables.iter().find_map(Callable::options) else {
                return Vec::new();
            };
            (option_keys(options), CompletionItemKind::PROPERTY)
        }
        (GroupKind::CallOptions, CalleeKind::ExternalFnPtr) => {
            (option_keys(EXTERNAL_CALL_OPTIONS), CompletionItemKind::PROPERTY)
        }
        (GroupKind::NamedArgs, CalleeKind::ExternalFnPtr)
        | (GroupKind::CallOptions, CalleeKind::Struct(_)) => return Vec::new(),
    };
    key_items(keys, existing, item_kind, insert_separator)
}

fn option_keys(options: &[&str]) -> Vec<(Name, String)> {
    options
        .iter()
        .filter_map(|name| option_type(name).map(|ty| (Name::from(*name), ty.to_string())))
        .collect()
}

/// The union of an overload set's parameter names, keeping the first type seen
/// for each name.
fn callable_keys(db: &Db, callables: &[Callable]) -> Vec<(Name, String)> {
    let mut keys: Vec<(Name, String)> = Vec::new();
    for callable in callables {
        let Some(parameters) = callable.named_parameters(db) else {
            continue;
        };
        for (name, ty) in parameters.iter() {
            if !name.is_empty() && !keys.iter().any(|(seen, _)| seen == name) {
                keys.push((name.clone(), ty.clone()));
            }
        }
    }
    keys
}

/// One item per key, skipping the ones already written.
fn key_items(
    keys: Vec<(Name, String)>,
    existing: &FxHashSet<Name>,
    kind: CompletionItemKind,
    insert_separator: bool,
) -> Vec<LspCompletionItem> {
    keys.into_iter()
        .filter(|(name, _)| !existing.contains(name))
        .map(|(name, ty)| LspCompletionItem {
            label: name.to_string(),
            kind: Some(kind),
            label_details: Some(CompletionItemLabelDetails {
                detail: None,
                description: Some(ty),
            }),
            filter_text: Some(name.to_string()),
            insert_text: Some(if insert_separator {
                format!("{name}: $0")
            } else {
                name.to_string()
            }),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        })
        .collect()
}

/// What the callee denotes, read from the inference table.
///
/// The call itself has no resolution while its argument list is incomplete, so
/// this reads the *callee's* resolution — the call is only its remapped range.
/// `None` when the callee has no recorded resolution: an unresolved name, or a
/// value of function type, whose parameter names are not knowable.
fn infer_callee(db: &Db, owner: DefId, callee_node: &AstNode) -> Option<CalleeKind> {
    let range = NodeRange::from(&callee_node.node());
    let id = callee_at(db, owner, range)?;
    let inference = db.inference(owner);
    if let Some(resolution) = inference.expr_resolution(id) {
        match resolution {
            Resolution::Callable(callable) => {
                return Some(CalleeKind::Callables(vec![*callable]));
            }
            Resolution::Callables(callables) => {
                return Some(CalleeKind::Callables(callables.to_vec()));
            }
            Resolution::Type(ty) => match ty.peel_loc(db).kind(db) {
                TyKind::TypeDef(TypeDefId::Struct(id)) => return Some(CalleeKind::Struct(id)),
                TyKind::TypeDef(TypeDefId::Contract(id))
                    if matches!(
                        callee_node.node().kind_id().into(),
                        NodeKind::MODIFIER_INVOCATION | NodeKind::INHERITANCE_SPECIFIER
                    ) =>
                {
                    return Some(CalleeKind::Callables(vec![Callable::new_constructor(id)]));
                }
                _ => {}
            },
            _ => {}
        }
    }
    // An external pointer is an option host — `Fn::options` answers exactly that.
    matches!(
        inference.expr_ty(id).kind(db),
        TyKind::FnPtr(sig) if sig.options().is_some()
    )
    .then_some(CalleeKind::ExternalFnPtr)
}

fn valid_member_expr(node: &tree_sitter::Node) -> bool {
    node.kind_id() == NodeKind::INCOMPLETE_MEMBER_EXPRESSION
        || node.kind_id() == NodeKind::MEMBER_EXPRESSION
}

fn find_missing_member_property<'a>(node: tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
    if node.kind_id() == NodeKind::MISSING_MEMBER_PROPERTY
        || (node.kind_id() == NodeKind::IDENTIFIER && node.is_missing())
    {
        return Some(node);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(property) = find_missing_member_property(child) {
            return Some(property);
        }
    }
    None
}

/// Extract the receiver node from a complete or incomplete member expression.
fn extract_receiver(node: &AstNode) -> Option<AstNode> {
    let current = node.node();

    let member = find_missing_member_property(current)
        .and_then(|property| property.parent())
        .filter(valid_member_expr)
        .or_else(|| valid_member_expr(&current).then_some(current))
        .or_else(|| current.parent().filter(valid_member_expr))?;

    member
        .child_by_field_id(FieldKind::OBJECT.into())
        .map(|n| node.upcast(n))
}

/// Resolve and infer the receiver expression for dot completion.
fn infer_receiver(db: &Db, owner: DefId, receiver_node: &AstNode) -> Option<Ty> {
    let range = NodeRange::from(&receiver_node.node());
    let inference = db.inference(owner);
    let id = semantic_at(db, owner, range)?;
    Some(match id {
        SemanticId::Type(ty_ref_id) => Ty::new(db, TyKind::Type(inference.ty(ty_ref_id))),
        _ => inference.sem_ty(id),
    })
}

fn completion_item_for_resolution(
    db: &Db,
    resolution: Resolution,
) -> Option<LspCompletionItem> {
    match resolution {
        // A public state-variable getter — present it as a zero-argument
        // method call.
        Resolution::Callable(callable) if callable.is_getter() => {
            let DefId::Var(id) = callable.def()? else {
                return None;
            };
            let name = db.var_data(id).name.to_string();
            Some(LspCompletionItem {
                label: format!("{name}()"),
                kind: Some(CompletionItemKind::METHOD),
                label_details: Some(CompletionItemLabelDetails {
                    detail: None,
                    description: Some("getter".to_string()),
                }),
                filter_text: Some(name.clone()),
                insert_text: Some(format!("{name}()")),
                ..Default::default()
            })
        }
        Resolution::Callable(callable) => {
            if let Some(def) = callable.def() {
                let name = match def {
                    DefId::Function(id) => db.function_data(id).name.to_string(),
                    DefId::Modifier(id) => db.modifier_data(id).name.to_string(),
                    _ => return None,
                };
                let bound = callable.bound() as usize;
                let (label, insert_text) = callable_label_and_insert(db, &def, &name, bound);
                return Some(LspCompletionItem {
                    label,
                    kind: Some(CompletionItemKind::METHOD),
                    label_details: label_details_for_def(db, &def, bound),
                    documentation: documentation_for_def(db, &def),
                    filter_text: Some(name),
                    insert_text: Some(insert_text),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    ..Default::default()
                });
            }
            let builtin = callable.builtin()?;
            let sig = builtin.call_sig(db);
            let params = sig
                .params
                .iter()
                .map(|ty| ty.display(db))
                .collect::<Vec<_>>()
                .join(", ");
            let ret = match sig.returns.as_ref() {
                [] => String::new(),
                [single] => format!(" returns {}", single.display(db)),
                many => format!(
                    " returns ({})",
                    many.iter()
                        .map(|ty| ty.display(db))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            };
            let label = if sig.params.is_empty() {
                format!("{}()", builtin.name())
            } else {
                format!("{}(\u{2026})", builtin.name())
            };
            Some(LspCompletionItem {
                label,
                kind: Some(CompletionItemKind::METHOD),
                label_details: Some(CompletionItemLabelDetails {
                    detail: Some(" (builtin)".to_string()),
                    description: Some(format!("function ({params}){ret}")),
                }),
                filter_text: Some(builtin.name().to_string()),
                insert_text: Some(format!("{}($0)", builtin.name())),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                documentation: Some(lsp_types::Documentation::String(
                    builtin.doc(db).to_string(),
                )),
                ..Default::default()
            })
        }
        Resolution::Callables(callables) => callables.into_iter().find_map(|callable| {
            completion_item_for_resolution(db, Resolution::Callable(callable))
        }),
        Resolution::Field(field) => {
            if let Some((DefId::Struct(struct_id), field_id)) = field.struct_field() {
                let data = db.struct_data(struct_id);
                let field_data = &data.fields[field_id];
                let ty = data.syntax_store.types[field_data.ty_ref].to_string(&data.syntax_store.types);
                return Some(LspCompletionItem {
                    label: field_data.name.to_string(),
                    kind: Some(CompletionItemKind::FIELD),
                    label_details: Some(CompletionItemLabelDetails {
                        detail: None,
                        description: Some(ty),
                    }),
                    ..Default::default()
                });
            }
            let builtin = field.builtin_field()?;
            Some(LspCompletionItem {
                label: builtin.name().to_string(),
                kind: Some(CompletionItemKind::FIELD),
                label_details: Some(CompletionItemLabelDetails {
                    detail: Some(" (builtin)".to_string()),
                    description: Some(builtin.ty(db).display(db)),
                }),
                documentation: Some(lsp_types::Documentation::String(
                    builtin.doc(db).to_string(),
                )),
                ..Default::default()
            })
        }
        Resolution::Variant(variant) => {
            let data = db.enum_data(variant.enum_id);
            Some(LspCompletionItem {
                label: data.variants[variant.id].name.to_string(),
                kind: Some(CompletionItemKind::ENUM_MEMBER),
                label_details: Some(CompletionItemLabelDetails {
                    detail: None,
                    description: Some(format!("{}.{}", data.name, data.variants[variant.id].name)),
                }),
                ..Default::default()
            })
        }
        _ => None,
    }
}

// MARK: Expr completions

pub(crate) fn complete_expr(
    db: &Db,
    resolver: &Resolver,
    owner: DefId,
    offset: u32,
) -> Vec<LspCompletionItem> {
    let mut acc = Vec::new();

    // 1. Locals (if inside a function/modifier body)
    if let Some(body) = resolver.body() {
        let local_ranges = match owner {
            DefId::Function(id) => {
                Some(db.body_source_map(BodyOwnerId::Function(id)))
            }
            DefId::Modifier(id) => Some(
                db.body_source_map(BodyOwnerId::Modifier(id)),
            ),
            _ => None,
        };
        for (local_id, local) in body.locals.iter() {
            let visible = local_ranges.as_ref().is_some_and(|map| {
                map.local_ranges
                    .get(local_id)
                    .is_some_and(|range| range.start <= offset)
            });
            if visible {
                acc.push(completion_item_for_local(local, &body.syntax_store));
            }
        }
    }

    // 2. Def-map scope walk (contracts, functions, types, events, errors, vars)
    acc.extend(iterate_scope(db, resolver, owner, |name, def_id, _namespace, bound| {
        let kind = completion_kind_for_def(&def_id);
        let (label, insert_text, filter_text) =
            callable_completion_text(db, &def_id, name, bound);
        let insert_text_format = insert_text.as_ref().map(|_| InsertTextFormat::SNIPPET);
        Some(LspCompletionItem {
            label,
            kind: Some(kind),
            label_details: label_details_for_def(db, &def_id, bound),
            documentation: documentation_for_def(db, &def_id),
            filter_text,
            insert_text,
            insert_text_format,
            ..Default::default()
        })
    }));

    // 3. Builtins (msg, block, tx, abi, this, super, primitives)
    acc.extend(completion_builtins(db, resolver));

    // 4. Declaration snippets where the current container permits them.
    acc.extend(complete_item_list(owner));

    acc
}

fn complete_item_list(owner: DefId) -> Vec<LspCompletionItem> {
    let keywords = [
        ("contract", "contract $1 {\n\t$0\n}"),
        ("interface", "interface $1 {\n\t$0\n}"),
        ("library", "library $1 {\n\t$0\n}"),
        ("function", "function $1($2) $3 {\n\t$0\n}"),
        ("modifier", "modifier $1($2) {\n\t$0\n}"),
        ("constructor", "constructor($1) {\n\t$0\n}"),
        ("fallback", "fallback() external $1 {\n\t$0\n}"),
        ("receive", "receive() external payable {\n\t$0\n}"),
        ("event", "event $1($2);"),
        ("error", "error $1($2);"),
        ("struct", "struct $1 {\n\t$0\n}"),
        ("enum", "enum $1 {\n\t$0\n}"),
        ("mapping", "mapping($1 => $2) $3;"),
        ("using", "using $1 for $2;"),
        ("type", "type $1 is $2;"),
    ];

    let is_file = matches!(owner, DefId::File(_));
    let is_container = matches!(
        owner,
        DefId::Contract(_) | DefId::Interface(_) | DefId::Library(_)
    );
    if !is_file && !is_container {
        return Vec::new();
    }

    keywords
        .iter()
        .filter(|(keyword, _)| {
            if is_file {
                !matches!(
                    *keyword,
                    "constructor" | "fallback" | "receive" | "modifier" | "mapping"
                )
            } else {
                !matches!(*keyword, "contract" | "interface" | "library")
            }
        })
        .map(|(keyword, snippet)| LspCompletionItem {
            label: keyword.to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            insert_text: Some(snippet.to_string()),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            ..Default::default()
        })
        .collect()
}

/// Walk the def-map scope chain from the current container upward,
/// calling `f` for each visible name. `bound` is the callable's number of
/// implicitly-bound receiver parameters (nonzero for `using` attachments).
fn iterate_scope(
    db: &Db,
    resolver: &Resolver,
    owner: DefId,
    mut f: impl FnMut(&Name, DefId, &DefNamespace, usize) -> Option<LspCompletionItem>,
) -> Vec<LspCompletionItem> {
    let mut acc = Vec::new();
    let mut seen: rustc_hash::FxHashSet<Name> = rustc_hash::FxHashSet::default();

    // Walk def-map scopes from container upward
    let defmap = resolver.def_map(&owner);
    if let Some(data) = defmap.defs.get(&owner) {
        let start = data.child_scope.unwrap_or(data.scope);
        let mut scope_id = start;

        loop {
            let scope = &defmap.scopes[scope_id];
            match scope.owner {
                owner @ (DefId::Contract(_) | DefId::Interface(_) | DefId::Library(_)) => {
                    for (name, result) in resolver.direct_members_of(owner) {
                        if seen.contains(&name) {
                            continue;
                        }
                        let mut found = false;
                        for resolution in result.completion_resolutions() {
                            let (def, bound) = match resolution {
                                Resolution::Var(def) => (Some(def), 0),
                                Resolution::Callable(callable) => {
                                    (callable.def(), callable.bound() as usize)
                                }
                                Resolution::Type(ty) => (ty.def_id(db), 0),
                                Resolution::Module(file) => (Some(DefId::File(file)), 0),
                                _ => (None, 0),
                            };
                            let Some(def) = def else {
                                continue;
                            };
                            if let Some(item) = f(&name, def, &DefNamespace::from(&def), bound) {
                                found = true;
                                acc.push(item);
                            }
                        }
                        if found {
                            seen.insert(name);
                        }
                    }
                }
                _ => {
                    for (name, scope_data) in &scope.by_name {
                        if seen.contains(name) {
                            continue;
                        }
                        for def_id in &scope_data.defs {
                            if let Some(item) = f(name, *def_id, &scope_data.namespace, 0) {
                                seen.insert(name.clone());
                                acc.push(item);
                                break;
                            }
                        }
                    }
                }
            }
            scope_id = match scope.parent {
                Some(p) => p,
                None => break,
            };
        }
    }

    acc
}

fn dedupe_items(items: Vec<LspCompletionItem>) -> Vec<LspCompletionItem> {
    // Callable labels carry their parameter list, so overloads already
    // differ; the description distinguishes same-label entries whose
    // declarations differ elsewhere (mutability, returns, kind).
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| {
            seen.insert((
                item.label.clone(),
                item.label_details
                    .as_ref()
                    .and_then(|details| details.description.clone()),
            ))
        })
        .collect()
}

/// Map a DefId to an LSP CompletionItemKind.
fn completion_kind_for_def(def_id: &DefId) -> CompletionItemKind {
    match def_id {
        DefId::Function(_) => CompletionItemKind::FUNCTION,
        DefId::Modifier(_) => CompletionItemKind::FUNCTION,
        DefId::Contract(_) => CompletionItemKind::CLASS,
        DefId::Interface(_) => CompletionItemKind::INTERFACE,
        DefId::Library(_) => CompletionItemKind::CLASS,
        DefId::Struct(_) => CompletionItemKind::STRUCT,
        DefId::Enum(_) => CompletionItemKind::ENUM,
        DefId::Event(_) => CompletionItemKind::FUNCTION,
        DefId::Error(_) => CompletionItemKind::FUNCTION,
        DefId::Var(_) => CompletionItemKind::VARIABLE,
        DefId::Udvt(_) => CompletionItemKind::TYPE_PARAMETER,
        DefId::Import(_) => CompletionItemKind::MODULE,
        DefId::Using(_) => CompletionItemKind::KEYWORD,
        DefId::File(_) => CompletionItemKind::FILE,
    }
}

/// Collect NatSpec documentation for a DefId, formatted as markdown for the
/// completion popup sidebar.
fn documentation_for_def(db: &Db, def_id: &DefId) -> Option<lsp_types::Documentation> {
    db.docs(*def_id).map(|d| {
        lsp_types::Documentation::MarkupContent(lsp_types::MarkupContent {
            kind: lsp_types::MarkupKind::Markdown,
            value: d,
        })
    })
}

/// A callable def's parameter types as `ty, ty`, skipping the `bound`
/// leading receiver parameters a `using`-attached function carries (only
/// functions are attachable, so `bound` is zero elsewhere).
fn callable_params(db: &Db, def_id: &DefId, bound: usize) -> Option<String> {
    Some(match def_id {
        DefId::Function(id) => {
            let data = db.function_data(*id);
            let ids: Vec<LocalId> = data.args.iter().copied().skip(bound).collect();
            format_param_types(&data.parameters, &ids, &data.syntax_store)
        }
        DefId::Modifier(id) => {
            let data = db.modifier_data(*id);
            let ids: Vec<LocalId> = data
                .parameters
                .iter()
                .map(|(id, _)| id)
                .skip(bound)
                .collect();
            format_param_types(&data.parameters, &ids, &data.syntax_store)
        }
        DefId::Event(id) => {
            let data = db.event_data(*id);
            let ids: Vec<LocalId> = data
                .parameters
                .iter()
                .map(|(id, _)| id)
                .skip(bound)
                .collect();
            format_param_types(&data.parameters, &ids, &data.syntax_store)
        }
        DefId::Error(id) => {
            let data = db.error_data(*id);
            let ids: Vec<LocalId> = data
                .parameters
                .iter()
                .map(|(id, _)| id)
                .skip(bound)
                .collect();
            format_param_types(&data.parameters, &ids, &data.syntax_store)
        }
        _ => return None,
    })
}

/// Generate label details for a DefId.
/// `detail` is rendered right after the label (e.g. `(address, uint256)` for functions).
/// `description` is rendered faded at the end (e.g. the containing contract name).
/// `bound` drops the receiver parameter an attached (`using`) function carries.
fn label_details_for_def(
    db: &Db,
    def_id: &DefId,
    bound: usize,
) -> Option<CompletionItemLabelDetails> {
    match def_id {
        DefId::Function(id) => {
            let data = db.function_data(*id);
            let params = callable_params(db, def_id, bound).unwrap_or_default();
            let ret = format_param_types(&data.parameters, &data.rets, &data.syntax_store);
            let mut_str = data.mutability.as_str();
            let ret_str = if ret.is_empty() {
                String::new()
            } else {
                format!(" returns {ret}")
            };
            Some(CompletionItemLabelDetails {
                detail: Some(" (function)".to_string()),
                description: Some(format!("function ({params}) {mut_str}{ret_str}")),
            })
        }
        DefId::Modifier(_) => {
            let params = callable_params(db, def_id, bound).unwrap_or_default();
            Some(CompletionItemLabelDetails {
                detail: Some(" (modifier)".to_string()),
                description: Some(format!("modifier ({params})")),
            })
        }
        DefId::Event(_) => {
            let params = callable_params(db, def_id, bound).unwrap_or_default();
            Some(CompletionItemLabelDetails {
                detail: Some(" (event)".to_string()),
                description: Some(format!("event ({params})")),
            })
        }
        DefId::Error(_) => {
            let params = callable_params(db, def_id, bound).unwrap_or_default();
            Some(CompletionItemLabelDetails {
                detail: Some(" (error)".to_string()),
                description: Some(format!("error ({params})")),
            })
        }
        DefId::Var(id) => {
            let data = db.var_data(*id);
            let ty = data.syntax_store.types[data.ty_ref].to_string(&data.syntax_store.types);
            Some(CompletionItemLabelDetails {
                detail: Some(format!(" ({})", data.kind)),
                description: Some(ty),
            })
        }
        DefId::Struct(id) => {
            let data = db.struct_data(*id);
            Some(CompletionItemLabelDetails {
                detail: None,
                description: Some(format!("struct {}", data.name)),
            })
        }
        DefId::Enum(id) => {
            let data = db.enum_data(*id);
            Some(CompletionItemLabelDetails {
                detail: None,
                description: Some(format!("enum {}", data.name)),
            })
        }
        DefId::Udvt(id) => {
            let data = db.udvt_data(*id);
            Some(CompletionItemLabelDetails {
                detail: None,
                description: Some(format!("is {}", data.underlying)),
            })
        }
        DefId::Contract(_) => Some(CompletionItemLabelDetails {
            detail: None,
            description: Some("contract".to_string()),
        }),
        DefId::Interface(_) => Some(CompletionItemLabelDetails {
            detail: None,
            description: Some("interface".to_string()),
        }),
        DefId::Library(_) => Some(CompletionItemLabelDetails {
            detail: None,
            description: Some("library".to_string()),
        }),
        _ => None,
    }
}

/// Format parameter types only as `type, type` (for label details).
fn format_param_types(
    arena: &la_arena::Arena<Local>,
    ids: &[LocalId],
    store: &SyntaxStore,
) -> String {
    ids.iter()
        .map(|id| store.types[*arena[*id].ty_ref()].to_string(&store.types))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build a completion item for a local variable.
fn completion_item_for_local(local: &Local, store: &SyntaxStore) -> LspCompletionItem {
    let ty = store.types[*local.ty_ref()].to_string(&store.types);
    LspCompletionItem {
        label: local.name().to_string(),
        kind: Some(CompletionItemKind::VARIABLE),
        label_details: Some(CompletionItemLabelDetails {
            // `detail` is appended straight after the label, so it carries its
            // own separator — otherwise `amount` + `uint256` reads `amountuint256`.
            detail: Some(format!(" {ty}")),
            description: Some(local.kind().to_string()),
        }),
        ..Default::default()
    }
}

/// Build `(label, insert_text)` for a callable def (function/modifier/event/error).
///
/// The label shows the unbound parameter list — `name(ty, ty)` — so
/// overloads stay distinct; `bound` drops the receiver parameters a
/// `using`-attached function already carries. `insert_text` is a snippet
/// `name($0)` so the cursor lands inside the parens.
fn callable_label_and_insert(
    db: &Db,
    def_id: &DefId,
    name: &str,
    bound: usize,
) -> (String, String) {
    let label = format!("{name}({})", callable_params(db, def_id, bound).unwrap_or_default());
    let insert_text = format!("{name}($0)");
    (label, insert_text)
}

/// Build `(label, insert_text, filter_text)` for a def in the scope walk.
///
/// For callables (functions, modifiers, events, errors): label gets parens,
/// insert_text is a snippet, filter_text is the bare name.
/// For everything else: label is the bare name, no insert_text, no filter_text.
fn callable_completion_text(
    db: &Db,
    def_id: &DefId,
    name: &Name,
    bound: usize,
) -> (String, Option<String>, Option<String>) {
    match def_id {
        DefId::Function(_) | DefId::Modifier(_) | DefId::Event(_) | DefId::Error(_) => {
            let name_str = name.to_string();
            let (label, insert_text) = callable_label_and_insert(db, def_id, &name_str, bound);
            (label, Some(insert_text), Some(name_str))
        }
        _ => (name.to_string(), None, None),
    }
}

/// Builtin globals and primitives for completion.
fn completion_builtins(db: &Db, resolver: &Resolver) -> Vec<LspCompletionItem> {
    let mut acc = Vec::new();
    for global in resolver.builtins().globals() {
        match global {
            Builtin::Obj(object) => acc.push(LspCompletionItem {
                label: object.name().to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                label_details: Some(CompletionItemLabelDetails {
                    detail: None,
                    description: Some(object.doc().to_string()),
                }),
                ..Default::default()
            }),
            Builtin::Fn(function) => acc.push(LspCompletionItem {
                label: format!("{}()", function.name()),
                kind: Some(CompletionItemKind::FUNCTION),
                label_details: Some(CompletionItemLabelDetails {
                    detail: Some(format!(
                        " ({})",
                        function
                            .call_sig(db)
                            .params
                            .iter()
                            .map(|ty| ty.display(db))
                            .collect::<Vec<_>>()
                            .join(", "),
                    )),
                    description: Some(function.doc(db).to_string()),
                }),
                ..Default::default()
            }),
            Builtin::Field(_) => {}
        }
    }

    // Primitives
    for prim in Primitive::all_primitives() {
        acc.push(LspCompletionItem {
            label: prim.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        });
    }

    // this / super
    acc.push(LspCompletionItem {
        label: "this".to_string(),
        kind: Some(CompletionItemKind::KEYWORD),
        label_details: Some(CompletionItemLabelDetails {
            detail: None,
            description: Some("Current contract instance".to_string()),
        }),
        ..Default::default()
    });
    acc.push(LspCompletionItem {
        label: "super".to_string(),
        kind: Some(CompletionItemKind::KEYWORD),
        label_details: Some(CompletionItemLabelDetails {
            detail: None,
            description: Some("Inherited contract".to_string()),
        }),
        ..Default::default()
    });

    acc
}

