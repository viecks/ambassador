use proc_macro::TokenStream;
use quote::quote;
use std::default::Default;
use syn::{parse_macro_input, parse_quote, DeriveInput, Generics};
use syn::{punctuated::Punctuated, token::Comma, WherePredicate};
use super::register::{macro_name, match_name};

#[derive(Clone, Debug)]
enum DelegateImplementer {
    Enum {
        variant_idents: Vec<syn::Ident>,
        first_type: syn::Type,
        other_types: Vec<syn::Type>,
        generics: Generics,
    },
    SingleFieldStruct {
        field_ident: syn::Member,
        field_type: syn::Type,
        generics: Generics,
    },
    MultiFieldStruct {
        fields: Vec<(syn::Member, syn::Type)>,
        generics: Generics,
    },
}

impl From<DeriveInput> for DelegateImplementer {
    fn from(input: DeriveInput) -> Self {
        let generics = input.generics;
        let implementer: DelegateImplementer = match input.data {
            syn::Data::Enum(enum_data) => {
                let (variant_idents, mut variant_types) = enum_data
                    .variants
                    .into_iter()
                    .map(|n| {
                        let mut it = n.fields.into_iter();
                        match it.next() {
                            None => panic!("enum variant {} has no fields", n.ident),
                            Some(f) => {
                                if it.count() != 0 {
                                    panic!("enum variant {} has multiple fields", n.ident)
                                };
                                (n.ident, f.ty)
                            }
                        }
                    })
                    .unzip::<_, _, Vec<_>, Vec<_>>();
                let first_type = variant_types.pop().expect("enum has no variants");
                DelegateImplementer::Enum {
                    variant_idents,
                    first_type,
                    other_types: variant_types,
                    generics,
                }
            }
            syn::Data::Struct(struct_data) => match struct_data.fields.len() {
                0 => panic!("struct has no fields"),
                1 => {
                    let field = struct_data.fields.into_iter().next().unwrap();
                    let field_ident = match field.ident {
                        Some(id) => syn::Member::Named(id),
                        None => syn::Member::Unnamed(0.into()),
                    };
                    DelegateImplementer::SingleFieldStruct {
                        field_ident,
                        field_type: field.ty,
                        generics,
                    }
                }
                _ => {
                    let fields = struct_data
                        .fields
                        .into_iter()
                        .enumerate()
                        .map(|(i, field)| match field.ident {
                            Some(id) => (syn::Member::Named(id), field.ty),
                            None => (syn::Member::Unnamed(i.into()), field.ty),
                        })
                        .collect();
                    DelegateImplementer::MultiFieldStruct { fields, generics }
                }
            },
            _ => panic!(
                "ambassador currently only supports #[derive(Delegate)] for: \n\
                 - single-field enums\n\
                 - (tuple) structs"
            ),
        };
        implementer
    }
}

struct DelegateArgs<'a> {
    trait_path_full: &'a syn::Path,
    target: Option<syn::Member>,
    where_clauses: Vec<Punctuated<WherePredicate, Comma>>,
}

impl<'a> DelegateArgs<'a> {
    pub fn from_meta(meta: &'a syn::Meta) -> Self {
        let meta_list = match meta {
            syn::Meta::List(meta_list) => meta_list,
            _ => panic!("Invalid syntax for delegate attribute"),
        };

        let nested_meta_items: Vec<&syn::Meta> = meta_list
            .nested
            .iter()
            .map(|n| match n {
                syn::NestedMeta::Meta(meta) => meta,
                _ => panic!("Invalid syntax for delegate attribute"),
            })
            .collect();
        let trait_path_full = match nested_meta_items[0] {
            syn::Meta::Path(ref path) => path,
            _ => panic!(
                "Invalid syntax for delegate attribute; First value has to be the Trait name"
            ),
        };

        let mut target = None;
        let mut where_clauses = Vec::new();
        for meta_item in nested_meta_items.iter().skip(1) {
            match meta_item {
                syn::Meta::NameValue(name_value) => {
                    if name_value.path.is_ident("target") {
                        match name_value.lit {
                            syn::Lit::Str(ref lit) => {
                                let target_val: syn::Member = lit.parse().expect("Invalid syntax for delegate attribute; Expected ident as value for \"target\"");
                                if target.is_some() {
                                    panic!("\"target\" value for delegate attribute can only be specified once");
                                }

                                target = Some(target_val);
                            }
                            _ => panic!("Invalid syntax for delegate attribute; delegate attribute values have to be strings"),
                        }
                    }
                    if name_value.path.is_ident("where") {
                        match name_value.lit {
                            syn::Lit::Str(ref lit) => {
                                let where_clause_val = lit.parse_with(Punctuated::<WherePredicate, Comma>::parse_terminated).expect("Invalid syntax for delegate attribute; Expected where clause syntax as value for \"where\"");

                                where_clauses.push(where_clause_val);
                            }
                            _ => panic!("Invalid syntax for delegate attribute; delegate attribute values have to be strings"),
                        }
                    }
                }
                _ => panic!("Invalid syntax for delegate attribute"),
            }
        }

        Self {
            trait_path_full,
            target,
            where_clauses,
        }
    }

    /// Select the correct field_ident based on the `target`.
    pub fn get_field(
        &self,
        field_idents: &'a [(syn::Member, syn::Type)],
    ) -> &'a (syn::Member, syn::Type) {
        if self.target.is_none() {
            panic!("\"target\" value on #[delegate] attribute has to be specified for structs with multiple fields");
        }
        let target = self.target.as_ref().unwrap();

        let field = field_idents.iter().find(|n| n.0 == *target);
        if field.is_none() {
            panic!(
                "Unknown field \"{}\" specified as \"target\" value in #[delegate] attribute",
                PrettyTarget(target.clone())
            );
        }
        field.unwrap()
    }

    fn generics_for_impl(
        self,
        implementer: &'a DelegateImplementer,
        ty: &syn::Type,
    ) -> (syn::ImplGenerics<'a>, syn::TypeGenerics<'a>, syn::WhereClause) {
        let generics = match implementer {
            DelegateImplementer::Enum { ref generics, .. } => generics,
            DelegateImplementer::SingleFieldStruct { ref generics, .. } => generics,
            DelegateImplementer::MultiFieldStruct { ref generics, .. } => generics,
        };
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

        // Merges the where clause based on the type generics with all the where clauses specified
        // via "where" macro attributes.
        let Self{ trait_path_full, where_clauses: explicit_where_clauses, .. } = self;
        let merged_where_clause = {
            let clauses_iter = std::iter::empty()
                .chain(where_clause.into_iter().flat_map(|n| n.predicates.clone()))
                .chain(std::iter::once(parse_quote!(#ty : #trait_path_full)))
                .chain(explicit_where_clauses.into_iter().flatten());

            syn::WhereClause {
                where_token: Default::default(),
                predicates: clauses_iter.collect(),
            }
        };

        (impl_generics, ty_generics, merged_where_clause)
    }
}

struct PrettyTarget(syn::Member);

impl std::fmt::Display for PrettyTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        match self.0 {
            syn::Member::Named(ref ident) => write!(f, "{}", ident.to_string()),
            syn::Member::Unnamed(ref index) => write!(f, "{}", index.index),
        }
    }
}

pub fn delegate_macro(input: TokenStream) -> TokenStream {
    // Parse the input tokens into a syntax tree
    let input = parse_macro_input!(input as DeriveInput);
    let implementer = input.clone().into();
    let implementer_ident = input.ident;

    let delegate_attributes: Vec<&syn::Attribute> = input
        .attrs
        .iter()
        .filter(|n| n.path.is_ident("delegate"))
        .collect();
    if delegate_attributes.is_empty() {
        panic!("No #[delegate] attribute specified. If you want to delegate an implementation of trait `SomeTrait` add the attribute:\n#[delegate(SomeTrait)]")
    }

    let mut impl_macros = vec![];

    for delegate_attr in delegate_attributes {
        let meta = delegate_attr.parse_meta().unwrap();
        let args = DelegateArgs::from_meta(&meta);
        let trait_path_full: syn::Path = args.trait_path_full.clone();
        let trait_ident: &syn::Ident = &trait_path_full.segments.last().unwrap().ident;
        let macro_name: syn::Ident = macro_name(trait_ident);

        let impl_macro = match &implementer {
            DelegateImplementer::Enum {
                variant_idents,
                first_type,
                other_types,
                ..
            } => {
                if args.target.is_some() {
                    panic!(
                        "\"target\" value on #[delegate] attribute can not be specified for enums"
                    );
                }
                let (impl_generics, ty_generics, mut where_clause) =
                    args.generics_for_impl(&implementer, first_type);
                let match_name = match_name(trait_ident);
                where_clause.predicates.extend(
                    other_types
                        .into_iter()
                        .map::<WherePredicate, _>(|arg| parse_quote!(#arg : #match_name<#first_type>)),
                );
                let mod_name = quote::format_ident!("ambassador_module_{}", trait_ident);
                quote! {
                    #[allow(non_snake_case)]
                    mod #mod_name {
                        use super::*;
                        #macro_name!{use_assoc_ty_bounds}
                        impl #impl_generics #trait_path_full for #implementer_ident #ty_generics #where_clause {
                            #macro_name!{body_enum(#first_type, (#(#other_types),*), (#(#implementer_ident::#variant_idents),*))}
                        }
                    }
                }
            }
            DelegateImplementer::SingleFieldStruct {
                field_ident,
                field_type,
                ..
            } => {
                if args.target.is_some() {
                    panic!("\"target\" value on #[delegate] attribute can not be specified for structs with a single field");
                }
                let (impl_generics, ty_generics, where_clause) =
                    args.generics_for_impl(&implementer, field_type);

                quote! {
                    impl #impl_generics #trait_ident for #implementer_ident #ty_generics #where_clause {
                        #macro_name!{body_struct(#field_type, #field_ident)}
                    }
                }
            }
            DelegateImplementer::MultiFieldStruct { fields, .. } => {
                let field = args.get_field(fields);
                let field_ident = &field.0;
                let field_type = &field.1;
                let (impl_generics, ty_generics, where_clause) =
                    args.generics_for_impl(&implementer, field_type);

                quote! {
                    impl #impl_generics #trait_ident for #implementer_ident #ty_generics #where_clause {
                        #macro_name!{body_struct(#field_type, #field_ident)}
                    }
                }
            }
        };
        impl_macros.push(impl_macro);
    }

    // Build the output, possibly using quasi-quotation
    let expanded = quote! {
        #(#impl_macros)*
    };

    // Hand the output tokens back to the compiler
    TokenStream::from(expanded)
}
