use super::Analyzer;
use crate::{
    analyzer::{pat::PatMode, Ctx, ScopeKind},
    ty,
    ty::{ClassInstance, FnParam, Tuple, Type, TypeParam},
    validator,
    validator::ValidateWith,
    ValidationResult,
};
use rnode::Fold;
use rnode::FoldWith;
use stc_ts_ast_rnode::RFnDecl;
use stc_ts_ast_rnode::RFnExpr;
use stc_ts_ast_rnode::RFunction;
use stc_ts_ast_rnode::RIdent;
use stc_ts_ast_rnode::RPat;
use stc_ts_ast_rnode::RTsEntityName;
use stc_ts_ast_rnode::RTsKeywordType;
use stc_ts_errors::Error;
use stc_ts_errors::Errors;
use stc_ts_types::{Alias, Interface, Ref};
use swc_common::{Span, Spanned};
use swc_ecma_ast::*;

#[validator]
impl Analyzer<'_, '_> {
    fn validate(&mut self, f: &RFunction) -> ValidationResult<ty::Function> {
        self.record(f);

        self.with_child(ScopeKind::Fn, Default::default(), |child: &mut Analyzer| {
            let mut errors = Errors::default();

            {
                // Validate params
                // TODO: Move this to parser
                let mut has_optional = false;
                for p in &f.params {
                    if has_optional {
                        match p.pat {
                            RPat::Ident(RIdent { optional: true, .. }) | RPat::Rest(..) => {}
                            _ => {
                                child.storage.report(Error::TS1016 { span: p.span() });
                            }
                        }
                    }

                    match p.pat {
                        RPat::Ident(RIdent { optional, .. }) => {
                            // Allow optional after optional parameter
                            if optional {
                                has_optional = true;
                            }
                        }
                        _ => {}
                    }
                }
            }

            let mut type_params = try_opt!(f.type_params.validate_with(child));

            let mut params = {
                let ctx = Ctx {
                    pat_mode: PatMode::Decl,
                    allow_ref_declaring: false,
                    ..child.ctx
                };
                f.params.validate_with(&mut *child.with_ctx(ctx))?
            };

            if !child.is_builtin {
                params = params
                    .into_iter()
                    .map(|param: FnParam| -> Result<_, Error> {
                        let ty = child.expand(param.span, param.ty)?;
                        Ok(FnParam { ty, ..param })
                    })
                    .collect::<Result<_, _>>()?;
            }

            let mut declared_ret_ty = try_opt!(f.return_type.validate_with(child));

            if let Some(ret_ty) = declared_ret_ty {
                let span = ret_ty.span();
                declared_ret_ty = Some(match *ret_ty {
                    Type::Class(cls) => box Type::ClassInstance(ClassInstance {
                        span,
                        ty: box Type::Class(cls),
                        type_args: None,
                    }),

                    _ => ret_ty,
                });
            }

            if let Some(ty) = &mut declared_ret_ty {
                match ty.normalize() {
                    Type::Ref(..) => {
                        child.prevent_expansion(ty);
                    }
                    _ => {}
                }
            }

            let span = f.span;
            let is_async = f.is_async;
            let is_generator = f.is_generator;

            let inferred_return_type = try_opt!(f.body.as_ref().map(
                |body| child.visit_stmts_for_return(span, is_async, is_generator, &body.stmts)
            ));

            let inferred_return_type = match inferred_return_type {
                Some(Some(inferred_return_type)) => {
                    let inferred_return_type = match *inferred_return_type {
                        Type::Ref(ty) => box Type::Ref(child.qualify_ref_type_args(span, ty)?),
                        _ => inferred_return_type,
                    };

                    if let Some(ref declared) = declared_ret_ty {
                        // Expand before assigning
                        let declared = child.expand_fully(f.span, declared.clone(), true)?;
                        let span = inferred_return_type.span();

                        child.assign(&declared, &inferred_return_type, span)?;
                    }

                    inferred_return_type
                }
                Some(None) => {
                    let mut span = f.span;

                    if let Some(ref declared) = declared_ret_ty {
                        span = declared.span();

                        match *declared.normalize() {
                            Type::Keyword(RTsKeywordType {
                                kind: TsKeywordTypeKind::TsAnyKeyword,
                                ..
                            })
                            | Type::Keyword(RTsKeywordType {
                                kind: TsKeywordTypeKind::TsVoidKeyword,
                                ..
                            })
                            | Type::Keyword(RTsKeywordType {
                                kind: TsKeywordTypeKind::TsNeverKeyword,
                                ..
                            }) => {}
                            _ => errors.push(Error::ReturnRequired { span }),
                        }
                    }

                    // No return statement -> void
                    if f.return_type.is_none() {
                        if let Some(m) = &mut child.mutations {
                            if m.for_fns.entry(f.node_id).or_default().ret_ty.is_none() {
                                m.for_fns.entry(f.node_id).or_default().ret_ty =
                                    Some(box Type::Keyword(RTsKeywordType {
                                        span,
                                        kind: TsKeywordTypeKind::TsVoidKeyword,
                                    }));
                            }
                        }
                    }
                    box Type::Keyword(RTsKeywordType {
                        span,
                        kind: TsKeywordTypeKind::TsVoidKeyword,
                    })
                }
                None => Type::any(f.span),
            };

            if f.return_type.is_none() {
                if let Some(m) = &mut child.mutations {
                    if m.for_fns.entry(f.node_id).or_default().ret_ty.is_none() {
                        m.for_fns.entry(f.node_id).or_default().ret_ty =
                            Some(inferred_return_type.clone())
                    }
                }
            }

            child.storage.report_all(errors);

            Ok(ty::Function {
                span: f.span,
                params,
                type_params,
                ret_ty: declared_ret_ty.unwrap_or_else(|| inferred_return_type),
            }
            .into())
        })
    }
}

impl Analyzer<'_, '_> {
    /// Fill type arguments using default value.
    ///
    /// If the referred type has default type parameter, we have to include it
    /// in function type of output (.d.ts)
    fn qualify_ref_type_args(&mut self, span: Span, mut ty: Ref) -> ValidationResult<Ref> {
        let actual_ty = self.type_of_ts_entity_name(
            span,
            self.ctx.module_id,
            &ty.type_name,
            ty.type_args.clone(),
        )?;

        let type_params = match actual_ty.foldable() {
            Type::Alias(Alias {
                type_params: Some(type_params),
                ..
            })
            | Type::Interface(Interface {
                type_params: Some(type_params),
                ..
            })
            | Type::Class(stc_ts_types::Class {
                type_params: Some(type_params),
                ..
            }) => type_params,

            _ => return Ok(ty),
        };

        let arg_cnt = ty.type_args.as_ref().map(|v| v.params.len()).unwrap_or(0);
        if type_params.params.len() <= arg_cnt {
            return Ok(ty);
        }

        self.prevent_expansion(&mut ty);

        if let Some(args) = ty.type_args.as_mut() {
            for (span, default) in type_params
                .params
                .into_iter()
                .skip(arg_cnt)
                .map(|param| (param.span, param.default))
            {
                if let Some(default) = default {
                    args.params.push(default);
                } else {
                    self.storage.report(Error::ImplicitAny { span });
                    args.params.push(Type::any(span));
                }
            }
        }

        Ok(ty)
    }

    /// TODO: Handle recursive funciton
    fn visit_fn(&mut self, name: Option<&RIdent>, f: &RFunction) -> Box<Type> {
        let fn_ty: Result<_, _> = try {
            let no_implicit_any_span = name.as_ref().map(|name| name.span);

            // if let Some(name) = name {
            //     // We use `typeof function` to infer recursive function's return type.
            //     match self.declare_var(
            //         f.span,
            //         VarDeclKind::Var,
            //         name.into(),
            //         Some(Type::Query(QueryType {
            //             span: f.span,
            //             expr: RTsEntityName::Ident(name.clone()).into(),
            //         })),
            //         // value is initialized
            //         true,
            //         // Allow overriding
            //         true,
            //     ) {
            //         Ok(()) => {}
            //         Err(err) => {
            //             self.storage.report(err);
            //         }
            //     }
            // }

            if let Some(name) = name {
                assert_eq!(self.scope.declaring_fn, None);
                self.scope.declaring_fn = Some(name.into());
            }

            let mut fn_ty: ty::Function = f.validate_with(self)?;
            // Handle type parameters in return type.
            fn_ty.ret_ty = fn_ty.ret_ty.fold_with(&mut TypeParamHandler {
                params: fn_ty.type_params.as_ref().map(|v| &*v.params),
            });
            match fn_ty {
                ty::Function { ref mut ret_ty, .. } => {
                    match **ret_ty {
                        // Handle tuple widening of the return type.
                        Type::Tuple(Tuple { ref mut elems, .. }) => {
                            for element in elems.iter_mut() {
                                let span = element.span();

                                match element.ty.normalize() {
                                    Type::Keyword(RTsKeywordType {
                                        kind: TsKeywordTypeKind::TsUndefinedKeyword,
                                        ..
                                    })
                                    | Type::Keyword(RTsKeywordType {
                                        kind: TsKeywordTypeKind::TsNullKeyword,
                                        ..
                                    }) => {}
                                    _ => continue,
                                }

                                //if child.rule.no_implicit_any
                                //    && child.span_allowed_implicit_any != f.span
                                //{
                                //    child.storage.report(Error::ImplicitAny {
                                //        span: no_implicit_any_span.unwrap_or(span),
                                //    });
                                //}

                                element.ty = Type::any(span);
                            }
                        }

                        _ => {}
                    }
                }
            }

            if let Some(name) = name {
                self.scope.declaring_fn = None;
            }

            fn_ty
        };

        match fn_ty {
            Ok(ty) => Type::Function(ty).cheap(),
            Err(err) => {
                self.storage.report(err);
                Type::any(f.span)
            }
        }
    }
}

#[validator]
impl Analyzer<'_, '_> {
    /// NOTE: This method **should not call f.fold_children_with(self)**
    fn validate(&mut self, f: &RFnDecl) {
        let fn_ty = self.visit_fn(Some(&f.ident), &f.function).cheap();

        match self.override_var(VarDeclKind::Var, f.ident.clone().into(), fn_ty) {
            Ok(()) => {}
            Err(err) => {
                self.storage.report(err);
            }
        }

        Ok(())
    }
}

#[validator]
impl Analyzer<'_, '_> {
    /// NOTE: This method **should not call f.fold_children_with(self)**
    fn validate(&mut self, f: &RFnExpr) {
        self.visit_fn(f.ident.as_ref(), &f.function);

        Ok(())
    }
}

struct TypeParamHandler<'a> {
    params: Option<&'a [TypeParam]>,
}

impl Fold<Type> for TypeParamHandler<'_> {
    fn fold(&mut self, ty: Type) -> Type {
        if let Some(params) = self.params {
            let ty: Type = ty.fold_children_with(self);

            match ty {
                Type::Ref(ref r) if r.type_args.is_none() => match r.type_name {
                    RTsEntityName::Ident(ref i) => {
                        //
                        for param in params {
                            if param.name == i {
                                return Type::Param(param.clone());
                            }
                        }
                    }
                    _ => {}
                },

                _ => {}
            }

            ty
        } else {
            ty
        }
    }
}