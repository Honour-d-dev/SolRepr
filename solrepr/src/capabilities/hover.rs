use anyhow::{Context as _, Ok};
use lsp_server::{Request, Response};
use lsp_types::{HoverContents, HoverParams, MarkupContent, MarkupKind};

use crate::ast::{
    ContractId, EnumId, ErrorId, EventId, FunctionId, FunctionKind, InterfaceId, LibraryId,
    ModifierId, NodeRange, StructId, VarId,
};
use crate::hir::body_map::{BodyOwnerId, Local, LocalId};
use crate::hir::builtins::ParamSchema;
use crate::hir::exprs::{Expr, ExprId, Literal};
use crate::hir::item_data::{EnumData, Field, FieldId, SemanticId, VariantId};
use crate::hir::syntax_store::SyntaxStore;
use crate::hir::resolution::{Callable, Resolution};
use crate::hir::types::{
    Location, Mutability, RefKind, Ty, TyKind, TyName, VariableKind, Visibility,
};
use crate::ir::def_map::DefId;
use crate::hir::syntax_store::semantic_at;
use crate::salsa::hir_db::owner_at;
use crate::salsa::{File, HirDb, RootDb, Db};
use crate::utilities::{log_info, to_url, to_utf8path};

pub fn code_block(code: &str) -> String {
    format!("```solidity \n\n{code}\n```")
}

struct HoverInfo {
    title: String,
    signature: Option<String>,
    documentation: Option<String>,
    definition_link: Option<String>,
}

impl HoverInfo {
    fn render(self) -> String {
        let mut parts = vec![format!("### {}\n___\n", self.title)];
        if let Some(signature) = self.signature {
            parts.push(code_block(&signature));
        }
        if let Some(documentation) = self.documentation.filter(|doc| !doc.is_empty()) {
            parts.push(documentation);
        }
        if let Some(link) = self.definition_link {
            let label = if self.title.starts_with("File") {
                "Go to file"
            } else {
                "Go to declaration"
            };
            parts.push(format!("[{label}]({link})"));
        }
        parts.join("\n\n")
    }

    fn declaration(title: impl Into<String>, body: String, link: Option<String>) -> String {
        Self {
            title: title.into(),
            signature: None,
            documentation: Some(body),
            definition_link: link,
        }
        .render()
    }
}

pub(crate) struct Hover<'db> {
    pub(crate) db: &'db Db,
}

impl<'db> Hover<'db> {
    fn hover_target(&self, def_id: &DefId, range: NodeRange) -> anyhow::Result<String> {
        let owner = *def_id;
        let inference = self.db.inference(owner);
        let id = semantic_at(self.db, owner, range).context("No inference found")?;
        match id {
            SemanticId::Expr(expr) => inference
                .expr_resolution(expr)
                .and_then(|resolution| self.hover_resolution(resolution)),
            SemanticId::Local(local) => self.hover_local(owner, local),
            SemanticId::Field(field) => self.hover_field(owner, field),
            SemanticId::Variant(variant) => self.hover_variant(owner, variant),
            SemanticId::Type(ty) => self.hover_ty(inference.ty(ty).ty()),
        }
        .context("No resolution found")
    }

    fn hover_local(&self, owner: DefId, local_id: LocalId) -> Option<String> {
        match owner {
            DefId::Function(id) => {
                let body = self.db.body_map(BodyOwnerId::Function(id));
                Some(self.format_local(&body.locals[local_id], &body.syntax_store))
            }
            DefId::Modifier(id) => {
                let body = self.db.body_map(BodyOwnerId::Modifier(id));
                Some(self.format_local(&body.locals[local_id], &body.syntax_store))
            }
            DefId::Event(id) => {
                let data = self.db.event_data(id);
                Some(self.format_local(&data.parameters[local_id], &data.syntax_store))
            }
            DefId::Error(id) => {
                let data = self.db.error_data(id);
                Some(self.format_local(&data.parameters[local_id], &data.syntax_store))
            }
            _ => None,
        }
    }

    fn hover_field(&self, owner: DefId, field_id: FieldId) -> Option<String> {
        let DefId::Struct(struct_id) = owner else {
            return None;
        };
        let struct_data = self.db.struct_data(struct_id);
        let (file, _) = owner.file_and_id();
        Some(self.format_field(&struct_data.fields[field_id], &struct_data.syntax_store, file))
    }

    fn hover_variant(&self, owner: DefId, variant_id: VariantId) -> Option<String> {
        let DefId::Enum(enum_id) = owner else {
            return None;
        };
        let enum_data = self.db.enum_data(enum_id);
        let (file, _) = owner.file_and_id();
        Some(self.format_variant(&enum_data, variant_id, file))
    }

    fn hover_ty(&self, ty: Ty) -> Option<String> {
        if let TyKind::Module(file) = ty.kind(self.db) {
            return self.hover_file(file);
        }
        if let Some(def_id) = ty.def_id(self.db) {
            self.hover_def(&def_id).ok()
        } else {
            self.format_resolved_type(ty)
        }
    }

    fn hover_file(&self, file: File) -> Option<String> {
        let path = file.path(self.db);
        let file_name = path.file_name().unwrap_or_default();
        Some(
            HoverInfo {
                title: format!("File: {file_name}"),
                signature: None,
                documentation: None,
                definition_link: Some(to_url(path.as_path()).to_string()),
            }
            .render(),
        )
    }

    fn hover_resolution(&self, res: &Resolution) -> Option<String> {
        match res {
            Resolution::Local(local) => self.hover_local(local.owner, local.id),
            Resolution::Literal(literal) => Some(self.format_literal(literal)),
            Resolution::Callable(callable) => self.hover_callable(callable),
            Resolution::Var(def) => self.hover_def(def).ok(),
            Resolution::This(def) => self.hover_def(def).ok(),
            Resolution::Type(ty) => self.hover_ty(ty.ty()),
            Resolution::Module(file) => self.hover_file(*file),
            Resolution::Callables(callables) => {
                let hovers = callables
                    .iter()
                    .filter_map(|callable| self.hover_callable(callable))
                    .collect::<Vec<_>>();
                (!hovers.is_empty()).then(|| hovers.join("\n---\n"))
            }
            Resolution::Variant(variant) => {
                self.hover_variant(DefId::Enum(variant.enum_id), variant.id)
            }
            Resolution::Field(field) => match field.struct_field() {
                Some((owner, field_id)) => self.hover_field(owner, field_id),
                None => field.builtin_field().map(|builtin| {
                    Self::format_with_doc(
                        &format!("{} {}", builtin.ty(self.db).display(self.db), builtin.name()),
                        Some(builtin.doc(self.db).to_string()),
                    )
                }),
            },
            Resolution::Builtin(object) => Some(
                HoverInfo {
                    title: "Builtin".into(),
                    signature: Some(object.name().to_string()),
                    documentation: Some(object.doc().to_string()),
                    definition_link: None,
                }
                .render(),
            ),
            Resolution::MetaType(_) => Some(
                HoverInfo {
                    title: "Meta_type".into(),
                    signature: Some("type(...)".into()), //we can show more here
                    documentation: None,
                    definition_link: None,
                }
                .render(),
            ),
            Resolution::Super(def) => self
                .hover_def(def)
                .ok()
                .map(|hover| format!("Inheriance lookup on: \n --- \n {hover}")),
        }
    }

    fn hover_def(&self, def_id: &DefId) -> anyhow::Result<String> {
        let def_body = match def_id {
            DefId::File(_) => Ok("File".into()),
            DefId::Udvt(id) => {
                let data = self.db.udvt_data(*id);
                Ok(code_block(&format!(
                    "type {} is {}",
                    data.name, data.underlying
                )))
            }
            DefId::Contract(id) => self.hover_contract(id),
            DefId::Library(id) => self.hover_library(id),
            DefId::Interface(id) => self.hover_interface(id),
            DefId::Function(id) => self.hover_function(id),
            DefId::Modifier(id) => self.hover_modifier(id),
            DefId::Struct(id) => self.hover_struct(id),
            DefId::Enum(id) => self.hover_enum(id),
            DefId::Event(id) => self.hover_event(id),
            DefId::Error(id) => self.hover_error(id),
            DefId::Var(id) => self.hover_var(id),
            DefId::Import(_) | DefId::Using(_) => {
                Err(anyhow::anyhow!("declaration has no hover signature"))
            }
        };

        Ok(HoverInfo::declaration(
            self.def_title(def_id),
            def_body?,
            self.def_link(def_id),
        ))
    }

    fn hover_var(&self, var_id: &VarId) -> anyhow::Result<String> {
        let var_data = self.db.var_data(*var_id);
        let ty = var_data.syntax_store.types[var_data.ty_ref].to_string(&var_data.syntax_store.types);
        let doc = self.db.docs(DefId::Var(*var_id));
        let mut parts = Vec::new();
        parts.push(ty);
        match var_data.vis {
            v @ (Visibility::Public | Visibility::Private) => parts.push(v.as_str().into()),
            _ => {}
        }
        match var_data.kind {
            k @ (VariableKind::Const | VariableKind::Immutable) => parts.push(k.to_string().to_lowercase()),
            _ => {}
        }
        
        parts.push(var_data.name.as_str().to_string());
        let sig = parts.join(" ");
        let sig = if var_data.kind == VariableKind::Const {
            self.constant_signature(*var_id, &var_data, &sig)
        } else {
            sig
        };
        Ok(Self::format_with_doc(&sig, doc))
    }

    fn constant_signature(
        &self,
        var_id: VarId,
        var_data: &crate::hir::item_data::VarData,
        signature: &str,
    ) -> String {
        let Some(init) = var_data.init else {
            return signature.to_string();
        };
        let Some(range) =
            var_data
                .syntax_store
                .range_to_semantic
                .iter()
                .find_map(|(range, semantic)| {
                    matches!(semantic, SemanticId::Expr(expr) if *expr == init).then_some(*range)
                })
        else {
            return signature.to_string();
        };
        let (file, _) = DefId::Var(var_id).file_and_id();
        let text = self.db.text(file);
        let Some(source) = text.get(range.start as usize..range.end as usize) else {
            return signature.to_string();
        };
        format!("{signature} = {}", Self::strip_comments(source))
    }

    fn hover_function(&self, fn_id: &FunctionId) -> anyhow::Result<String> {
        let fn_data = self.db.function_data(*fn_id);
        let params = fn_data
            .args
            .iter()
            .map(|id| {
                let local = &fn_data.parameters[*id];
                let ty =
                    fn_data.syntax_store.types[*local.ty_ref()].to_string(&fn_data.syntax_store.types);
                format!("{} {}", ty, local.name())
            })
            .collect::<Vec<_>>()
            .join(", ");
        let ret = fn_data
            .rets
            .iter()
            .map(|id| {
                let local = &fn_data.parameters[*id];
                fn_data.syntax_store.types[*local.ty_ref()].to_string(&fn_data.syntax_store.types)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let ret_str = if ret.is_empty() {
            String::new()
        } else {
            format!(" returns ({ret})")
        };
        let doc = self.db.docs(DefId::Function(*fn_id));
        let sig = match fn_data.kind {
            FunctionKind::Regular => format!(
                "function {}({params}) {} {}{ret_str}",
                fn_data.name,
                fn_data.vis.as_str(),
                fn_data.mutability.as_str()
            ),
            FunctionKind::Constructor => format!(
                "constructor({params}) {} {}",
                fn_data.vis.as_str(),
                fn_data.mutability.as_str()
            ),
            FunctionKind::Fallback => {
                let mutability = if fn_data.mutability == Mutability::Payable {
                    "payable "
                } else {
                    Default::default()
                };
                format!("fallback({params}) external {mutability}{ret_str}")
            }
            FunctionKind::Receive => "receive() external payable".to_string(),
        };
        Ok(Self::format_with_doc(&sig, doc))
    }

    fn hover_modifier(&self, mod_id: &ModifierId) -> anyhow::Result<String> {
        let mod_data = self.db.modifier_data(*mod_id);
        let params = mod_data
            .parameters
            .iter()
            .map(|(_, local)| {
                let ty = mod_data.syntax_store.types[*local.ty_ref()]
                    .to_string(&mod_data.syntax_store.types);
                format!("{} {}", ty, local.name())
            })
            .collect::<Vec<_>>()
            .join(", ");
        let doc = self.db.docs(DefId::Modifier(*mod_id));
        let sig = format!("modifier {}({params})", mod_data.name);
        Ok(Self::format_with_doc(&sig, doc))
    }

    pub(crate) fn hover_struct(&self, struct_id: &StructId) -> anyhow::Result<String> {
        let struct_data = self.db.struct_data(*struct_id);
        let fields = struct_data
            .fields
            .iter()
            .map(|(_, field)| {
                let ty = struct_data.syntax_store.types[field.ty_ref]
                    .to_string(&struct_data.syntax_store.types);
                format!("{} {};", ty, field.name)
            })
            .collect::<Vec<_>>();
        let doc = self.db.docs(DefId::Struct(*struct_id));
        let sig = if fields.is_empty() {
            format!("struct {} {{}}", struct_data.name)
        } else {
            format!("struct {} {{\n    {}\n}}", struct_data.name, fields.join("\n    "))
        };
        Ok(Self::format_with_doc(&sig, doc))
    }

    pub(crate) fn hover_enum(&self, enum_id: &EnumId) -> anyhow::Result<String> {
        let enum_data = self.db.enum_data(*enum_id);
        let variants = enum_data
            .variants
            .iter()
            .map(|(_, variant)| variant.name.as_str().to_string())
            .collect::<Vec<_>>();
        let doc = self.db.docs(DefId::Enum(*enum_id));
        let sig = if variants.is_empty() {
            format!("enum {} {{}}", enum_data.name)
        } else {
            format!("enum {} {{\n    {}\n}}", enum_data.name, variants.join(",\n    "))
        };
        Ok(Self::format_with_doc(&sig, doc))
    }

    fn hover_event(&self, event_id: &EventId) -> anyhow::Result<String> {
        let event_data = self.db.event_data(*event_id);
        let params = event_data
            .parameters
            .iter()
            .map(|(_, p)| {
                let ty = event_data.syntax_store.types[*p.ty_ref()]
                    .to_string(&event_data.syntax_store.types);
                format!("{} {}", ty, p.name())
            })
            .collect::<Vec<_>>()
            .join(", ");
        let doc = self.db.docs(DefId::Event(*event_id));
        let sig = format!("event {}({params})", event_data.name);
        Ok(Self::format_with_doc(&sig, doc))
    }

    fn hover_error(&self, error_id: &ErrorId) -> anyhow::Result<String> {
        let error_data = self.db.error_data(*error_id);
        let params = error_data
            .parameters
            .iter()
            .map(|(_, p)| {
                let ty = error_data.syntax_store.types[*p.ty_ref()]
                    .to_string(&error_data.syntax_store.types);
                format!("{} {}", ty, p.name())
            })
            .collect::<Vec<_>>()
            .join(", ");
        let doc = self.db.docs(DefId::Error(*error_id));
        let sig = format!("error {}({params})", error_data.name);
        Ok(Self::format_with_doc(&sig, doc))
    }

    fn format_base(expr_id: ExprId, store: &SyntaxStore) -> Option<String> {
        match &store.exprs[expr_id] {
            Expr::Ident(name) => Some(name.to_string()),
            Expr::Member { obj, prop } => {
                Some(format!("{}.{prop}", Self::format_base(*obj, store)?))
            }
            Expr::Call { callee, .. } | Expr::ModifierInv(callee) => {
                Self::format_base(*callee, store)
            }
            _ => None,
        }
    }

    pub(crate) fn hover_contract(&self, contract_id: &ContractId) -> anyhow::Result<String> {
        let contract_data = self.db.contract_data(*contract_id);
        let doc = self.db.docs(DefId::Contract(*contract_id));
        let bases = contract_data
            .bases
            .iter()
            .filter_map(|&expr_id| Self::format_base(expr_id, &contract_data.syntax_store))
            .collect::<Vec<_>>()
            .join(", ");
        let sig = if bases.is_empty() {
            format!("contract {}", contract_data.name)
        } else {
            format!("contract {} is {}", contract_data.name, bases)
        };
        Ok(Self::format_with_doc(&sig, doc))
    }

    fn hover_interface(&self, interface_id: &InterfaceId) -> anyhow::Result<String> {
        let interface_data = self.db.interface_data(*interface_id);
        let doc = self.db.docs(DefId::Interface(*interface_id));
        let bases = interface_data
            .bases
            .iter()
            .filter_map(|&expr_id| Self::format_base(expr_id, &interface_data.syntax_store))
            .collect::<Vec<_>>()
            .join(", ");
        let sig = if bases.is_empty() {
            format!("interface {}", interface_data.name)
        } else {
            format!("interface {} is {}", interface_data.name, bases)
        };
        Ok(Self::format_with_doc(&sig, doc))
    }

    fn hover_library(&self, library_id: &LibraryId) -> anyhow::Result<String> {
        let library_data = self.db.library_data(*library_id);
        let doc = self.db.docs(DefId::Library(*library_id));
        let sig = format!("library {}", library_data.name);
        Ok(Self::format_with_doc(&sig, doc))
    }

    fn hover_callable(&self, callable: &Callable) -> Option<String> {
        if let Some(def) = callable.def() {
            return self.hover_def(&def).ok();
        }
        let builtin = callable.builtin()?;
        let sig = builtin.call_sig(self.db);
        let mut params = sig
            .params
            .iter()
            .map(|ty| ty.display(self.db))
            .collect::<Vec<_>>();
        if builtin.schema() != ParamSchema::Fixed {
            params.push("…".into());
        }
        let params = params.join(", ");
        let ret = match sig.returns.as_ref() {
            [] => String::new(),
            returns => format!(
                " returns ({})",
                returns
                    .iter()
                    .map(|ty| ty.display(self.db))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        Some(
            HoverInfo {
                title: "Builtin_function".into(),
                signature: Some(format!("function {}({}){}", builtin.name(), params, ret)),
                documentation: Some(builtin.doc(self.db).to_string()),
                definition_link: None,
            }
            .render(),
        )
    }

    pub(crate) fn format_local(&self, local: &Local, store: &SyntaxStore) -> String {
        let ty = store.types[*local.ty_ref()].to_string(&store.types);
        let footer = match local.location(&store.types) {
            Some(Location::Storage(RefKind::Pointer)) => Some("storage pointer"),
            _ if matches!(&store.types[*local.ty_ref()], TyName::Fn(_)) => {
                Some("function pointer")
            }
            _ => None,
        };
        HoverInfo {
            title: format!("{}", local.kind()),
            signature: Some(format!("{ty} {}", local.name())),
            documentation: footer.map(|note| format!("*{note}*")),
            definition_link: None,
        }
        .render()
    }

    fn format_field(&self, field: &Field, store: &SyntaxStore, file: File) -> String {
        let ty = store.types[field.ty_ref].to_string(&store.types);
        let doc = self.db.inline_docs(file, field.range);
        HoverInfo {
            title: "Field".into(),
            signature: Some(format!("{ty} {}", field.name)),
            documentation: doc,
            definition_link: Some(self.range_link(file, field.range)),
        }
        .render()
    }

    fn format_variant(&self, enum_data: &EnumData, variant_id: VariantId, file: File) -> String {
        let variant = &enum_data.variants[variant_id];
        let doc = self.db.inline_docs(file, variant.range);
        HoverInfo {
            title: "Enum_variant".into(),
            signature: Some(format!("enum variant {}.{}", enum_data.name, variant.name)),
            documentation: doc,
            definition_link: Some(self.range_link(file, variant.range)),
        }
        .render()
    }

    fn format_literal(&self, literal: &Literal) -> String {
        let inferred = match literal.ty(self.db).kind(self.db) {
            TyKind::Literal(value) => value
                .inferred_type()
                .map(|ty| ty.display(self.db))
                .unwrap_or_else(|| "literal".to_string()),
            ty => ty.display(self.db),
        };
        HoverInfo {
            title: match literal {
                Literal::Boolean(_) => "Boolean_literal",
                Literal::Number(_) => "Number_literal",
                Literal::String(_) => "String_literal",
                Literal::HexString(_) => "Hex_string_literal",
            }
            .to_string(),
            signature: Some(format!("{} : {}", literal.source_text(), inferred)),
            documentation: None,
            definition_link: None,
        }
        .render()
    }

    fn format_resolved_type(&self, ty: Ty) -> Option<String> {
        let kind = ty.peel_loc(self.db).kind(self.db);
        let inferred = match &kind {
            TyKind::Literal(literal) => literal.inferred_type()?,
            _ => kind,
        };
        Some(
            HoverInfo {
                title: "Type".into(),
                signature: Some(inferred.display(self.db)),
                documentation: None,
                definition_link: None,
            }
            .render(),
        )
    }

    /// A source fragment with `//` and `/* */` comments removed — string
    /// literals pass through verbatim and whitespace collapses to single
    /// spaces, so a multi-line initializer displays on one line.
    fn strip_comments(source: &str) -> String {
        enum Mode {
            Normal,
            Line,
            Block,
        }
        let mut out = String::with_capacity(source.len());
        let mut chars = source.chars().peekable();
        let mut mode = Mode::Normal;
        let mut string: Option<char> = None;
        let mut pending_space = false;
        while let Some(ch) = chars.next() {
            match string {
                Some(quote) => {
                    out.push(ch);
                    if ch == '\\' {
                        if let Some(escaped) = chars.next() {
                            out.push(escaped);
                        }
                    } else if ch == quote {
                        string = None;
                    }
                }
                None => match mode {
                    Mode::Line => {
                        if ch == '\n' {
                            mode = Mode::Normal;
                        }
                    }
                    Mode::Block => {
                        if ch == '*' && chars.peek() == Some(&'/') {
                            chars.next();
                            mode = Mode::Normal;
                        }
                    }
                    Mode::Normal => match ch {
                        '"' | '\'' => {
                            out.push(ch);
                            string = Some(ch);
                        }
                        '/' if chars.peek() == Some(&'/') => {
                            chars.next();
                            mode = Mode::Line;
                        }
                        '/' if chars.peek() == Some(&'*') => {
                            chars.next();
                            mode = Mode::Block;
                        }
                        ch if ch.is_whitespace() => pending_space = true,
                        _ => {
                            if std::mem::take(&mut pending_space) && !out.is_empty() {
                                out.push(' ');
                            }
                            out.push(ch);
                        }
                    },
                },
            }
        }
        out
    }

    fn format_with_doc(sig: &str, doc: Option<String>) -> String {
        match doc {
            Some(d) if !d.is_empty() => format!("{}\n---\n{d}", code_block(sig)),
            _ => code_block(sig),
        }
    }

    fn def_title(&self, def_id: &DefId) -> String {
        match def_id {
            DefId::Function(id) => match self.db.function_data(*id).kind {
                FunctionKind::Regular => "Function",
                FunctionKind::Constructor => "Constructor",
                FunctionKind::Fallback => "Fallback",
                FunctionKind::Receive => "Receive",
            }
            .into(),
            DefId::Modifier(_) => "Modifier".into(),
            DefId::Struct(_) => "Struct".into(),
            DefId::Enum(_) => "Enum".into(),
            DefId::Event(_) => "Event".into(),
            DefId::Error(_) => "Error".into(),
            DefId::Contract(_) => "Contract".into(),
            DefId::Interface(_) => "Interface".into(),
            DefId::Library(_) => "Library".into(),
            DefId::Udvt(_) => "Type".into(),
            DefId::Var(id) => self.db.var_data(*id).kind.to_string(),
            DefId::File(_) => "File".into(),
            DefId::Import(_) => "Import".into(),
            DefId::Using(_) => "Using".into(),
        }
    }

    fn def_link(&self, def_id: &DefId) -> Option<String> {
        let (file, Some(ast_id)) = def_id.file_and_id() else {
            return None;
        };
        let node = self
            .db
            .ast_id_map(file)
            .get_node(&self.db.root(file), ast_id)?;
        Some(self.range_link(file, NodeRange::from(&node.node())))
    }

    fn range_link(&self, file: File, range: NodeRange) -> String {
        let path = file.path(self.db);
        let line = self
            .db
            .root(file)
            .named_child_node(range)
            .map(|node| node.node().start_position().row + 1)
            .unwrap_or(1);
        let mut uri = to_url(path.as_path());
        uri.set_fragment(Some(&format!("L{line}")));
        uri.to_string()
    }
}

    pub fn hover(db: &Db, request: Request) -> anyhow::Result<Response> {
    let params: HoverParams = serde_json::from_value(request.params)?;
    let path = to_utf8path(&params.text_document_position_params.text_document.uri)?;
    log_info(format!("Hover request for {path}"));
    if db.is_ignored_path(&path) {
        return Ok(Response {
            id: request.id,
            result: Some(serde_json::Value::Null),
            error: None,
        });
    }

    let position = params.text_document_position_params.position;
    let (file, offset) = db
        .convert(&path, position)
        .context("document is not loaded")?;
    let node = db
        .named_node_at(file, offset)
        .context("No node at cursor")?;
    let owner = owner_at(db, file, offset).context("no owner at cursor")?;

    let range = NodeRange::from(&node.node());
    let hover = Hover { db };
    let hover_str = hover.hover_target(&owner, range)?;

    let result = lsp_types::Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("\n\n {hover_str} \n\n"),
        }),
        range: None,
    };

    Ok(Response {
        id: request.id,
        result: Some(serde_json::to_value(&result)?),
        error: None,
    })
}
