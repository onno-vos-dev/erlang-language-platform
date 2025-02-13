/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::iter;
use std::sync::Arc;

use either::Either;
use elp_base_db::FileId;
use elp_syntax::ast;
use elp_syntax::ast::ExprMax;
use elp_syntax::ast::MacroCallArgs;
use elp_syntax::ast::MacroDefReplacement;
use elp_syntax::ast::MapOp;
use elp_syntax::unescape;
use elp_syntax::AstPtr;
use fxhash::FxHashMap;

use super::InFileAstPtr;
use crate::db::MinDefDatabase;
use crate::expr::MaybeExpr;
use crate::known;
use crate::macro_exp;
use crate::macro_exp::BuiltInMacro;
use crate::name::AsName;
use crate::Atom;
use crate::AttributeBody;
use crate::BinarySeg;
use crate::Body;
use crate::BodySourceMap;
use crate::CRClause;
use crate::CallTarget;
use crate::CatchClause;
use crate::Clause;
use crate::ComprehensionBuilder;
use crate::ComprehensionExpr;
use crate::DefineBody;
use crate::DefineId;
use crate::Expr;
use crate::ExprId;
use crate::ExprSource;
use crate::FunType;
use crate::FunctionBody;
use crate::IfClause;
use crate::InFile;
use crate::ListType;
use crate::Literal;
use crate::MacroName;
use crate::Name;
use crate::NameArity;
use crate::Pat;
use crate::PatId;
use crate::ReceiveAfter;
use crate::Record;
use crate::RecordBody;
use crate::RecordFieldBody;
use crate::ResolvedMacro;
use crate::SpecBody;
use crate::SpecSig;
use crate::Term;
use crate::TermId;
use crate::TypeBody;
use crate::TypeExpr;
use crate::TypeExprId;
use crate::Var;

struct MacroStackEntry {
    name: MacroName,
    file_id: FileId,
    var_map: FxHashMap<Var, ast::MacroExpr>,
    parent_id: usize,
}

pub struct Ctx<'a> {
    db: &'a dyn MinDefDatabase,
    original_file_id: FileId,
    macro_stack: Vec<MacroStackEntry>,
    macro_stack_id: usize,
    function_info: Option<(Atom, u32)>,
    body: Body,
    source_map: BodySourceMap,
}

#[derive(Debug)]
enum MacroReplacement {
    BuiltIn(BuiltInMacro),
    Ast(ast::MacroDefReplacement),
    BuiltInArgs(BuiltInMacro, MacroCallArgs),
    AstArgs(ast::MacroDefReplacement, MacroCallArgs),
}

impl<'a> Ctx<'a> {
    pub fn new(db: &'a dyn MinDefDatabase, file_id: FileId) -> Self {
        Self {
            db,
            original_file_id: file_id,
            macro_stack: vec![MacroStackEntry {
                name: MacroName::new(Name::MISSING, None),
                file_id,
                var_map: FxHashMap::default(),
                parent_id: 0,
            }],
            macro_stack_id: 0,
            function_info: None,
            body: Body::default(),
            source_map: BodySourceMap::default(),
        }
    }

    pub fn set_function_info(&mut self, info: &NameArity) {
        let name = self.db.atom(info.name().clone());
        let arity = info.arity();
        self.function_info = Some((name, arity));
    }

    fn finish(mut self) -> (Arc<Body>, BodySourceMap) {
        // Verify macro expansion state
        let entry = self.macro_stack.pop().expect("BUG: macro stack empty");
        assert_eq!(entry.file_id, self.original_file_id);
        assert_eq!(entry.parent_id, 0);
        assert!(entry.var_map.is_empty());
        assert!(self.macro_stack.is_empty());

        self.body.shrink_to_fit();
        (Arc::new(self.body), self.source_map)
    }

    pub fn lower_function(mut self, function: &ast::FunDecl) -> (FunctionBody, BodySourceMap) {
        let clauses = function
            .clauses()
            .flat_map(|clause| self.lower_clause_or_macro(clause))
            .collect();
        let (body, source_map) = self.finish();

        (FunctionBody { body, clauses }, source_map)
    }

    pub fn lower_type_alias(self, type_alias: &ast::TypeAlias) -> (TypeBody, BodySourceMap) {
        self.do_lower_type_alias(type_alias.name(), type_alias.ty())
    }

    pub fn lower_opaque_type_alias(self, type_alias: &ast::Opaque) -> (TypeBody, BodySourceMap) {
        self.do_lower_type_alias(type_alias.name(), type_alias.ty())
    }

    fn do_lower_type_alias(
        mut self,
        name: Option<ast::TypeName>,
        ty: Option<ast::Expr>,
    ) -> (TypeBody, BodySourceMap) {
        let vars = name
            .and_then(|name| name.args())
            .iter()
            .flat_map(|args| args.args())
            .map(|var| self.db.var(var.as_name()))
            .collect();
        let ty = self.lower_optional_type_expr(ty);
        let (body, source_map) = self.finish();

        (TypeBody { body, vars, ty }, source_map)
    }

    pub fn lower_record(
        mut self,
        record: &Record,
        ast: &ast::RecordDecl,
    ) -> (RecordBody, BodySourceMap) {
        let fields = record
            .fields
            .clone()
            .zip(ast.fields())
            .map(|(field_id, field)| {
                let expr = field
                    .expr()
                    .and_then(|field| field.expr())
                    .map(|expr| self.lower_expr(&expr));
                let ty = field
                    .ty()
                    .and_then(|field| field.expr())
                    .map(|expr| self.lower_type_expr(&expr));
                RecordFieldBody { field_id, expr, ty }
            })
            .collect();

        let (body, source_map) = self.finish();
        (RecordBody { body, fields }, source_map)
    }

    pub fn lower_spec(mut self, spec: &ast::Spec) -> (SpecBody, BodySourceMap) {
        let sigs = self.lower_sigs(spec.sigs());
        let (body, source_map) = self.finish();
        (SpecBody { body, sigs }, source_map)
    }

    pub fn lower_callback(mut self, callback: &ast::Callback) -> (SpecBody, BodySourceMap) {
        let sigs = self.lower_sigs(callback.sigs());
        let (body, source_map) = self.finish();
        (SpecBody { body, sigs }, source_map)
    }

    fn lower_sigs(&mut self, sigs: impl Iterator<Item = ast::TypeSig>) -> Vec<SpecSig> {
        sigs.map(|sig| {
            let args = sig
                .args()
                .iter()
                .flat_map(|args| args.args())
                .map(|arg| self.lower_type_expr(&arg))
                .collect();
            let result = self.lower_optional_type_expr(sig.ty());
            let guards = sig
                .guard()
                .iter()
                .flat_map(|guards| guards.guards())
                .flat_map(|guard| {
                    let ty = self.lower_optional_type_expr(guard.ty());
                    let var = self.db.var(guard.var()?.var()?.as_name());
                    Some((var, ty))
                })
                .collect();
            SpecSig {
                args,
                result,
                guards,
            }
        })
        .collect()
    }

    pub fn lower_attribute(mut self, attr: &ast::WildAttribute) -> (AttributeBody, BodySourceMap) {
        let value = self.lower_optional_term(attr.value());
        let (body, source_map) = self.finish();
        (AttributeBody { body, value }, source_map)
    }

    pub fn lower_define(mut self, define: &ast::PpDefine) -> Option<(DefineBody, BodySourceMap)> {
        let replacement = define.replacement()?;
        match replacement {
            MacroDefReplacement::Expr(expr) => {
                let expr = self.lower_expr(&expr);
                let (body, source_map) = self.finish();
                Some((DefineBody { body, expr }, source_map))
            }
            _ => None,
        }
    }

    pub fn lower_compile(
        mut self,
        attr: &ast::CompileOptionsAttribute,
    ) -> (AttributeBody, BodySourceMap) {
        let value = self.lower_optional_term(attr.options());
        let (body, source_map) = self.finish();
        (AttributeBody { body, value }, source_map)
    }

    fn lower_clause_or_macro(
        &mut self,
        clause: ast::FunctionOrMacroClause,
    ) -> impl Iterator<Item = Clause> {
        match clause {
            ast::FunctionOrMacroClause::FunctionClause(clause) => {
                Either::Left(self.lower_clause(&clause).into_iter())
            }
            ast::FunctionOrMacroClause::MacroCallExpr(call) => {
                Either::Right(
                    self.resolve_macro(&call, |this, _source, replacement| {
                        match replacement {
                            MacroReplacement::Ast(
                                ast::MacroDefReplacement::ReplacementFunctionClauses(clauses),
                            ) => clauses
                                .clauses()
                                .flat_map(|clause| this.lower_clause_or_macro(clause))
                                .collect(),
                            // no built-in macro makes sense in this place
                            MacroReplacement::Ast(_) | MacroReplacement::BuiltIn(_) => vec![],
                            // args make no sense here
                            MacroReplacement::AstArgs(_, _)
                            | MacroReplacement::BuiltInArgs(_, _) => vec![],
                        }
                    })
                    .into_iter()
                    .flatten(),
                )
            }
        }
    }

    fn lower_clause(&mut self, clause: &ast::FunctionClause) -> Option<Clause> {
        let pats = clause
            .args()
            .iter()
            .flat_map(|args| args.args())
            .map(|pat| self.lower_pat(&pat))
            .collect();
        let guards = self.lower_guards(clause.guard());
        let exprs = self.lower_clause_body(clause.body());

        Some(Clause {
            pats,
            guards,
            exprs,
        })
    }

    fn lower_optional_pat(&mut self, expr: Option<ast::Expr>) -> PatId {
        if let Some(expr) = &expr {
            self.lower_pat(expr)
        } else {
            self.alloc_pat(Pat::Missing, None)
        }
    }

    fn lower_pat(&mut self, expr: &ast::Expr) -> PatId {
        match expr {
            ast::Expr::ExprMax(expr_max) => self.lower_pat_max(expr_max, expr),
            ast::Expr::AnnType(ann) => {
                let _ = self.lower_optional_pat(ann.ty());
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::Expr::BinaryOpExpr(binary_op) => {
                let lhs = self.lower_optional_pat(binary_op.lhs());
                let rhs = self.lower_optional_pat(binary_op.rhs());
                if let Some((op, _)) = binary_op.op() {
                    self.alloc_pat(Pat::BinaryOp { lhs, op, rhs }, Some(expr))
                } else {
                    self.alloc_pat(Pat::Missing, Some(expr))
                }
            }
            ast::Expr::Call(call) => {
                let _ = self.lower_optional_pat(call.expr());
                let _ = call
                    .args()
                    .iter()
                    .flat_map(|args| args.args())
                    .for_each(|expr| {
                        let _ = self.lower_pat(&expr);
                    });
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::Expr::CatchExpr(catch) => {
                let _ = self.lower_optional_pat(catch.expr());
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::Expr::Dotdotdot(_) => self.alloc_pat(Pat::Missing, Some(expr)),
            ast::Expr::MapExpr(map) => {
                let fields = map
                    .fields()
                    .flat_map(|field| {
                        let key = self.lower_optional_expr(field.key());
                        let value = self.lower_optional_pat(field.value());
                        if let Some((ast::MapOp::Exact, _)) = field.op() {
                            Some((key, value))
                        } else {
                            None
                        }
                    })
                    .collect();
                self.alloc_pat(Pat::Map { fields }, Some(expr))
            }
            ast::Expr::MapExprUpdate(update) => {
                let _ = self.lower_optional_pat(update.expr().map(Into::into));
                let _ = update.fields().for_each(|field| {
                    let _ = self.lower_optional_expr(field.key());
                    let _ = self.lower_optional_expr(field.value());
                });
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::Expr::MatchExpr(mat) => {
                let lhs = self.lower_optional_pat(mat.lhs());
                let rhs = self.lower_optional_pat(mat.rhs());
                self.alloc_pat(Pat::Match { lhs, rhs }, Some(expr))
            }
            ast::Expr::Pipe(pipe) => {
                let _ = self.lower_optional_pat(pipe.lhs());
                let _ = self.lower_optional_pat(pipe.rhs());
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::Expr::RangeType(range) => {
                let _ = self.lower_optional_pat(range.lhs());
                let _ = self.lower_optional_pat(range.rhs());
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::Expr::RecordExpr(record) => {
                let name = record.name().and_then(|n| self.resolve_name(n.name()?));
                let fields = record
                    .fields()
                    .flat_map(|field| {
                        let value =
                            self.lower_optional_pat(field.expr().and_then(|expr| expr.expr()));
                        let name = self.resolve_name(field.name()?)?;
                        Some((name, value))
                    })
                    .collect();
                if let Some(name) = name {
                    self.alloc_pat(Pat::Record { name, fields }, Some(expr))
                } else {
                    self.alloc_pat(Pat::Missing, Some(expr))
                }
            }
            ast::Expr::RecordFieldExpr(field) => {
                let _ = self.lower_optional_pat(field.expr().map(Into::into));
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::Expr::RecordIndexExpr(index) => {
                let name = index.name().and_then(|n| self.resolve_name(n.name()?));
                let field = index.field().and_then(|n| self.resolve_name(n.name()?));
                if let (Some(name), Some(field)) = (name, field) {
                    self.alloc_pat(Pat::RecordIndex { name, field }, Some(expr))
                } else {
                    self.alloc_pat(Pat::Missing, Some(expr))
                }
            }
            ast::Expr::RecordUpdateExpr(update) => {
                let _ = self.lower_optional_pat(update.expr().map(Into::into));
                let _ = update
                    .fields()
                    .flat_map(|field| field.expr()?.expr())
                    .for_each(|expr| {
                        let _ = self.lower_pat(&expr);
                    });
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::Expr::Remote(remote) => {
                let _ = self.lower_optional_pat(
                    remote
                        .module()
                        .and_then(|module| module.module())
                        .map(Into::into),
                );
                let _ = self.lower_optional_pat(remote.fun().map(Into::into));
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::Expr::UnaryOpExpr(unary_op) => {
                let operand = self.lower_optional_pat(unary_op.operand());
                if let Some((op, _)) = unary_op.op() {
                    self.alloc_pat(Pat::UnaryOp { pat: operand, op }, Some(expr))
                } else {
                    self.alloc_pat(Pat::Missing, Some(expr))
                }
            }
            ast::Expr::CondMatchExpr(cond) => {
                self.lower_optional_pat(cond.lhs());
                self.lower_optional_pat(cond.rhs());
                self.alloc_pat(Pat::Missing, Some(expr))
            }
        }
    }

    fn lower_pat_max(&mut self, expr_max: &ast::ExprMax, expr: &ast::Expr) -> PatId {
        match expr_max {
            ast::ExprMax::AnonymousFun(fun) => {
                let _ = fun.clauses().for_each(|clause| {
                    let _ = clause
                        .args()
                        .iter()
                        .flat_map(|args| args.args())
                        .for_each(|pat| {
                            let _ = self.lower_pat(&pat);
                        });
                    let _ = self.lower_guards(clause.guard());
                    let _ = self.lower_clause_body(clause.body());
                });
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::ExprMax::Atom(atom) => {
                let atom = self.db.atom(atom.as_name());
                self.alloc_pat(Pat::Literal(Literal::Atom(atom)), Some(expr))
            }
            ast::ExprMax::Binary(bin) => {
                let segs = bin
                    .elements()
                    .flat_map(|element| self.lower_bin_element(&element, Self::lower_optional_pat))
                    .collect();
                self.alloc_pat(Pat::Binary { segs }, Some(expr))
            }
            ast::ExprMax::BinaryComprehension(bc) => {
                let _ = self.lower_optional_pat(bc.expr().map(Into::into));
                let _ = self.lower_lc_exprs(bc.lc_exprs());
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::ExprMax::BlockExpr(block) => {
                let _ = block.exprs().for_each(|expr| {
                    self.lower_expr(&expr);
                });
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::ExprMax::CaseExpr(case) => {
                let _ = self.lower_optional_pat(case.expr());
                let _ = case
                    .clauses()
                    .flat_map(|clause| self.lower_cr_clause(clause))
                    .last();
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::ExprMax::Char(char) => {
                let value = lower_char(char).map_or(Pat::Missing, Pat::Literal);
                self.alloc_pat(value, Some(expr))
            }
            ast::ExprMax::Concatables(concat) => {
                let value = lower_concat(concat).map_or(Pat::Missing, Pat::Literal);
                self.alloc_pat(value, Some(expr))
            }
            ast::ExprMax::ExternalFun(fun) => {
                let _ = self.lower_optional_pat(
                    fun.module()
                        .and_then(|module| module.name())
                        .map(Into::into),
                );
                let _ = self.lower_optional_pat(fun.fun().map(Into::into));
                let _ = self.lower_optional_pat(
                    fun.arity().and_then(|arity| arity.value()).map(Into::into),
                );
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::ExprMax::Float(float) => {
                let value = lower_float(float).map_or(Pat::Missing, Pat::Literal);
                self.alloc_pat(value, Some(expr))
            }
            ast::ExprMax::FunType(fun) => {
                if let Some(sig) = fun.sig() {
                    let _ = self.lower_optional_pat(sig.ty());
                    let _ = sig
                        .args()
                        .iter()
                        .flat_map(|args| args.args())
                        .for_each(|pat| {
                            let _ = self.lower_pat(&pat);
                        });
                }
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::ExprMax::IfExpr(if_expr) => {
                let _ = if_expr.clauses().for_each(|clause| {
                    let _ = self.lower_guards(clause.guard());
                    let _ = self.lower_clause_body(clause.body());
                });
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::ExprMax::Integer(int) => {
                let value = lower_int(int).map_or(Pat::Missing, Pat::Literal);
                self.alloc_pat(value, Some(expr))
            }
            ast::ExprMax::InternalFun(fun) => {
                let _ = self.lower_optional_pat(fun.fun().map(Into::into));
                let _ = self.lower_optional_pat(
                    fun.arity().and_then(|arity| arity.value()).map(Into::into),
                );
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::ExprMax::List(list) => {
                let (pats, tail) = self.lower_list(
                    list,
                    |this| this.alloc_pat(Pat::Missing, None),
                    |this, expr| this.lower_pat(expr),
                );
                self.alloc_pat(Pat::List { pats, tail }, Some(expr))
            }
            ast::ExprMax::ListComprehension(lc) => {
                let _ = self.lower_optional_pat(lc.expr());
                let _ = self.lower_lc_exprs(lc.lc_exprs());
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::ExprMax::MacroCallExpr(call) => self
                .resolve_macro(call, |this, source, replacement| match replacement {
                    MacroReplacement::BuiltIn(built_in) => this
                        .lower_built_in_macro(built_in)
                        .map(|literal| {
                            let pat_id = this.alloc_pat(Pat::Literal(literal), Some(expr));
                            this.record_pat_source(pat_id, source);
                            pat_id
                        }),
                    MacroReplacement::Ast(ast::MacroDefReplacement::Expr(macro_expr)) => {
                        let pat_id = this.lower_pat(&macro_expr);
                        this.record_pat_source(pat_id, source);
                        Some(pat_id)
                    }
                    MacroReplacement::Ast(_)
                    // calls are not allowed in patterns
                    | MacroReplacement::BuiltInArgs(_, _)
                    | MacroReplacement::AstArgs(_, _) => None,
                })
                .flatten()
                .map(|expansion| {
                    let args = call
                        .args()
                        .iter()
                        .flat_map(|args| args.args())
                        .map(|expr| self.lower_optional_expr(expr.expr()))
                        .collect();
                    let expr_id = self.alloc_pat(Pat::MacroCall { expansion, args }, Some(expr));
                    expr_id
                })
                .unwrap_or_else(|| {
                    let _ = call
                        .args()
                        .iter()
                        .flat_map(|args| args.args())
                        .for_each(|expr| {
                            let _ = self.lower_optional_pat(expr.expr());
                            let _ = self.lower_optional_pat(expr.guard());
                        });
                    self.alloc_pat(Pat::Missing, Some(expr))
                }),
            ast::ExprMax::MacroString(_) => self.alloc_pat(Pat::Missing, Some(expr)),
            ExprMax::MapComprehension(map_comp) => {
                self.lower_optional_pat(map_comp.expr().and_then(|mf| mf.key()));
                self.lower_optional_pat(map_comp.expr().and_then(|mf| mf.value()));
                let _ = self.lower_lc_exprs(map_comp.lc_exprs());
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::ExprMax::MaybeExpr(maybe) => {
                let _ = maybe.exprs().for_each(|expr| {
                    self.lower_expr(&expr);
                });
                let _ = maybe
                    .clauses()
                    .flat_map(|clause| self.lower_cr_clause(clause))
                    .last();
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::ExprMax::ParenExpr(paren_expr) => {
                if let Some(expr) = paren_expr.expr() {
                    let pat_id = self.lower_pat(&expr);
                    let ptr = AstPtr::new(&expr);
                    let source = InFileAstPtr::new(self.curr_file_id(), ptr);
                    self.record_pat_source(pat_id, source);
                    pat_id
                } else {
                    self.alloc_pat(Pat::Missing, Some(expr))
                }
            }
            ast::ExprMax::ReceiveExpr(receive) => {
                let _ = receive
                    .clauses()
                    .flat_map(|clause| self.lower_cr_clause(clause))
                    .last();
                let _ = receive.after().map(|after| {
                    let _ = self.lower_optional_expr(after.expr());
                    let _ = self.lower_clause_body(after.body());
                });
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::ExprMax::String(str) => {
                let value = lower_str(str).map_or(Pat::Missing, Pat::Literal);
                self.alloc_pat(value, Some(expr))
            }
            ast::ExprMax::TryExpr(try_expr) => {
                let _ = try_expr.exprs().for_each(|expr| {
                    self.lower_pat(&expr);
                });
                let _ = try_expr
                    .clauses()
                    .flat_map(|clause| self.lower_cr_clause(clause))
                    .last();
                let _ = try_expr.catch().for_each(|clause| {
                    let _ = clause
                        .class()
                        .and_then(|class| class.class())
                        .map(|class| self.lower_pat(&class.into()));
                    let _ = self.lower_optional_pat(clause.pat().map(Into::into));
                    let _ = clause
                        .stack()
                        .and_then(|stack| stack.class())
                        .map(|var| self.lower_pat(&ast::Expr::ExprMax(ast::ExprMax::Var(var))));
                    let _ = self.lower_guards(clause.guard());
                    let _ = self.lower_clause_body(clause.body());
                });
                let _ = try_expr
                    .after()
                    .iter()
                    .flat_map(|after| after.exprs())
                    .for_each(|expr| {
                        self.lower_pat(&expr);
                    });
                self.alloc_pat(Pat::Missing, Some(expr))
            }
            ast::ExprMax::Tuple(tup) => {
                let pats = tup.expr().map(|expr| self.lower_pat(&expr)).collect();
                self.alloc_pat(Pat::Tuple { pats }, Some(expr))
            }
            ast::ExprMax::Var(var) => self
                .resolve_var(var, |this, expr| this.lower_optional_pat(expr.expr()))
                .unwrap_or_else(|var| self.alloc_pat(Pat::Var(var), Some(expr))),
        }
    }

    fn lower_optional_expr(&mut self, expr: Option<ast::Expr>) -> ExprId {
        if let Some(expr) = &expr {
            self.lower_expr(expr)
        } else {
            self.alloc_expr(Expr::Missing, None)
        }
    }

    fn lower_expr(&mut self, expr: &ast::Expr) -> ExprId {
        match expr {
            ast::Expr::ExprMax(expr_max) => self.lower_expr_max(expr_max, expr),
            ast::Expr::AnnType(ann) => {
                let _ = self.lower_optional_expr(ann.ty());
                self.alloc_expr(Expr::Missing, Some(expr))
            }
            ast::Expr::BinaryOpExpr(binary_op) => {
                let lhs = self.lower_optional_expr(binary_op.lhs());
                let rhs = self.lower_optional_expr(binary_op.rhs());
                if let Some((op, _)) = binary_op.op() {
                    self.alloc_expr(Expr::BinaryOp { lhs, op, rhs }, Some(expr))
                } else {
                    self.alloc_expr(Expr::Missing, Some(expr))
                }
            }
            ast::Expr::Call(call) => {
                let target = self.lower_call_target(call.expr());
                let args = call
                    .args()
                    .iter()
                    .flat_map(|args| args.args())
                    .map(|expr| self.lower_expr(&expr))
                    .collect();
                self.alloc_expr(Expr::Call { target, args }, Some(expr))
            }
            ast::Expr::CatchExpr(catch) => {
                let value = self.lower_optional_expr(catch.expr());
                self.alloc_expr(Expr::Catch { expr: value }, Some(expr))
            }
            ast::Expr::Dotdotdot(_) => self.alloc_expr(Expr::Missing, Some(expr)),
            ast::Expr::MapExpr(map) => {
                let fields = map
                    .fields()
                    .flat_map(|field| {
                        let key = self.lower_optional_expr(field.key());
                        let value = self.lower_optional_expr(field.value());
                        if let Some((ast::MapOp::Assoc, _)) = field.op() {
                            Some((key, value))
                        } else {
                            None
                        }
                    })
                    .collect();
                self.alloc_expr(Expr::Map { fields }, Some(expr))
            }
            ast::Expr::MapExprUpdate(update) => {
                let base = self.lower_optional_expr(update.expr().map(Into::into));
                let fields = update
                    .fields()
                    .flat_map(|field| {
                        let key = self.lower_optional_expr(field.key());
                        let value = self.lower_optional_expr(field.value());
                        Some((key, field.op()?.0, value))
                    })
                    .collect();
                self.alloc_expr(Expr::MapUpdate { expr: base, fields }, Some(expr))
            }
            ast::Expr::MatchExpr(mat) => {
                let lhs = self.lower_optional_pat(mat.lhs());
                let rhs = self.lower_optional_expr(mat.rhs());
                self.alloc_expr(Expr::Match { lhs, rhs }, Some(expr))
            }
            ast::Expr::Pipe(pipe) => {
                let _ = self.lower_optional_expr(pipe.lhs());
                let _ = self.lower_optional_expr(pipe.rhs());
                self.alloc_expr(Expr::Missing, Some(expr))
            }
            ast::Expr::RangeType(range) => {
                let _ = self.lower_optional_expr(range.lhs());
                let _ = self.lower_optional_expr(range.rhs());
                self.alloc_expr(Expr::Missing, Some(expr))
            }
            ast::Expr::RecordExpr(record) => {
                let name = record.name().and_then(|n| self.resolve_name(n.name()?));
                let fields = record
                    .fields()
                    .flat_map(|field| {
                        let value =
                            self.lower_optional_expr(field.expr().and_then(|expr| expr.expr()));
                        let name = self.resolve_name(field.name()?)?;
                        Some((name, value))
                    })
                    .collect();
                if let Some(name) = name {
                    self.alloc_expr(Expr::Record { name, fields }, Some(expr))
                } else {
                    self.alloc_expr(Expr::Missing, Some(expr))
                }
            }
            ast::Expr::RecordFieldExpr(field) => {
                let base = self.lower_optional_expr(field.expr().map(Into::into));
                let name = field.name().and_then(|n| self.resolve_name(n.name()?));
                let field = field.field().and_then(|n| self.resolve_name(n.name()?));
                if let (Some(name), Some(field)) = (name, field) {
                    self.alloc_expr(
                        Expr::RecordField {
                            expr: base,
                            name,
                            field,
                        },
                        Some(expr),
                    )
                } else {
                    self.alloc_expr(Expr::Missing, Some(expr))
                }
            }
            ast::Expr::RecordIndexExpr(index) => {
                let name = index.name().and_then(|n| self.resolve_name(n.name()?));
                let field = index.field().and_then(|n| self.resolve_name(n.name()?));
                if let (Some(name), Some(field)) = (name, field) {
                    self.alloc_expr(Expr::RecordIndex { name, field }, Some(expr))
                } else {
                    self.alloc_expr(Expr::Missing, Some(expr))
                }
            }
            ast::Expr::RecordUpdateExpr(update) => {
                let base = self.lower_optional_expr(update.expr().map(Into::into));
                let name = update.name().and_then(|n| self.resolve_name(n.name()?));
                let fields = update
                    .fields()
                    .flat_map(|field| {
                        let value =
                            self.lower_optional_expr(field.expr().and_then(|expr| expr.expr()));
                        let name = self.resolve_name(field.name()?)?;
                        Some((name, value))
                    })
                    .collect();
                if let Some(name) = name {
                    self.alloc_expr(
                        Expr::RecordUpdate {
                            expr: base,
                            name,
                            fields,
                        },
                        Some(expr),
                    )
                } else {
                    self.alloc_expr(Expr::Missing, Some(expr))
                }
            }
            ast::Expr::Remote(remote) => {
                let _ = self.lower_optional_expr(
                    remote
                        .module()
                        .and_then(|module| module.module())
                        .map(Into::into),
                );
                let _ = self.lower_optional_expr(remote.fun().map(Into::into));
                self.alloc_expr(Expr::Missing, Some(expr))
            }
            ast::Expr::UnaryOpExpr(unary_op) => {
                let operand = self.lower_optional_expr(unary_op.operand());
                if let Some((op, _)) = unary_op.op() {
                    self.alloc_expr(Expr::UnaryOp { expr: operand, op }, Some(expr))
                } else {
                    self.alloc_expr(Expr::Missing, Some(expr))
                }
            }
            ast::Expr::CondMatchExpr(cond) => {
                self.lower_optional_pat(cond.lhs());
                self.lower_optional_expr(cond.rhs());
                self.alloc_expr(Expr::Missing, Some(expr))
            }
        }
    }

    fn lower_call_target(&mut self, expr: Option<ast::Expr>) -> CallTarget<ExprId> {
        match expr.as_ref() {
            Some(ast::Expr::ExprMax(ast::ExprMax::ParenExpr(paren))) => {
                self.lower_call_target(paren.expr())
            }
            Some(ast::Expr::Remote(remote)) => CallTarget::Remote {
                module: self.lower_optional_expr(
                    remote
                        .module()
                        .and_then(|module| module.module())
                        .map(ast::Expr::ExprMax),
                ),
                name: self.lower_optional_expr(remote.fun().map(ast::Expr::ExprMax)),
            },
            Some(ast::Expr::ExprMax(ast::ExprMax::MacroCallExpr(call))) => self
                .resolve_macro(call, |this, source, replacement| match replacement {
                    MacroReplacement::BuiltIn(built_in) => {
                        this.lower_built_in_macro(built_in).map(|literal| {
                            let name = this.alloc_expr(Expr::Literal(literal), None);
                            this.record_expr_source(name, source);
                            CallTarget::Local { name }
                        })
                    }
                    MacroReplacement::Ast(ast::MacroDefReplacement::Expr(expr)) => {
                        Some(this.lower_call_target(Some(expr)))
                    }
                    MacroReplacement::Ast(_) => None,
                    // This would mean double parens in the call - invalid
                    MacroReplacement::BuiltInArgs(_, _) | MacroReplacement::AstArgs(_, _) => None,
                })
                .flatten()
                .unwrap_or_else(|| {
                    let _ = call
                        .args()
                        .iter()
                        .flat_map(|args| args.args())
                        .for_each(|expr| {
                            let _ = self.lower_optional_expr(expr.expr());
                            let _ = self.lower_optional_expr(expr.guard());
                        });
                    CallTarget::Local {
                        name: self.alloc_expr(Expr::Missing, expr.as_ref()),
                    }
                }),
            Some(expr) => CallTarget::Local {
                name: self.lower_expr(expr),
            },
            None => CallTarget::Local {
                name: self.alloc_expr(Expr::Missing, None),
            },
        }
    }

    fn lower_expr_max(&mut self, expr_max: &ast::ExprMax, expr: &ast::Expr) -> ExprId {
        match expr_max {
            ast::ExprMax::AnonymousFun(fun) => {
                let mut name = None;
                let clauses = fun
                    .clauses()
                    .map(|clause| {
                        if let Some(found_name) = clause.name() {
                            name = Some(self.lower_pat(&found_name.into()));
                        }
                        let pats = clause
                            .args()
                            .iter()
                            .flat_map(|args| args.args())
                            .map(|pat| self.lower_pat(&pat))
                            .collect();
                        let guards = self.lower_guards(clause.guard());
                        let exprs = self.lower_clause_body(clause.body());
                        Clause {
                            pats,
                            guards,
                            exprs,
                        }
                    })
                    .collect();
                self.alloc_expr(Expr::Closure { clauses, name }, Some(expr))
            }
            ast::ExprMax::Atom(atom) => {
                let atom = self.db.atom(atom.as_name());
                self.alloc_expr(Expr::Literal(Literal::Atom(atom)), Some(expr))
            }
            ast::ExprMax::Binary(bin) => {
                let segs = bin
                    .elements()
                    .flat_map(|element| self.lower_bin_element(&element, Self::lower_optional_expr))
                    .collect();
                self.alloc_expr(Expr::Binary { segs }, Some(expr))
            }
            ast::ExprMax::BinaryComprehension(bc) => {
                let value = self.lower_optional_expr(bc.expr().map(Into::into));
                let builder = ComprehensionBuilder::Binary(value);
                let exprs = self.lower_lc_exprs(bc.lc_exprs());
                self.alloc_expr(Expr::Comprehension { builder, exprs }, Some(expr))
            }
            ast::ExprMax::BlockExpr(block) => {
                let exprs = block.exprs().map(|expr| self.lower_expr(&expr)).collect();
                self.alloc_expr(Expr::Block { exprs }, Some(expr))
            }
            ast::ExprMax::CaseExpr(case) => {
                let value = self.lower_optional_expr(case.expr());
                let clauses = case
                    .clauses()
                    .flat_map(|clause| self.lower_cr_clause(clause))
                    .collect();
                self.alloc_expr(
                    Expr::Case {
                        expr: value,
                        clauses,
                    },
                    Some(expr),
                )
            }
            ast::ExprMax::Char(char) => {
                let value = lower_char(char).map_or(Expr::Missing, Expr::Literal);
                self.alloc_expr(value, Some(expr))
            }
            ast::ExprMax::Concatables(concat) => {
                let value = lower_concat(concat).map_or(Expr::Missing, Expr::Literal);
                self.alloc_expr(value, Some(expr))
            }
            ast::ExprMax::ExternalFun(fun) => {
                let target = CallTarget::Remote {
                    module: self.lower_optional_expr(
                        fun.module()
                            .and_then(|module| module.name())
                            .map(Into::into),
                    ),
                    name: self.lower_optional_expr(fun.fun().map(Into::into)),
                };
                let arity = self.lower_optional_expr(
                    fun.arity().and_then(|arity| arity.value()).map(Into::into),
                );
                self.alloc_expr(Expr::CaptureFun { target, arity }, Some(expr))
            }
            ast::ExprMax::Float(float) => {
                let value = lower_float(float).map_or(Expr::Missing, Expr::Literal);
                self.alloc_expr(value, Some(expr))
            }
            ast::ExprMax::FunType(fun) => {
                if let Some(sig) = fun.sig() {
                    let _ = self.lower_optional_expr(sig.ty());
                    let _ = sig
                        .args()
                        .iter()
                        .flat_map(|args| args.args())
                        .for_each(|pat| {
                            let _ = self.lower_expr(&pat);
                        });
                }
                self.alloc_expr(Expr::Missing, Some(expr))
            }
            ast::ExprMax::IfExpr(if_expr) => {
                let clauses = if_expr
                    .clauses()
                    .map(|clause| {
                        let guards = self.lower_guards(clause.guard());
                        let exprs = self.lower_clause_body(clause.body());
                        IfClause { guards, exprs }
                    })
                    .collect();
                self.alloc_expr(Expr::If { clauses }, Some(expr))
            }
            ast::ExprMax::Integer(int) => {
                let value = lower_int(int).map_or(Expr::Missing, Expr::Literal);
                self.alloc_expr(value, Some(expr))
            }
            ast::ExprMax::InternalFun(fun) => {
                let target = CallTarget::Local {
                    name: self.lower_optional_expr(fun.fun().map(Into::into)),
                };
                let arity = self.lower_optional_expr(
                    fun.arity().and_then(|arity| arity.value()).map(Into::into),
                );
                self.alloc_expr(Expr::CaptureFun { target, arity }, Some(expr))
            }
            ast::ExprMax::List(list) => {
                let (exprs, tail) = self.lower_list(
                    list,
                    |this| this.alloc_expr(Expr::Missing, None),
                    |this, expr| this.lower_expr(expr),
                );
                self.alloc_expr(Expr::List { exprs, tail }, Some(expr))
            }
            ast::ExprMax::ListComprehension(lc) => {
                let value = self.lower_optional_expr(lc.expr());
                let builder = ComprehensionBuilder::List(value);
                let exprs = self.lower_lc_exprs(lc.lc_exprs());
                self.alloc_expr(Expr::Comprehension { builder, exprs }, Some(expr))
            }
            ast::ExprMax::MacroCallExpr(call) => self
                .resolve_macro(call, |this, source, replacement| match replacement {
                    MacroReplacement::BuiltIn(built_in) => {
                        this.lower_built_in_macro(built_in).map(|literal| {
                            let expr_id = this.alloc_expr(Expr::Literal(literal), None);
                            this.record_expr_source(expr_id, source);
                            expr_id
                        })
                    }
                    MacroReplacement::Ast(ast::MacroDefReplacement::Expr(macro_expr)) => {
                        let expr_id = this.lower_expr(&macro_expr);
                        this.record_expr_source(expr_id, source);
                        Some(expr_id)
                    }
                    MacroReplacement::Ast(_) => None,
                    MacroReplacement::BuiltInArgs(built_in, args) => {
                        let name = this
                            .lower_built_in_macro(built_in)
                            .map(|literal| this.alloc_expr(Expr::Literal(literal), None))
                            .unwrap_or_else(|| this.alloc_expr(Expr::Missing, None));
                        let target = CallTarget::Local { name };
                        let args = args
                            .args()
                            .map(|expr| this.lower_optional_expr(expr.expr()))
                            .collect();
                        let expr_id = this.alloc_expr(Expr::Call { target, args }, None);
                        this.record_expr_source(expr_id, source);
                        Some(expr_id)
                    }
                    MacroReplacement::AstArgs(
                        ast::MacroDefReplacement::Expr(replacement),
                        args,
                    ) => {
                        let target = this.lower_call_target(Some(replacement));
                        let args = args
                            .args()
                            .map(|expr| this.lower_optional_expr(expr.expr()))
                            .collect();
                        let expr_id = this.alloc_expr(Expr::Call { target, args }, None);
                        this.record_expr_source(expr_id, source);
                        Some(expr_id)
                    }
                    MacroReplacement::AstArgs(_, _) => None,
                })
                .flatten()
                .map(|expansion| {
                    let args = call
                        .args()
                        .iter()
                        .flat_map(|args| args.args())
                        .map(|expr| self.lower_optional_expr(expr.expr()))
                        .collect();
                    let expr_id = self.alloc_expr(Expr::MacroCall { expansion, args }, Some(expr));
                    expr_id
                })
                .unwrap_or_else(|| {
                    let _ = call
                        .args()
                        .iter()
                        .flat_map(|args| args.args())
                        .for_each(|expr| {
                            let _ = self.lower_optional_expr(expr.expr());
                            let _ = self.lower_optional_expr(expr.guard());
                        });
                    self.alloc_expr(Expr::Missing, Some(expr))
                }),
            ast::ExprMax::MacroString(_) => self.alloc_expr(Expr::Missing, Some(expr)),
            ast::ExprMax::ParenExpr(paren_expr) => {
                if let Some(paren_expr) = paren_expr.expr() {
                    let expr_id = self.lower_expr(&paren_expr);
                    let ptr = AstPtr::new(expr);
                    let source = InFileAstPtr::new(self.curr_file_id(), ptr);
                    self.record_expr_source(expr_id, source);
                    expr_id
                } else {
                    self.alloc_expr(Expr::Missing, Some(expr))
                }
            }
            ast::ExprMax::ReceiveExpr(receive) => {
                let clauses = receive
                    .clauses()
                    .flat_map(|clause| self.lower_cr_clause(clause))
                    .collect();
                let after = receive.after().map(|after| {
                    let timeout = self.lower_optional_expr(after.expr());
                    let exprs = self.lower_clause_body(after.body());
                    ReceiveAfter { timeout, exprs }
                });
                self.alloc_expr(Expr::Receive { clauses, after }, Some(expr))
            }
            ast::ExprMax::String(str) => {
                let value = lower_str(str).map_or(Expr::Missing, Expr::Literal);
                self.alloc_expr(value, Some(expr))
            }
            ast::ExprMax::TryExpr(try_expr) => {
                let exprs = try_expr
                    .exprs()
                    .map(|expr| self.lower_expr(&expr))
                    .collect();
                let of_clauses = try_expr
                    .clauses()
                    .flat_map(|clause| self.lower_cr_clause(clause))
                    .collect();
                let catch_clauses = try_expr
                    .catch()
                    .map(|clause| {
                        let class = clause
                            .class()
                            .and_then(|class| class.class())
                            .map(|class| self.lower_pat(&class.into()));
                        let reason = self.lower_optional_pat(clause.pat().map(Into::into));
                        let stack = clause
                            .stack()
                            .and_then(|stack| stack.class())
                            .map(|var| self.lower_pat(&ast::Expr::ExprMax(ast::ExprMax::Var(var))));
                        let guards = self.lower_guards(clause.guard());
                        let exprs = self.lower_clause_body(clause.body());
                        CatchClause {
                            class,
                            reason,
                            stack,
                            guards,
                            exprs,
                        }
                    })
                    .collect();
                let after = try_expr
                    .after()
                    .iter()
                    .flat_map(|after| after.exprs())
                    .map(|expr| self.lower_expr(&expr))
                    .collect();
                self.alloc_expr(
                    Expr::Try {
                        exprs,
                        of_clauses,
                        catch_clauses,
                        after,
                    },
                    Some(expr),
                )
            }
            ast::ExprMax::Tuple(tup) => {
                let exprs = tup.expr().map(|expr| self.lower_expr(&expr)).collect();
                self.alloc_expr(Expr::Tuple { exprs }, Some(expr))
            }
            ast::ExprMax::Var(var) => self
                .resolve_var(var, |this, expr| this.lower_optional_expr(expr.expr()))
                .unwrap_or_else(|var| self.alloc_expr(Expr::Var(var), Some(expr))),
            ast::ExprMax::MaybeExpr(maybe) => {
                let exprs = maybe
                    .exprs()
                    .map(|expr| self.lower_maybe_expr(&expr))
                    .collect();

                let else_clauses = maybe
                    .clauses()
                    .flat_map(|clause| self.lower_cr_clause(clause))
                    .collect();
                self.alloc_expr(
                    Expr::Maybe {
                        exprs,
                        else_clauses,
                    },
                    Some(expr),
                )
            }
            ast::ExprMax::MapComprehension(map_comp) => {
                let key = self.lower_optional_expr(map_comp.expr().and_then(|mf| mf.key()));
                let value = self.lower_optional_expr(map_comp.expr().and_then(|mf| mf.value()));
                let exprs = self.lower_lc_exprs(map_comp.lc_exprs());
                let comp_expr = match map_comp.expr().and_then(|mf| mf.op()) {
                    Some((MapOp::Assoc, _)) => Expr::Comprehension {
                        builder: ComprehensionBuilder::Map(key, value),
                        exprs,
                    },
                    _ => Expr::Missing,
                };

                self.alloc_expr(comp_expr, Some(expr))
            }
        }
    }

    fn lower_maybe_expr(&mut self, expr: &ast::Expr) -> MaybeExpr {
        match expr {
            ast::Expr::CondMatchExpr(cond) => {
                let pat_id = self.lower_optional_pat(cond.lhs());
                let expr_id = self.lower_optional_expr(cond.rhs());
                self.alloc_expr(Expr::Missing, Some(expr));
                MaybeExpr::Cond {
                    lhs: pat_id,
                    rhs: expr_id,
                }
            }
            ast::Expr::ExprMax(ast::ExprMax::ParenExpr(paren)) => match paren.expr() {
                Some(paren_expr) => self.lower_maybe_expr(&paren_expr),
                None => MaybeExpr::Expr(self.alloc_expr(Expr::Missing, None)),
            },
            e => MaybeExpr::Expr(self.lower_expr(e)),
        }
    }

    fn lower_list<Id>(
        &mut self,
        list: &ast::List,
        make_missing: impl Fn(&mut Self) -> Id,
        lower: impl Fn(&mut Self, &ast::Expr) -> Id,
    ) -> (Vec<Id>, Option<Id>) {
        let mut tail = None;
        let mut ids = vec![];

        for expr in list.exprs() {
            if let ast::Expr::Pipe(pipe) = &expr {
                let id = pipe
                    .lhs()
                    .map(|expr| lower(self, &expr))
                    .unwrap_or_else(|| make_missing(self));
                ids.push(id);

                if let Some(tail) = tail {
                    // TODO: add error
                    ids.push(tail)
                }
                tail = pipe.rhs().map(|expr| lower(self, &expr));
            } else {
                ids.push(lower(self, &expr));
            }
        }

        (ids, tail)
    }

    fn lower_bin_element<Id>(
        &mut self,
        element: &ast::BinElement,
        lower: fn(&mut Self, Option<ast::Expr>) -> Id,
    ) -> Option<BinarySeg<Id>> {
        let elem = lower(self, element.element().map(Into::into));
        let size = element
            .size()
            .and_then(|size| size.size())
            .map(|expr| self.lower_expr(&expr.into()));

        let mut unit = None;
        let tys = element
            .types()
            .iter()
            .flat_map(|types| types.types())
            .flat_map(|ty| match ty {
                ast::BitType::Name(name) => self.resolve_name(name),
                ast::BitType::BitTypeUnit(ty_unit) => {
                    unit = ty_unit.size().and_then(|unit| self.resolve_arity(unit));
                    None
                }
            })
            .collect();

        Some(BinarySeg {
            elem,
            size,
            unit,
            tys,
        })
    }

    fn lower_cr_clause(&mut self, clause: ast::CrClauseOrMacro) -> impl Iterator<Item = CRClause> {
        match clause {
            ast::CrClauseOrMacro::CrClause(clause) => {
                let pat = self.lower_optional_pat(clause.pat());
                let guards = self.lower_guards(clause.guard());
                let exprs = self.lower_clause_body(clause.body());
                Either::Left(Some(CRClause { pat, guards, exprs }).into_iter())
            }
            ast::CrClauseOrMacro::MacroCallExpr(call) => {
                Either::Right(
                    self.resolve_macro(&call, |this, _source, replacement| {
                        match replacement {
                            MacroReplacement::Ast(
                                ast::MacroDefReplacement::ReplacementCrClauses(clauses),
                            ) => clauses
                                .clauses()
                                .flat_map(|clause| this.lower_cr_clause(clause))
                                .collect(),
                            // no built-in macro makes sense in this place
                            MacroReplacement::Ast(_) | MacroReplacement::BuiltIn(_) => vec![],
                            // args make no sense here
                            MacroReplacement::AstArgs(_, _)
                            | MacroReplacement::BuiltInArgs(_, _) => vec![],
                        }
                    })
                    .into_iter()
                    .flatten(),
                )
            }
        }
    }

    fn lower_guards(&mut self, guards: Option<ast::Guard>) -> Vec<Vec<ExprId>> {
        guards
            .iter()
            .flat_map(|guard| guard.clauses())
            .map(|clause| clause.exprs().map(|expr| self.lower_expr(&expr)).collect())
            .collect()
    }

    fn lower_clause_body(&mut self, body: Option<ast::ClauseBody>) -> Vec<ExprId> {
        body.iter()
            .flat_map(|body| body.exprs())
            .map(|expr| self.lower_expr(&expr))
            .collect()
    }

    fn lower_lc_exprs(&mut self, exprs: Option<ast::LcExprs>) -> Vec<ComprehensionExpr> {
        exprs
            .iter()
            .flat_map(|exprs| exprs.exprs())
            .map(|expr| match expr {
                ast::LcExpr::Expr(expr) => ComprehensionExpr::Expr(self.lower_expr(&expr)),
                ast::LcExpr::BGenerator(bin_gen) => {
                    let pat = self.lower_optional_pat(bin_gen.lhs());
                    let expr = self.lower_optional_expr(bin_gen.rhs());
                    ComprehensionExpr::BinGenerator { pat, expr }
                }
                ast::LcExpr::Generator(list_gen) => {
                    let pat = self.lower_optional_pat(list_gen.lhs());
                    let expr = self.lower_optional_expr(list_gen.rhs());
                    ComprehensionExpr::ListGenerator { pat, expr }
                }
                ast::LcExpr::MapGenerator(map_gen) => {
                    let key = self.lower_optional_pat(map_gen.lhs().and_then(|mf| mf.key()));
                    let value = self.lower_optional_pat(map_gen.lhs().and_then(|mf| mf.value()));
                    let expr = self.lower_optional_expr(map_gen.rhs());
                    ComprehensionExpr::MapGenerator { key, value, expr }
                }
            })
            .collect()
    }

    fn lower_optional_type_expr(&mut self, expr: Option<ast::Expr>) -> TypeExprId {
        if let Some(expr) = &expr {
            self.lower_type_expr(expr)
        } else {
            self.alloc_type_expr(TypeExpr::Missing, None)
        }
    }

    fn lower_type_expr(&mut self, expr: &ast::Expr) -> TypeExprId {
        match expr {
            ast::Expr::ExprMax(expr_max) => self.lower_type_expr_max(expr_max, expr),
            ast::Expr::AnnType(ann) => {
                let ty = self.lower_optional_type_expr(ann.ty());
                if let Some(var) = ann.var().and_then(|var| var.var()) {
                    let var = self.db.var(var.as_name());
                    self.alloc_type_expr(TypeExpr::AnnType { var, ty }, Some(expr))
                } else {
                    self.alloc_type_expr(TypeExpr::Missing, Some(expr))
                }
            }
            ast::Expr::BinaryOpExpr(binary_op) => {
                let lhs = self.lower_optional_type_expr(binary_op.lhs());
                let rhs = self.lower_optional_type_expr(binary_op.rhs());
                if let Some((op, _)) = binary_op.op() {
                    self.alloc_type_expr(TypeExpr::BinaryOp { lhs, op, rhs }, Some(expr))
                } else {
                    self.alloc_type_expr(TypeExpr::Missing, Some(expr))
                }
            }
            ast::Expr::Call(call) => {
                let target = self.lower_type_call_target(call.expr());
                let args = call
                    .args()
                    .iter()
                    .flat_map(|args| args.args())
                    .map(|expr| self.lower_type_expr(&expr))
                    .collect();
                self.alloc_type_expr(TypeExpr::Call { target, args }, Some(expr))
            }
            ast::Expr::CatchExpr(catch) => {
                let _ = self.lower_optional_type_expr(catch.expr());
                self.alloc_type_expr(TypeExpr::Missing, Some(expr))
            }
            ast::Expr::Dotdotdot(_) => self.alloc_type_expr(TypeExpr::Missing, Some(expr)),
            ast::Expr::MapExpr(map) => {
                let fields = map
                    .fields()
                    .flat_map(|field| {
                        let key = self.lower_optional_type_expr(field.key());
                        let value = self.lower_optional_type_expr(field.value());
                        Some((key, field.op()?.0, value))
                    })
                    .collect();
                self.alloc_type_expr(TypeExpr::Map { fields }, Some(expr))
            }
            ast::Expr::MapExprUpdate(update) => {
                let _ = self.lower_optional_type_expr(update.expr().map(Into::into));
                update.fields().for_each(|field| {
                    let _ = self.lower_optional_type_expr(field.key());
                    let _ = self.lower_optional_type_expr(field.value());
                });
                self.alloc_type_expr(TypeExpr::Missing, Some(expr))
            }
            ast::Expr::MatchExpr(mat) => {
                let _ = self.lower_optional_type_expr(mat.lhs());
                let _ = self.lower_optional_type_expr(mat.rhs());
                self.alloc_type_expr(TypeExpr::Missing, Some(expr))
            }
            ast::Expr::Pipe(pipe) => {
                let mut pipe = pipe.clone();
                let mut types = vec![self.lower_optional_type_expr(pipe.lhs())];
                while let Some(ast::Expr::Pipe(next)) = pipe.rhs() {
                    types.push(self.lower_optional_type_expr(next.lhs()));
                    pipe = next;
                }
                types.push(self.lower_optional_type_expr(pipe.rhs()));
                self.alloc_type_expr(TypeExpr::Union { types }, Some(expr))
            }
            ast::Expr::RangeType(range) => {
                let lhs = self.lower_optional_type_expr(range.lhs());
                let rhs = self.lower_optional_type_expr(range.rhs());
                self.alloc_type_expr(TypeExpr::Range { lhs, rhs }, Some(expr))
            }
            ast::Expr::RecordExpr(record) => {
                let name = record.name().and_then(|n| self.resolve_name(n.name()?));
                let fields = record
                    .fields()
                    .flat_map(|field| {
                        let ty =
                            self.lower_optional_type_expr(field.ty().and_then(|expr| expr.expr()));
                        let name = self.resolve_name(field.name()?)?;
                        Some((name, ty))
                    })
                    .collect();
                if let Some(name) = name {
                    self.alloc_type_expr(TypeExpr::Record { name, fields }, Some(expr))
                } else {
                    self.alloc_type_expr(TypeExpr::Missing, Some(expr))
                }
            }
            ast::Expr::RecordFieldExpr(field) => {
                let _ = self.lower_optional_type_expr(field.expr().map(Into::into));
                self.alloc_type_expr(TypeExpr::Missing, Some(expr))
            }
            ast::Expr::RecordIndexExpr(_index) => {
                self.alloc_type_expr(TypeExpr::Missing, Some(expr))
            }
            ast::Expr::RecordUpdateExpr(update) => {
                let _ = self.lower_optional_type_expr(update.expr().map(Into::into));
                update.fields().for_each(|field| {
                    let _ = field.expr().iter().for_each(|field_expr| {
                        self.lower_optional_type_expr(field_expr.expr());
                    });
                });
                self.alloc_type_expr(TypeExpr::Missing, Some(expr))
            }
            ast::Expr::Remote(remote) => {
                let _ = self.lower_optional_type_expr(
                    remote
                        .module()
                        .and_then(|module| module.module())
                        .map(Into::into),
                );
                let _ = self.lower_optional_type_expr(remote.fun().map(Into::into));
                self.alloc_type_expr(TypeExpr::Missing, Some(expr))
            }
            ast::Expr::UnaryOpExpr(unary_op) => {
                let operand = self.lower_optional_type_expr(unary_op.operand());
                if let Some((op, _)) = unary_op.op() {
                    self.alloc_type_expr(
                        TypeExpr::UnaryOp {
                            type_expr: operand,
                            op,
                        },
                        Some(expr),
                    )
                } else {
                    self.alloc_type_expr(TypeExpr::Missing, Some(expr))
                }
            }
            ast::Expr::CondMatchExpr(cond) => {
                let _ = self.lower_optional_type_expr(cond.lhs());
                let _ = self.lower_optional_type_expr(cond.rhs());
                self.alloc_type_expr(TypeExpr::Missing, Some(expr))
            }
        }
    }

    fn lower_type_call_target(&mut self, expr: Option<ast::Expr>) -> CallTarget<TypeExprId> {
        match expr.as_ref() {
            Some(ast::Expr::ExprMax(ast::ExprMax::ParenExpr(paren))) => {
                self.lower_type_call_target(paren.expr())
            }
            Some(ast::Expr::Remote(remote)) => CallTarget::Remote {
                module: self.lower_optional_type_expr(
                    remote
                        .module()
                        .and_then(|module| module.module())
                        .map(ast::Expr::ExprMax),
                ),
                name: self.lower_optional_type_expr(remote.fun().map(ast::Expr::ExprMax)),
            },
            Some(ast::Expr::ExprMax(ast::ExprMax::MacroCallExpr(call))) => self
                .resolve_macro(call, |this, source, replacement| match replacement {
                    MacroReplacement::BuiltIn(built_in) => {
                        this.lower_built_in_macro(built_in).map(|literal| {
                            let name = this.alloc_type_expr(TypeExpr::Literal(literal), None);
                            this.record_type_source(name, source);
                            CallTarget::Local { name }
                        })
                    }
                    MacroReplacement::Ast(ast::MacroDefReplacement::Expr(expr)) => {
                        Some(this.lower_type_call_target(Some(expr)))
                    }
                    MacroReplacement::Ast(_) => None,
                    // This would mean double parens in the call - invalid
                    MacroReplacement::BuiltInArgs(_, _) | MacroReplacement::AstArgs(_, _) => None,
                })
                .flatten()
                .unwrap_or_else(|| {
                    let _ = call
                        .args()
                        .iter()
                        .flat_map(|args| args.args())
                        .for_each(|expr| {
                            let _ = self.lower_optional_type_expr(expr.expr());
                            let _ = self.lower_optional_type_expr(expr.guard());
                        });
                    CallTarget::Local {
                        name: self.alloc_type_expr(TypeExpr::Missing, expr.as_ref()),
                    }
                }),
            Some(expr) => CallTarget::Local {
                name: self.lower_type_expr(expr),
            },
            None => CallTarget::Local {
                name: self.alloc_type_expr(TypeExpr::Missing, None),
            },
        }
    }

    fn lower_type_expr_max(&mut self, expr_max: &ast::ExprMax, expr: &ast::Expr) -> TypeExprId {
        match expr_max {
            ast::ExprMax::AnonymousFun(_fun) => self.alloc_type_expr(TypeExpr::Missing, Some(expr)),
            ast::ExprMax::Atom(atom) => {
                let atom = self.db.atom(atom.as_name());
                self.alloc_type_expr(TypeExpr::Literal(Literal::Atom(atom)), Some(expr))
            }
            ast::ExprMax::Binary(_bin) => self.alloc_type_expr(TypeExpr::Missing, Some(expr)),
            ast::ExprMax::BinaryComprehension(_bc) => {
                self.alloc_type_expr(TypeExpr::Missing, Some(expr))
            }
            ast::ExprMax::BlockExpr(_block) => self.alloc_type_expr(TypeExpr::Missing, Some(expr)),
            ast::ExprMax::CaseExpr(_case) => self.alloc_type_expr(TypeExpr::Missing, Some(expr)),
            ast::ExprMax::Char(char) => {
                let value = lower_char(char).map_or(TypeExpr::Missing, TypeExpr::Literal);
                self.alloc_type_expr(value, Some(expr))
            }
            ast::ExprMax::Concatables(_concat) => {
                self.alloc_type_expr(TypeExpr::Missing, Some(expr))
            }
            ast::ExprMax::ExternalFun(_fun) => self.alloc_type_expr(TypeExpr::Missing, Some(expr)),
            ast::ExprMax::Float(float) => {
                let value = lower_float(float).map_or(TypeExpr::Missing, TypeExpr::Literal);
                self.alloc_type_expr(value, Some(expr))
            }
            ast::ExprMax::FunType(fun) => match fun.sig() {
                None => self.alloc_type_expr(TypeExpr::Fun(FunType::Any), Some(expr)),
                Some(sig) => {
                    let result = self.lower_optional_type_expr(sig.ty());
                    let mut params = Vec::new();
                    let has_dot_dot_dot =
                        sig.args().iter().flat_map(|args| args.args()).any(|param| {
                            params.push(self.lower_type_expr(&param));
                            matches!(param, ast::Expr::Dotdotdot(_))
                        });
                    let fun = if has_dot_dot_dot {
                        FunType::AnyArgs { result }
                    } else {
                        FunType::Full { params, result }
                    };
                    self.alloc_type_expr(TypeExpr::Fun(fun), Some(expr))
                }
            },
            ast::ExprMax::IfExpr(_if_expr) => self.alloc_type_expr(TypeExpr::Missing, Some(expr)),
            ast::ExprMax::Integer(int) => {
                let value = lower_int(int).map_or(TypeExpr::Missing, TypeExpr::Literal);
                self.alloc_type_expr(value, Some(expr))
            }
            ast::ExprMax::InternalFun(_fun) => self.alloc_type_expr(TypeExpr::Missing, Some(expr)),
            ast::ExprMax::List(list) => {
                let ty = list.exprs().fold(ListType::Empty, |ty, expr| {
                    let elem = self.lower_type_expr(&expr);
                    match ty {
                        ListType::Empty => ListType::Regular(elem),
                        ListType::Regular(elem) if matches!(expr, ast::Expr::Dotdotdot(_)) => {
                            ListType::NonEmpty(elem)
                        }
                        other => other,
                    }
                });
                self.alloc_type_expr(TypeExpr::List(ty), Some(expr))
            }
            ast::ExprMax::ListComprehension(_lc) => {
                self.alloc_type_expr(TypeExpr::Missing, Some(expr))
            }
            ast::ExprMax::MacroCallExpr(call) => self
                .resolve_macro(call, |this, source, replacement| match replacement {
                    MacroReplacement::BuiltIn(built_in) => {
                        this.lower_built_in_macro(built_in).map(|literal| {
                            let type_id = this.alloc_type_expr(TypeExpr::Literal(literal), None);
                            this.record_type_source(type_id, source);
                            type_id
                        })
                    }
                    MacroReplacement::Ast(ast::MacroDefReplacement::Expr(macro_expr)) => {
                        let type_id = this.lower_type_expr(&macro_expr);
                        this.record_type_source(type_id, source);
                        Some(type_id)
                    }
                    MacroReplacement::Ast(_) => None,
                    MacroReplacement::BuiltInArgs(built_in, args) => {
                        let name = this
                            .lower_built_in_macro(built_in)
                            .map(|literal| this.alloc_type_expr(TypeExpr::Literal(literal), None))
                            .unwrap_or_else(|| this.alloc_type_expr(TypeExpr::Missing, None));
                        let target = CallTarget::Local { name };
                        let args = args
                            .args()
                            .map(|expr| this.lower_optional_type_expr(expr.expr()))
                            .collect();
                        let type_id = this.alloc_type_expr(TypeExpr::Call { target, args }, None);
                        this.record_type_source(type_id, source);
                        Some(type_id)
                    }
                    MacroReplacement::AstArgs(
                        ast::MacroDefReplacement::Expr(replacement),
                        args,
                    ) => {
                        let target = this.lower_type_call_target(Some(replacement));
                        let args = args
                            .args()
                            .map(|expr| this.lower_optional_type_expr(expr.expr()))
                            .collect();
                        let type_id = this.alloc_type_expr(TypeExpr::Call { target, args }, None);
                        this.record_type_source(type_id, source);
                        Some(type_id)
                    }
                    MacroReplacement::AstArgs(_, _) => None,
                })
                .flatten()
                .map(|expansion| {
                    let args = call
                        .args()
                        .iter()
                        .flat_map(|args| args.args())
                        .map(|expr| self.lower_optional_expr(expr.expr()))
                        .collect();
                    let expr_id =
                        self.alloc_type_expr(TypeExpr::MacroCall { expansion, args }, Some(expr));
                    expr_id
                })
                .unwrap_or_else(|| {
                    let _ = call
                        .args()
                        .iter()
                        .flat_map(|args| args.args())
                        .for_each(|expr| {
                            let _ = self.lower_optional_type_expr(expr.expr());
                            let _ = self.lower_optional_type_expr(expr.guard());
                        });
                    self.alloc_type_expr(TypeExpr::Missing, Some(expr))
                }),
            ast::ExprMax::MacroString(_) => self.alloc_type_expr(TypeExpr::Missing, Some(expr)),
            ast::ExprMax::ParenExpr(paren_expr) => {
                if let Some(expr) = paren_expr.expr() {
                    let type_expr_id = self.lower_type_expr(&expr);
                    let ptr = AstPtr::new(&expr);
                    let source = InFileAstPtr::new(self.curr_file_id(), ptr);
                    self.record_type_source(type_expr_id, source);
                    type_expr_id
                } else {
                    self.alloc_type_expr(TypeExpr::Missing, Some(expr))
                }
            }
            ast::ExprMax::ReceiveExpr(_receive) => {
                self.alloc_type_expr(TypeExpr::Missing, Some(expr))
            }
            ast::ExprMax::String(str) => {
                let value = lower_str(str).map_or(TypeExpr::Missing, TypeExpr::Literal);
                self.alloc_type_expr(value, Some(expr))
            }
            ast::ExprMax::TryExpr(_try_expr) => self.alloc_type_expr(TypeExpr::Missing, Some(expr)),
            ast::ExprMax::Tuple(tup) => {
                let args = tup.expr().map(|expr| self.lower_type_expr(&expr)).collect();
                self.alloc_type_expr(TypeExpr::Tuple { args }, Some(expr))
            }
            ast::ExprMax::Var(var) => self
                .resolve_var(var, |this, expr| this.lower_optional_type_expr(expr.expr()))
                .unwrap_or_else(|var| self.alloc_type_expr(TypeExpr::Var(var), Some(expr))),
            ast::ExprMax::MaybeExpr(maybe) => {
                maybe.exprs().for_each(|expr| {
                    self.lower_expr(&expr);
                });

                maybe
                    .clauses()
                    .flat_map(|clause| self.lower_cr_clause(clause))
                    .last();
                self.alloc_type_expr(TypeExpr::Missing, Some(expr))
            }
            ExprMax::MapComprehension(_mc) => self.alloc_type_expr(TypeExpr::Missing, Some(expr)),
        }
    }

    fn lower_optional_term(&mut self, expr: Option<ast::Expr>) -> TermId {
        if let Some(expr) = &expr {
            self.lower_term(expr)
        } else {
            self.alloc_term(Term::Missing, None)
        }
    }

    fn lower_term(&mut self, expr: &ast::Expr) -> TermId {
        match expr {
            ast::Expr::ExprMax(expr_max) => self.lower_term_max(expr_max, expr),
            ast::Expr::AnnType(ann) => {
                let _ = self.lower_optional_term(ann.ty());
                self.alloc_term(Term::Missing, Some(expr))
            }
            ast::Expr::BinaryOpExpr(binary_op) => {
                // Interpreting foo/1 as {foo, 1}
                let lhs = self.lower_optional_term(binary_op.lhs());
                let rhs = self.lower_optional_term(binary_op.rhs());
                if matches!(
                    binary_op.op(),
                    Some((ast::BinaryOp::ArithOp(ast::ArithOp::FloatDiv), _))
                ) && matches!(self.body[lhs], Term::Literal(Literal::Atom(_)))
                    && matches!(self.body[rhs], Term::Literal(Literal::Integer(_)))
                {
                    let exprs = vec![lhs, rhs];
                    self.alloc_term(Term::Tuple { exprs }, Some(expr))
                } else {
                    self.alloc_term(Term::Missing, Some(expr))
                }
            }
            ast::Expr::Call(call) => {
                let _ = self.lower_optional_term(call.expr());
                let _ = call
                    .args()
                    .iter()
                    .flat_map(|args| args.args())
                    .for_each(|expr| {
                        let _ = self.lower_term(&expr);
                    });
                self.alloc_term(Term::Missing, Some(expr))
            }
            ast::Expr::CatchExpr(catch) => {
                let _ = self.lower_optional_term(catch.expr());
                self.alloc_term(Term::Missing, Some(expr))
            }
            ast::Expr::Dotdotdot(_) => self.alloc_term(Term::Missing, Some(expr)),
            ast::Expr::MapExpr(map) => {
                let fields = map
                    .fields()
                    .flat_map(|field| {
                        let key = self.lower_optional_term(field.key());
                        let value = self.lower_optional_term(field.value());
                        if let Some((ast::MapOp::Assoc, _)) = field.op() {
                            Some((key, value))
                        } else {
                            None
                        }
                    })
                    .collect();
                self.alloc_term(Term::Map { fields }, Some(expr))
            }
            ast::Expr::MapExprUpdate(update) => {
                let _ = self.lower_optional_term(update.expr().map(Into::into));
                update.fields().for_each(|field| {
                    let _ = self.lower_optional_term(field.key());
                    let _ = self.lower_optional_term(field.value());
                });
                self.alloc_term(Term::Missing, Some(expr))
            }
            ast::Expr::MatchExpr(mat) => {
                let _ = self.lower_optional_term(mat.lhs());
                let _ = self.lower_optional_term(mat.rhs());
                self.alloc_term(Term::Missing, Some(expr))
            }
            ast::Expr::Pipe(pipe) => {
                let _ = self.lower_optional_term(pipe.lhs());
                let _ = self.lower_optional_term(pipe.rhs());
                self.alloc_term(Term::Missing, Some(expr))
            }
            ast::Expr::RangeType(range) => {
                let _ = self.lower_optional_term(range.lhs());
                let _ = self.lower_optional_term(range.rhs());
                self.alloc_term(Term::Missing, Some(expr))
            }
            ast::Expr::RecordExpr(record) => {
                record.fields().for_each(|field| {
                    let _ = self.lower_optional_term(field.ty().and_then(|expr| expr.expr()));
                });
                self.alloc_term(Term::Missing, Some(expr))
            }
            ast::Expr::RecordFieldExpr(field) => {
                let _ = self.lower_optional_term(field.expr().map(Into::into));
                self.alloc_term(Term::Missing, Some(expr))
            }
            ast::Expr::RecordIndexExpr(_index) => self.alloc_term(Term::Missing, Some(expr)),
            ast::Expr::RecordUpdateExpr(update) => {
                let _ = self.lower_optional_term(update.expr().map(Into::into));
                update.fields().for_each(|field| {
                    let _ = self.lower_optional_term(field.expr().and_then(|expr| expr.expr()));
                    let _ = self.lower_optional_term(field.ty().and_then(|expr| expr.expr()));
                });
                self.alloc_term(Term::Missing, Some(expr))
            }
            ast::Expr::Remote(remote) => {
                let _ = self.lower_optional_term(
                    remote
                        .module()
                        .and_then(|module| module.module())
                        .map(Into::into),
                );
                let _ = self.lower_optional_term(remote.fun().map(Into::into));
                self.alloc_term(Term::Missing, Some(expr))
            }
            ast::Expr::UnaryOpExpr(unary_op) => {
                let term = self.lower_optional_term(unary_op.operand());
                match unary_op.op() {
                    Some((ast::UnaryOp::Plus, _)) => {
                        self.alloc_term(self.body[term].clone(), Some(expr))
                    }
                    Some((ast::UnaryOp::Minus, _)) => {
                        if let Term::Literal(literal) = &self.body[term] {
                            let value = literal.negate().map_or(Term::Missing, Term::Literal);
                            self.alloc_term(value, Some(expr))
                        } else {
                            self.alloc_term(Term::Missing, Some(expr))
                        }
                    }
                    _ => self.alloc_term(Term::Missing, Some(expr)),
                }
            }
            ast::Expr::CondMatchExpr(cond) => {
                let _ = self.lower_optional_term(cond.lhs());
                let _ = self.lower_optional_term(cond.rhs());
                self.alloc_term(Term::Missing, Some(expr))
            }
        }
    }

    fn lower_term_max(&mut self, expr_max: &ast::ExprMax, expr: &ast::Expr) -> TermId {
        match expr_max {
            ast::ExprMax::AnonymousFun(_fun) => self.alloc_term(Term::Missing, Some(expr)),
            ast::ExprMax::Atom(atom) => {
                let atom = self.db.atom(atom.as_name());
                self.alloc_term(Term::Literal(Literal::Atom(atom)), Some(expr))
            }
            ast::ExprMax::Binary(bin) => {
                let value = bin
                    .elements()
                    .fold(Term::Binary(Vec::new()), |acc, element| {
                        if let Some(seg) =
                            self.lower_bin_element(&element, Self::lower_optional_term)
                        {
                            match acc {
                                Term::Binary(mut vec) => {
                                    // TODO: process size & unit & types
                                    if seg.size.is_none()
                                        && seg.unit.is_none()
                                        && seg.tys.is_empty()
                                    {
                                        match &self.body[seg.elem] {
                                            Term::Literal(Literal::Char(ch)) => {
                                                vec.push(*ch as u8);
                                                Term::Binary(vec)
                                            }
                                            Term::Literal(Literal::Integer(int)) => {
                                                vec.push(*int as u8);
                                                Term::Binary(vec)
                                            }
                                            Term::Literal(Literal::String(str)) => {
                                                vec.extend(str.chars().map(|ch| ch as u8));
                                                Term::Binary(vec)
                                            }
                                            _ => Term::Missing,
                                        }
                                    } else {
                                        Term::Missing
                                    }
                                }
                                _ => Term::Missing,
                            }
                        } else {
                            acc
                        }
                    });

                self.alloc_term(value, Some(expr))
            }
            ast::ExprMax::BinaryComprehension(_bc) => self.alloc_term(Term::Missing, Some(expr)),
            ast::ExprMax::BlockExpr(_block) => self.alloc_term(Term::Missing, Some(expr)),
            ast::ExprMax::CaseExpr(_case) => self.alloc_term(Term::Missing, Some(expr)),
            ast::ExprMax::Char(char) => {
                let value = lower_char(char).map_or(Term::Missing, Term::Literal);
                self.alloc_term(value, Some(expr))
            }
            ast::ExprMax::Concatables(concat) => {
                let value = lower_concat(concat).map_or(Term::Missing, Term::Literal);
                self.alloc_term(value, Some(expr))
            }
            ast::ExprMax::ExternalFun(fun) => {
                let module = self.lower_optional_term(
                    fun.module()
                        .and_then(|module| module.name())
                        .map(Into::into),
                );
                let name = self.lower_optional_term(fun.fun().map(Into::into));
                let arity = self.lower_optional_term(
                    fun.arity().and_then(|arity| arity.value()).map(Into::into),
                );
                if let (
                    Term::Literal(Literal::Atom(module)),
                    Term::Literal(Literal::Atom(name)),
                    Term::Literal(Literal::Integer(arity)),
                ) = (&self.body[module], &self.body[name], &self.body[arity])
                {
                    if let Ok(arity) = (*arity).try_into() {
                        let term = Term::CaptureFun {
                            module: *module,
                            name: *name,
                            arity,
                        };
                        self.alloc_term(term, Some(expr))
                    } else {
                        self.alloc_term(Term::Missing, Some(expr))
                    }
                } else {
                    self.alloc_term(Term::Missing, Some(expr))
                }
            }
            ast::ExprMax::Float(float) => {
                let value = lower_float(float).map_or(Term::Missing, Term::Literal);
                self.alloc_term(value, Some(expr))
            }
            ast::ExprMax::FunType(fun) => {
                if let Some(sig) = fun.sig() {
                    let _ = self.lower_optional_term(sig.ty());
                    let _ = sig
                        .args()
                        .iter()
                        .flat_map(|args| args.args())
                        .for_each(|pat| {
                            let _ = self.lower_term(&pat);
                        });
                }
                self.alloc_term(Term::Missing, Some(expr))
            }
            ast::ExprMax::IfExpr(_if_expr) => self.alloc_term(Term::Missing, Some(expr)),
            ast::ExprMax::Integer(int) => {
                let value = lower_int(int).map_or(Term::Missing, Term::Literal);
                self.alloc_term(value, Some(expr))
            }
            ast::ExprMax::InternalFun(_fun) => self.alloc_term(Term::Missing, Some(expr)),
            ast::ExprMax::List(list) => {
                let (exprs, tail) = self.lower_list(
                    list,
                    |this| this.alloc_term(Term::Missing, None),
                    |this, expr| this.lower_term(expr),
                );
                self.alloc_term(Term::List { exprs, tail }, Some(expr))
            }
            ast::ExprMax::ListComprehension(_lc) => self.alloc_term(Term::Missing, Some(expr)),
            ast::ExprMax::MacroCallExpr(call) => self
                .resolve_macro(call, |this, source, replacement| match replacement {
                    MacroReplacement::BuiltIn(built_in) => {
                        this.lower_built_in_macro(built_in).map(|literal| {
                            let term_id = this.alloc_term(Term::Literal(literal), None);
                            this.record_term_source(term_id, source);
                            term_id
                        })
                    }
                    MacroReplacement::Ast(ast::MacroDefReplacement::Expr(macro_expr)) => {
                        let term_id = this.lower_term(&macro_expr);
                        this.record_term_source(term_id, source);
                        Some(term_id)
                    }
                    _ => None,
                })
                .flatten()
                .map(|expansion| {
                    let args = call
                        .args()
                        .iter()
                        .flat_map(|args| args.args())
                        .map(|expr| self.lower_optional_expr(expr.expr()))
                        .collect();
                    let expr_id = self.alloc_term(Term::MacroCall { expansion, args }, Some(expr));
                    expr_id
                })
                .unwrap_or_else(|| {
                    let _ = call
                        .args()
                        .iter()
                        .flat_map(|args| args.args())
                        .for_each(|expr| {
                            let _ = self.lower_optional_term(expr.expr());
                            let _ = self.lower_optional_term(expr.guard());
                        });
                    self.alloc_term(Term::Missing, Some(expr))
                }),
            ast::ExprMax::MacroString(_) => self.alloc_term(Term::Missing, Some(expr)),
            ast::ExprMax::ParenExpr(paren_expr) => {
                if let Some(expr) = paren_expr.expr() {
                    self.lower_term(&expr)
                } else {
                    self.alloc_term(Term::Missing, Some(expr))
                }
            }
            ast::ExprMax::ReceiveExpr(_receive) => self.alloc_term(Term::Missing, Some(expr)),
            ast::ExprMax::String(str) => {
                let value = lower_str(str).map_or(Term::Missing, Term::Literal);
                self.alloc_term(value, Some(expr))
            }
            ast::ExprMax::TryExpr(_try_expr) => self.alloc_term(Term::Missing, Some(expr)),
            ast::ExprMax::Tuple(tup) => {
                let exprs = tup.expr().map(|expr| self.lower_term(&expr)).collect();
                self.alloc_term(Term::Tuple { exprs }, Some(expr))
            }
            ast::ExprMax::Var(var) => self
                .resolve_var(var, |this, expr| this.lower_optional_term(expr.expr()))
                .unwrap_or_else(|_var| self.alloc_term(Term::Missing, Some(expr))),
            ast::ExprMax::MaybeExpr(maybe_expr) => {
                maybe_expr.exprs().for_each(|expr| {
                    self.lower_expr(&expr);
                });

                maybe_expr
                    .clauses()
                    .flat_map(|clause| self.lower_cr_clause(clause))
                    .last();
                self.alloc_term(Term::Missing, Some(expr))
            }
            ExprMax::MapComprehension(_mc) => self.alloc_term(Term::Missing, Some(expr)),
        }
    }

    fn lower_built_in_macro(&mut self, built_in: BuiltInMacro) -> Option<Literal> {
        match built_in {
            // This is a bit of a hack, but allows us not to depend on the file system
            // It somewhat replicates the behaviour of -deterministic option
            BuiltInMacro::FILE => {
                let form_list = self.db.file_form_list(self.original_file_id);
                form_list
                    .module_attribute()
                    .map(|attr| Literal::String(format!("{}.erl", attr.name)))
            }
            BuiltInMacro::FUNCTION_NAME => self.function_info.map(|(name, _)| Literal::Atom(name)),
            BuiltInMacro::FUNCTION_ARITY => self
                .function_info
                .map(|(_, arity)| Literal::Integer(arity as i128)),
            // Dummy value, we don't want to depend on the exact position
            BuiltInMacro::LINE => Some(Literal::Integer(0)),
            BuiltInMacro::MODULE => {
                let form_list = self.db.file_form_list(self.original_file_id);
                form_list
                    .module_attribute()
                    .map(|attr| Literal::Atom(self.db.atom(attr.name.clone())))
            }
            BuiltInMacro::MODULE_STRING => {
                let form_list = self.db.file_form_list(self.original_file_id);
                form_list
                    .module_attribute()
                    .map(|attr| Literal::String(attr.name.to_string()))
            }
            BuiltInMacro::MACHINE => Some(Literal::Atom(self.db.atom(known::ELP))),
            // Dummy value, must be an integer
            BuiltInMacro::OTP_RELEASE => Some(Literal::Integer(2000)),
        }
    }

    fn resolve_name(&mut self, name: ast::Name) -> Option<Atom> {
        let expr_id = self.lower_expr(&name.into());
        if let Expr::Literal(Literal::Atom(atom)) = self.body[expr_id] {
            Some(atom)
        } else {
            None
        }
    }

    fn resolve_arity(&mut self, arity: ast::ArityValue) -> Option<i128> {
        let expr_id = self.lower_expr(&arity.into());
        if let Expr::Literal(Literal::Integer(int)) = self.body[expr_id] {
            Some(int)
        } else {
            None
        }
    }

    fn resolve_macro<R>(
        &mut self,
        call: &ast::MacroCallExpr,
        cb: impl FnOnce(&mut Self, ExprSource, MacroReplacement) -> R,
    ) -> Option<R> {
        let name = macro_exp::macro_name(call)?;
        if self.macro_stack().any(|entry| entry.name == name) {
            return None;
        }

        let source = InFileAstPtr::new(self.curr_file_id(), AstPtr::new(call).cast().unwrap());

        match self.db.resolve_macro(self.original_file_id, name.clone()) {
            Some(res @ ResolvedMacro::BuiltIn(built_in)) => {
                self.record_macro_resolution(call, res);
                Some(cb(self, source, MacroReplacement::BuiltIn(built_in)))
            }
            Some(res @ ResolvedMacro::User(def_idx)) => {
                self.record_macro_resolution(call, res);
                self.enter_macro(name, def_idx, call.args(), |this, replacement| {
                    cb(this, source, MacroReplacement::Ast(replacement))
                })
            }
            None => {
                let name = name.with_arity(None);
                let args = call.args()?;
                let res = self.db.resolve_macro(self.original_file_id, name.clone())?;
                self.record_macro_resolution(call, res);
                match res {
                    ResolvedMacro::BuiltIn(built_in) => Some(cb(
                        self,
                        source,
                        MacroReplacement::BuiltInArgs(built_in, args),
                    )),
                    ResolvedMacro::User(def_idx) => {
                        self.enter_macro(name, def_idx, None, |this, replacement| {
                            cb(this, source, MacroReplacement::AstArgs(replacement, args))
                        })
                    }
                }
            }
        }
    }

    fn enter_macro<R>(
        &mut self,
        name: MacroName,
        def_idx: InFile<DefineId>,
        args: Option<ast::MacroCallArgs>,
        cb: impl FnOnce(&mut Self, ast::MacroDefReplacement) -> R,
    ) -> Option<R> {
        let form_list = self.db.file_form_list(def_idx.file_id);
        let define_form_id = form_list[def_idx.value].form_id;
        let source = self.db.parse(def_idx.file_id);
        let define = define_form_id.get(&source.tree());
        let replacement = define.replacement()?;

        let var_map = if let Some(args) = args {
            define
                .args()
                .zip(args.args())
                .map(|(var, arg)| (self.db.var(var.as_name()), arg))
                .collect()
        } else {
            FxHashMap::default()
        };
        let new_stack_id = self.macro_stack.len();
        self.macro_stack.push(MacroStackEntry {
            name,
            file_id: def_idx.file_id,
            var_map,
            parent_id: self.macro_stack_id,
        });
        self.macro_stack_id = new_stack_id;

        let ret = cb(self, replacement);

        let entry = self.macro_stack.pop().expect("BUG: missing stack entry");
        self.macro_stack_id = entry.parent_id;

        Some(ret)
    }

    fn macro_stack(&self) -> impl Iterator<Item = &MacroStackEntry> {
        iter::successors(Some(&self.macro_stack[self.macro_stack_id]), |entry| {
            if entry.parent_id != 0 {
                Some(&self.macro_stack[entry.parent_id])
            } else {
                None
            }
        })
    }

    fn resolve_var<R>(
        &mut self,
        var: &ast::Var,
        cb: impl FnOnce(&mut Self, ast::MacroExpr) -> R,
    ) -> Result<R, Var> {
        let var = self.db.var(var.as_name());
        let entry = &self.macro_stack[self.macro_stack_id];
        if let Some(expr) = entry.var_map.get(&var).cloned() {
            let curr_stack_id = self.macro_stack_id;
            self.macro_stack_id = entry.parent_id;

            let ret = cb(self, expr);

            self.macro_stack_id = curr_stack_id;

            Ok(ret)
        } else {
            Err(var)
        }
    }

    fn alloc_expr(&mut self, expr: Expr, source: Option<&ast::Expr>) -> ExprId {
        let expr_id = self.body.exprs.alloc(expr);
        if let Some(source) = source {
            let ptr = AstPtr::new(source);
            let source = InFileAstPtr::new(self.curr_file_id(), ptr);
            self.record_expr_source(expr_id, source);
        }
        expr_id
    }

    fn record_expr_source(&mut self, expr_id: ExprId, source: ExprSource) {
        self.source_map.expr_map.insert(source, expr_id);
        self.source_map.expr_map_back.insert(expr_id, source);
    }

    fn alloc_pat(&mut self, expr: Pat, source: Option<&ast::Expr>) -> PatId {
        let pat_id = self.body.pats.alloc(expr);
        if let Some(source) = source {
            let ptr = AstPtr::new(source);
            let source = InFileAstPtr::new(self.curr_file_id(), ptr);
            self.record_pat_source(pat_id, source);
        }
        pat_id
    }

    fn record_pat_source(&mut self, pat_id: PatId, source: ExprSource) {
        self.source_map.pat_map.insert(source, pat_id);
        self.source_map.pat_map_back.insert(pat_id, source);
    }

    fn alloc_type_expr(&mut self, type_expr: TypeExpr, source: Option<&ast::Expr>) -> TypeExprId {
        let type_expr_id = self.body.type_exprs.alloc(type_expr);
        if let Some(source) = source {
            let ptr = AstPtr::new(source);
            let source = InFileAstPtr::new(self.curr_file_id(), ptr);
            self.record_type_source(type_expr_id, source);
        }
        type_expr_id
    }

    fn record_type_source(&mut self, type_id: TypeExprId, source: ExprSource) {
        self.source_map.type_expr_map.insert(source, type_id);
        self.source_map.type_expr_map_back.insert(type_id, source);
    }

    fn alloc_term(&mut self, term: Term, source: Option<&ast::Expr>) -> TermId {
        let term_id = self.body.terms.alloc(term);
        if let Some(source) = source {
            let ptr = AstPtr::new(source);
            let source = InFileAstPtr::new(self.curr_file_id(), ptr);
            self.record_term_source(term_id, source);
        }
        term_id
    }

    fn record_term_source(&mut self, term_id: TermId, source: ExprSource) {
        self.source_map.term_map.insert(source, term_id);
        self.source_map.term_map_back.insert(term_id, source);
    }

    fn record_macro_resolution(&mut self, call: &ast::MacroCallExpr, res: ResolvedMacro) {
        let ptr = AstPtr::new(call);
        let source = InFileAstPtr::new(self.curr_file_id(), ptr);
        self.source_map.macro_map.insert(source, res);
    }

    fn curr_file_id(&self) -> FileId {
        self.macro_stack[self.macro_stack_id].file_id
    }
}

fn lower_char(char: &ast::Char) -> Option<Literal> {
    unescape::unescape_string(&char.text())
        .and_then(|str| str.chars().next())
        .map(Literal::Char)
}

fn lower_float(float: &ast::Float) -> Option<Literal> {
    let float: f64 = float.text().parse().ok()?;
    Some(Literal::Float(float.to_bits()))
}

fn lower_raw_int(int: &ast::Integer) -> Option<i128> {
    let text = int.text();
    if text.contains('_') {
        let str = text.replace('_', "");
        str.parse().ok()
    } else {
        text.parse().ok()
    }
}

fn lower_int(int: &ast::Integer) -> Option<Literal> {
    lower_raw_int(int).map(Literal::Integer)
}

fn lower_str(str: &ast::String) -> Option<Literal> {
    Some(Literal::String(
        unescape::unescape_string(&str.text())?.to_string(),
    ))
}

fn lower_concat(concat: &ast::Concatables) -> Option<Literal> {
    let mut buf = String::new();

    for concatable in concat.elems() {
        // TODO: macro resolution
        match concatable {
            ast::Concatable::MacroCallExpr(_) => return None,
            ast::Concatable::MacroString(_) => return None,
            ast::Concatable::String(str) => buf.push_str(&unescape::unescape_string(&str.text())?),
            ast::Concatable::Var(_) => return None,
        }
    }

    Some(Literal::String(buf))
}
