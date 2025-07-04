/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ops::Deref;
use std::sync::Arc;

use dupe::Dupe;
use itertools::Either;
use itertools::Itertools;
use pyrefly_util::prelude::SliceExt;
use ruff_python_ast::Expr;
use ruff_python_ast::Identifier;
use ruff_python_ast::name::Name;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use starlark_map::small_map::SmallMap;
use starlark_map::small_set::SmallSet;

use crate::alt::answers::AnswersSolver;
use crate::alt::answers::LookupAnswer;
use crate::alt::solve::TypeFormContext;
use crate::alt::types::class_metadata::ClassMetadata;
use crate::alt::types::class_metadata::DataclassMetadata;
use crate::alt::types::class_metadata::EnumMetadata;
use crate::alt::types::class_metadata::NamedTupleMetadata;
use crate::alt::types::class_metadata::ProtocolMetadata;
use crate::alt::types::class_metadata::TypedDictMetadata;
use crate::binding::binding::Key;
use crate::binding::binding::KeyLegacyTypeParam;
use crate::error::collector::ErrorCollector;
use crate::error::kind::ErrorKind;
use crate::graph::index::Idx;
use crate::module::module_name::ModuleName;
use crate::ruff::ast::Ast;
use crate::types::callable::FunctionKind;
use crate::types::class::Class;
use crate::types::class::ClassType;
use crate::types::literal::Lit;
use crate::types::special_form::SpecialForm;
use crate::types::tuple::Tuple;
use crate::types::types::AnyStyle;
use crate::types::types::CalleeKind;
use crate::types::types::TParam;
use crate::types::types::Type;

/// Private helper type used to share part of the logic needed for the
/// binding-level work of finding legacy type parameters versus the type-level
/// work of computing inherticance information and the MRO.
#[derive(Debug, Clone)]
pub enum BaseClass {
    TypedDict,
    Generic(Vec<Type>),
    Protocol(Vec<Type>),
    Expr(Expr),
    NamedTuple(TextRange),
}

impl BaseClass {
    pub fn can_apply(&self) -> bool {
        matches!(self, BaseClass::Generic(_) | BaseClass::Protocol(_))
    }

    pub fn apply(&mut self, args: Vec<Type>) {
        match self {
            BaseClass::Generic(xs) | BaseClass::Protocol(xs) => {
                xs.extend(args);
            }
            _ => panic!("cannot apply base class"),
        }
    }
}

impl<'a, Ans: LookupAnswer> AnswersSolver<'a, Ans> {
    fn new_type_base(
        &self,
        base_type_and_range: Option<(Type, TextRange)>,
        fallback_range: TextRange,
        errors: &ErrorCollector,
    ) -> Option<(ClassType, Arc<ClassMetadata>)> {
        match base_type_and_range {
            // TODO: raise an error for generic classes and other forbidden types such as hashable
            Some((Type::ClassType(c), range)) => {
                let base_cls = c.class_object();
                let base_class_metadata = self.get_metadata_for_class(base_cls);
                if base_class_metadata.is_protocol() {
                    self.error(
                        errors,
                        range,
                        ErrorKind::InvalidArgument,
                        None,
                        "Second argument to NewType cannot be a protocol".to_owned(),
                    );
                }
                if c.targs().as_slice().iter().any(|ty| {
                    ty.any(|ty| {
                        matches!(
                            ty,
                            Type::TypeVar(_) | Type::TypeVarTuple(_) | Type::ParamSpec(_)
                        )
                    })
                }) {
                    self.error(
                        errors,
                        range,
                        ErrorKind::InvalidArgument,
                        None,
                        "Second argument to NewType cannot be an unbound generic".to_owned(),
                    );
                }
                let metadata = self.get_metadata_for_class(c.class_object());
                Some((c, metadata))
            }
            Some((Type::Tuple(Tuple::Concrete(ts)), _)) => {
                // TODO: we lose ordering/length information when we convert to the class representation
                let class_ty = self.stdlib.tuple(self.unions(ts));
                let metadata = self.get_metadata_for_class(class_ty.class_object());
                Some((class_ty, metadata))
            }
            Some((Type::Tuple(Tuple::Unbounded(t)), _)) => {
                let class_ty = self.stdlib.tuple(*t);
                let metadata = self.get_metadata_for_class(class_ty.class_object());
                Some((class_ty, metadata))
            }
            Some((_, range)) => {
                self.error(
                    errors,
                    range,
                    ErrorKind::InvalidArgument,
                    None,
                    "Second argument to NewType is invalid".to_owned(),
                );
                None
            }
            None => {
                self.error(
                    errors,
                    fallback_range,
                    ErrorKind::InvalidArgument,
                    None,
                    "Second argument to NewType is invalid".to_owned(),
                );
                None
            }
        }
    }

    pub fn class_metadata_of(
        &self,
        cls: &Class,
        bases: &[Expr],
        keywords: &[(Name, Expr)],
        decorators: &[Idx<Key>],
        is_new_type: bool,
        special_base: &Option<Box<BaseClass>>,
        errors: &ErrorCollector,
    ) -> ClassMetadata {
        let mut is_typed_dict = false;
        let mut named_tuple_metadata = None;
        let mut enum_metadata = None;
        let mut dataclass_metadata = None;
        let mut bases: Vec<BaseClass> = bases.map(|x| self.base_class_of(x, errors));
        if let Some(special_base) = special_base {
            bases.push((**special_base).clone());
        }
        let mut protocol_metadata = if bases.iter().any(|x| matches!(x, BaseClass::Protocol(_))) {
            Some(ProtocolMetadata {
                members: cls.fields().cloned().collect(),
                is_runtime_checkable: false,
            })
        } else {
            None
        };
        let mut has_base_any = false;
        let mut has_generic_base_class = false;
        let bases_with_metadata = bases
            .iter()
            .filter_map(|x| {
                let base_type_and_range = match x {
                    BaseClass::Expr(x) => Some((self.expr_untype(x, TypeFormContext::BaseClassList, errors), x.range())),
                    BaseClass::TypedDict => {
                        is_typed_dict = true;
                        None
                    }
                    BaseClass::NamedTuple(range) => {
                        Some((self.stdlib.named_tuple_fallback().clone().to_type(), *range))
                    }
                    BaseClass::Generic(ts) | BaseClass::Protocol(ts) if !ts.is_empty() => {
                        has_generic_base_class = true;
                        None
                    }
                    _ => None,
                };
                if is_new_type {
                    self.new_type_base(base_type_and_range, cls.range(), errors)
                } else {
                    match base_type_and_range {
                        Some((Type::ClassType(c), range)) => {
                            let base_cls = c.class_object();
                            let base_class_metadata = self.get_metadata_for_class(base_cls);
                            if base_class_metadata.has_base_any() {
                                has_base_any = true;
                            }
                            if base_class_metadata.is_typed_dict() {
                                is_typed_dict = true;
                            }
                            if base_class_metadata.is_final() {
                                self.error(errors,
                                    range,
                                    ErrorKind::InvalidInheritance,
                                    None,
                                    format!("Cannot extend final class `{}`", base_cls.name()),
                                );
                            }
                           if base_class_metadata.is_new_type() {
                                self.error(
                                    errors,
                                    range,
                                    ErrorKind::InvalidInheritance,
                                    None,
                                    "Subclassing a NewType not allowed".to_owned(),
                                );
                            }
                            if base_cls.has_qname(ModuleName::type_checker_internals().as_str(), "NamedTupleFallback")
                            {
                                if named_tuple_metadata.is_none() {
                                    named_tuple_metadata = Some(NamedTupleMetadata {
                                        elements: self.get_named_tuple_elements(cls)
                                    })
                                }
                            } else if let Some(base_named_tuple) = base_class_metadata.named_tuple_metadata() {
                                if named_tuple_metadata.is_none() {
                                    named_tuple_metadata = Some(base_named_tuple.clone());
                                }
                            }
                            if let Some(proto) = &mut protocol_metadata {
                                if let Some(base_proto) = base_class_metadata.protocol_metadata() {
                                    proto.members.extend(base_proto.members.iter().cloned());
                                    if base_proto.is_runtime_checkable {
                                        proto.is_runtime_checkable = true;
                                    }
                                } else {
                                    self.error(errors,
                                        range,
                                        ErrorKind::InvalidInheritance,
                                        None,
                                        "If `Protocol` is included as a base class, all other bases must be protocols".to_owned(),
                                    );
                                }
                            }
                            if dataclass_metadata.is_none() && let Some(base_dataclass) = base_class_metadata.dataclass_metadata() {
                                // If we inherit from a dataclass, inherit its metadata. Note that if this class is
                                // itself decorated with @dataclass, we'll compute new metadata and overwrite this.
                                dataclass_metadata = Some(base_dataclass.inherit());
                            }
                            Some((c, base_class_metadata))
                        }
                        Some((Type::Tuple(Tuple::Concrete(ts)), _)) => {
                            // TODO: we lose ordering/length information when we convert to the class representation
                            let class_ty = self.stdlib.tuple(self.unions(ts));
                            let metadata = self.get_metadata_for_class(class_ty.class_object());
                            Some((class_ty, metadata))
                        }
                        Some((Type::Tuple(Tuple::Unbounded(t)), _)) => {
                            let class_ty = self.stdlib.tuple(*t);
                            let metadata = self.get_metadata_for_class(class_ty.class_object());
                            Some((class_ty, metadata))
                        }
                        Some((Type::TypedDict(typed_dict), _)) => {
                            is_typed_dict = true;
                            let class_object = typed_dict.class_object();
                            let class_metadata = self.get_metadata_for_class(class_object);
                            // HACK HACK HACK - TypedDict instances behave very differently from instances of other
                            // classes, so we don't represent them as ClassType in normal typechecking logic. However,
                            // class ancestors are represented as ClassType all over the code base, and changing this
                            // would be quite painful. So we convert TypedDict to ClassType in this one spot. Please do
                            // not do this anywhere else.
                            Some((
                                ClassType::new(typed_dict.class_object().dupe(), typed_dict.targs().clone()),
                                class_metadata,
                            ))
                        }
                        // todo zeina: Ideally, we can directly add this class to the list of base classes. Revisit this when fixing the "Any" representation.  
                        Some((Type::Any(_), _)) => {
                            has_base_any = true;
                            None
                        }
                        Some((t, range)) => {
                            self.error(
                                errors, range, ErrorKind::InvalidInheritance, None,
                                format!("Invalid base class: `{}`", self.for_display(t)));
                            has_base_any = true;
                            None
                        }
                        None => None,
                    }
                }
            })
            .collect::<Vec<_>>();
        if named_tuple_metadata.is_some() && bases_with_metadata.len() > 1 {
            self.error(
                errors,
                cls.range(),
                ErrorKind::InvalidInheritance,
                None,
                "Named tuples do not support multiple inheritance".to_owned(),
            );
        }
        let (metaclasses, keywords): (Vec<_>, Vec<(_, _)>) =
            keywords.iter().partition_map(|(n, x)| match n.as_str() {
                "metaclass" => Either::Left(x),
                _ => Either::Right((n.clone(), self.expr_infer(x, errors))),
            });
        let typed_dict_metadata = if is_typed_dict {
            // Validate that only 'total' keyword is allowed for TypedDict and determine is_total
            let mut is_total = true;
            for (name, value) in &keywords {
                if name.as_str() != "total" {
                    self.error(
                        errors,
                        cls.range(),
                        ErrorKind::BadTypedDict,
                        None,
                        format!(
                            "TypedDict does not support keyword argument `{}`",
                            name.as_str()
                        ),
                    );
                } else if matches!(value, Type::Literal(Lit::Bool(false))) {
                    is_total = false;
                }
            }
            let fields =
                self.calculate_typed_dict_metadata_fields(cls, &bases_with_metadata, is_total);
            Some(TypedDictMetadata { fields })
        } else {
            None
        };
        let base_metaclasses = bases_with_metadata
            .iter()
            .filter_map(|(b, metadata)| metadata.metaclass().map(|m| (b.name(), m)))
            .collect::<Vec<_>>();
        let metaclass = self.calculate_metaclass(
            cls,
            metaclasses.into_iter().next(),
            &base_metaclasses,
            errors,
        );
        if let Some(metaclass) = &metaclass {
            self.check_base_class_metaclasses(cls, metaclass, &base_metaclasses, errors);
            if self.is_subset_eq(
                &Type::ClassType(metaclass.clone()),
                &Type::ClassType(self.stdlib.enum_meta().clone()),
            ) {
                if !cls.tparams().is_empty() {
                    self.error(
                        errors,
                        cls.range(),
                        ErrorKind::InvalidInheritance,
                        None,
                        "Enums may not be generic".to_owned(),
                    );
                }
                enum_metadata = Some(EnumMetadata {
                    // A generic enum is an error, but we create Any type args anyway to handle it gracefully.
                    cls: self.promote_nontypeddict_silently_to_classtype(cls),
                    has_value: bases_with_metadata.iter().any(|(base, _)| {
                        base.class_object().contains(&Name::new_static("_value_"))
                    }),
                    is_flag: bases_with_metadata.iter().any(|(base, _)| {
                        self.is_subset_eq(
                            &Type::ClassType(base.clone()),
                            &Type::ClassType(self.stdlib.enum_flag().clone()),
                        )
                    }),
                })
            }
            if is_typed_dict {
                self.error(
                    errors,
                    cls.range(),
                    ErrorKind::InvalidInheritance,
                    None,
                    "Typed dictionary definitions may not specify a metaclass".to_owned(),
                );
            }
            if metaclass.targs().as_slice().iter().any(|targ| {
                targ.any(|ty| {
                    matches!(
                        ty,
                        Type::TypeVar(_) | Type::TypeVarTuple(_) | Type::ParamSpec(_)
                    )
                })
            }) {
                self.error(
                    errors,
                    cls.range(),
                    ErrorKind::InvalidInheritance,
                    None,
                    "Metaclass may not be an unbound generic".to_owned(),
                );
            }
        }
        let mut is_final = false;
        for decorator in decorators {
            let decorator = self.get_idx(*decorator);
            match decorator.ty().callee_kind() {
                Some(CalleeKind::Function(FunctionKind::Dataclass(kws))) => {
                    let dataclass_fields = self.get_dataclass_fields(cls, &bases_with_metadata);
                    dataclass_metadata = Some(DataclassMetadata {
                        fields: dataclass_fields,
                        kws: *kws,
                    });
                }
                Some(CalleeKind::Function(FunctionKind::Final)) => {
                    is_final = true;
                }
                Some(CalleeKind::Function(FunctionKind::RuntimeCheckable)) => {
                    if let Some(proto) = &mut protocol_metadata {
                        proto.is_runtime_checkable = true;
                    } else {
                        self.error(
                            errors,
                            cls.range(),
                            ErrorKind::InvalidArgument,
                            None,
                            "@runtime_checkable can only be applied to Protocol classes".to_owned(),
                        );
                    }
                }
                _ => {}
            }
        }
        if is_typed_dict
            && let Some(bad) = bases_with_metadata.iter().find(|x| !x.1.is_typed_dict())
        {
            self.error(errors,
                cls.range(),
                ErrorKind::InvalidInheritance,
                None,
                format!("`{}` is not a typed dictionary. Typed dictionary definitions may only extend other typed dictionaries.", bad.0),
            );
        }
        let bases_with_metadata = if is_typed_dict && bases_with_metadata.is_empty() {
            // This is a "fallback" class that contains attributes that are available on all TypedDict subclasses.
            // Note that this also makes those attributes available on *instances* of said subclasses; this is
            // desirable for methods but problematic for fields like `__total__` that should be available on the class
            // but not the instance. For now, we make all fields available on both classes and instances.
            let td_fallback = self.stdlib.typed_dict_fallback();
            vec![(
                td_fallback.clone(),
                self.get_metadata_for_class(td_fallback.class_object()),
            )]
        } else {
            bases_with_metadata
        };
        // We didn't find any type parameters for this class, but it may have ones we don't know about if:
        // - the class inherits from Any, or
        // - the class inherits from Generic[...] or Protocol [...]. We probably dropped the type
        //   arguments because we found an error in them.
        let has_unknown_tparams =
            cls.tparams().is_empty() && (has_base_any || has_generic_base_class);
        ClassMetadata::new(
            cls,
            bases_with_metadata,
            metaclass,
            keywords,
            typed_dict_metadata,
            named_tuple_metadata,
            enum_metadata,
            protocol_metadata,
            dataclass_metadata,
            has_base_any,
            is_new_type,
            is_final,
            has_unknown_tparams,
            errors,
        )
    }

    fn calculate_typed_dict_metadata_fields(
        &self,
        cls: &Class,
        bases_with_metadata: &[(ClassType, Arc<ClassMetadata>)],
        is_total: bool,
    ) -> SmallMap<Name, bool> {
        let mut all_fields = SmallMap::new();
        for (_, metadata) in bases_with_metadata.iter().rev() {
            if let Some(td) = metadata.typed_dict_metadata() {
                all_fields.extend(td.fields.clone());
            }
        }
        for name in cls.fields() {
            if cls.is_field_annotated(name) {
                all_fields.insert(name.clone(), is_total);
            }
        }
        all_fields
    }

    /// This helper deals with special cases where we want to intercept an `Expr`
    /// manually and create a special variant of `BaseClass` instead of calling
    /// `expr_untype` and creating a `BaseClass::Type`.
    ///
    /// TODO(stroxler): See if there's a way to express this more clearly in the types.
    fn special_base_class(&self, base_expr: &Expr, errors: &ErrorCollector) -> Option<BaseClass> {
        let name = match base_expr {
            Expr::Name(x) => &x.id,
            Expr::Attribute(x) => &x.attr.id,
            _ => return None,
        };
        if !["Protocol", "Generic", "TypedDict", "NamedTuple"].contains(&name.as_str()) {
            // Calling expr_infer when figuring out the base class leads to cycles, so we really want to try
            // and avoid doing it unless there is a high likelihood of a special form.
            // Downside is that you can't alias `Generic` etc, but I'm not sure you should want to.
            return None;
        }

        match self.expr_infer(base_expr, errors) {
            Type::Type(box Type::SpecialForm(special)) => match special {
                SpecialForm::Protocol => Some(BaseClass::Protocol(Vec::new())),
                SpecialForm::Generic => Some(BaseClass::Generic(Vec::new())),
                SpecialForm::TypedDict => Some(BaseClass::TypedDict),
                _ => None,
            },
            Type::ClassDef(cls) if cls.has_qname("typing", "NamedTuple") => {
                Some(BaseClass::NamedTuple(base_expr.range()))
            }
            _ => None,
        }
    }

    pub fn base_class_of(&self, base_expr: &Expr, errors: &ErrorCollector) -> BaseClass {
        if let Some(special_base_class) = self.special_base_class(base_expr, errors) {
            // This branch handles cases like `Protocol`
            special_base_class
        } else if let Expr::Subscript(subscript) = base_expr
            && let Some(mut special_base_class) = self.special_base_class(&subscript.value, errors)
            && special_base_class.can_apply()
        {
            // This branch handles `Generic[...]` and `Protocol[...]`
            let mut type_var_tuple_count = 0;
            let args = Ast::unpack_slice(&subscript.slice).map(|x| {
                let ty = self.expr_untype(x, TypeFormContext::GenericBase, errors);
                if let Type::Unpack(unpacked) = &ty
                    && unpacked.is_kind_type_var_tuple()
                {
                    if type_var_tuple_count == 1 {
                        self.error(
                            errors,
                            x.range(),
                            ErrorKind::InvalidInheritance,
                            None,
                            "There cannot be more than one TypeVarTuple type parameter".to_owned(),
                        );
                    }
                    type_var_tuple_count += 1;
                }
                ty
            });
            special_base_class.apply(args);
            special_base_class
        } else {
            // This branch handles all other base classes.
            BaseClass::Expr(base_expr.clone())
        }
    }

    pub fn class_tparams(
        &self,
        name: &Identifier,
        scoped_tparams: Vec<TParam>,
        bases: Vec<BaseClass>,
        legacy: &[Idx<KeyLegacyTypeParam>],
        errors: &ErrorCollector,
    ) -> Vec<TParam> {
        let legacy_tparams = legacy
            .iter()
            .filter_map(|key| self.get_idx(*key).deref().parameter().cloned())
            .collect::<SmallSet<_>>();
        let legacy_map = legacy_tparams
            .iter()
            .map(|p| (p.quantified.clone(), p))
            .collect::<SmallMap<_, _>>();

        let lookup_tparam = |t: &Type| {
            let (q, kind) = match t {
                Type::Unpack(t) => (t.as_quantified(), "TypeVarTuple"),
                _ => (t.as_quantified(), "type variable"),
            };
            if q.is_none() && !matches!(t, Type::Any(AnyStyle::Error)) {
                self.error(
                    errors,
                    name.range,
                    ErrorKind::InvalidTypeVar,
                    None,
                    format!("Expected a {kind}, got `{}`", self.for_display(t.clone())),
                );
            }
            q.and_then(|q| {
                let p = legacy_map.get(&q);
                if p.is_none() {
                    self.error(
                        errors,
                        name.range,
                        ErrorKind::InvalidTypeVar,
                        None,
                        "Redundant type parameter declaration".to_owned(),
                    );
                }
                p.map(|x| (*x).clone())
            })
        };

        // TODO(stroxler): There are a lot of checks, such as that `Generic` only appears once
        // and no non-type-vars are used, that we can more easily detect in a dedictated class
        // validation step that validates all the bases. We are deferring these for now.
        let mut generic_tparams = SmallSet::new();
        let mut protocol_tparams = SmallSet::new();
        for base in bases.iter() {
            match base {
                BaseClass::Generic(ts) => {
                    for t in ts {
                        if let Some(p) = lookup_tparam(t) {
                            generic_tparams.insert(p);
                        }
                    }
                }
                BaseClass::Protocol(ts) if !ts.is_empty() => {
                    for t in ts {
                        if let Some(p) = lookup_tparam(t) {
                            protocol_tparams.insert(p);
                        }
                    }
                }
                _ => {}
            }
        }
        if !generic_tparams.is_empty() && !protocol_tparams.is_empty() {
            self.error(
                errors,
                name.range,
                ErrorKind::InvalidInheritance,
                None,
                format!(
                    "Class `{}` specifies type parameters in both `Generic` and `Protocol` bases",
                    name.id,
                ),
            );
        }
        // Initialized the tparams: combine scoped and explicit type parameters
        let mut tparams = SmallSet::new();
        tparams.extend(scoped_tparams);
        tparams.extend(generic_tparams);
        tparams.extend(protocol_tparams);
        // Handle implicit tparams: if a Quantified was bound at this scope and is not yet
        // in tparams, we add it. These will be added in left-to-right order.
        let implicit_tparams_okay = tparams.is_empty();
        for p in legacy_tparams.iter() {
            if !tparams.contains(p) {
                if !implicit_tparams_okay {
                    self.error(errors,
                        name.range,
                        ErrorKind::InvalidTypeVar,
                        None,
                        format!(
                            "Class `{}` uses type variables not specified in `Generic` or `Protocol` base",
                            name.id,
                        ),
                    );
                }
                tparams.insert(p.clone());
            }
        }

        tparams.into_iter().collect()
    }

    fn calculate_metaclass(
        &self,
        cls: &Class,
        raw_metaclass: Option<&Expr>,
        base_metaclasses: &[(&Name, &ClassType)],
        errors: &ErrorCollector,
    ) -> Option<ClassType> {
        let direct_meta = raw_metaclass.and_then(|x| self.direct_metaclass(cls, x, errors));

        if let Some(metaclass) = direct_meta {
            Some(metaclass)
        } else {
            let mut inherited_meta: Option<ClassType> = None;
            for (_, m) in base_metaclasses {
                let m = (*m).clone();
                let accept_m = match &inherited_meta {
                    None => true,
                    Some(inherited) => self.is_subset_eq(
                        &Type::ClassType(m.clone()),
                        &Type::ClassType(inherited.clone()),
                    ),
                };
                if accept_m {
                    inherited_meta = Some(m);
                }
            }
            inherited_meta
        }
    }

    fn check_base_class_metaclasses(
        &self,
        cls: &Class,
        metaclass: &ClassType,
        base_metaclasses: &[(&Name, &ClassType)],
        errors: &ErrorCollector,
    ) {
        // It is a runtime error to define a class whose metaclass (whether
        // specified directly or through inheritance) is not a subtype of all
        // base class metaclasses.
        let metaclass_type = Type::ClassType(metaclass.clone());
        for (base_name, m) in base_metaclasses {
            let base_metaclass_type = Type::ClassType((*m).clone());
            if !self
                .solver()
                .is_subset_eq(&metaclass_type, &base_metaclass_type, self.type_order())
            {
                self.error(errors,
                    cls.range(),
                    ErrorKind::InvalidInheritance,
                    None,
                    format!(
                        "Class `{}` has metaclass `{}` which is not a subclass of metaclass `{}` from base class `{}`",
                        cls.name(),
                        self.for_display(metaclass_type.clone()),
                        self.for_display(base_metaclass_type),
                        base_name,
                    ),
                );
            }
        }
    }

    fn direct_metaclass(
        &self,
        cls: &Class,
        raw_metaclass: &Expr,
        errors: &ErrorCollector,
    ) -> Option<ClassType> {
        match self.expr_untype(raw_metaclass, TypeFormContext::BaseClassList, errors) {
            Type::ClassType(meta) => {
                if self.is_subset_eq(
                    &Type::ClassType(meta.clone()),
                    &Type::ClassType(self.stdlib.builtins_type().clone()),
                ) {
                    Some(meta)
                } else {
                    self.error(
                        errors,
                        raw_metaclass.range(),
                        ErrorKind::InvalidInheritance,
                        None,
                        format!(
                            "Metaclass of `{}` has type `{}` which is not a subclass of `type`",
                            cls.name(),
                            self.for_display(Type::ClassType(meta)),
                        ),
                    );
                    None
                }
            }
            ty => {
                self.error(
                    errors,
                    cls.range(),
                    ErrorKind::InvalidInheritance,
                    None,
                    format!(
                        "Metaclass of `{}` has type `{}` that is not a simple class type",
                        cls.name(),
                        self.for_display(ty),
                    ),
                );
                None
            }
        }
    }
}
