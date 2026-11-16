/*
 * Copyright (c) Mihir Paldhikar
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the “Software”), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::{
    Attribute, Data, DataEnum, DataStruct, DeriveInput, Expr, Fields, GenericParam, Ident,
    Lifetime, LifetimeParam, LitStr, Token, Type, WherePredicate, parse_macro_input,
};

#[proc_macro_derive(FromYaml, attributes(yaml))]
pub fn derive_from_yaml(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_from_yaml(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

#[derive(Clone, Copy, Default)]
enum RenameRule {
    #[default]
    None,
    SnakeCase,
    CamelCase,
    PascalCase,
    ScreamingSnakeCase,
    KebabCase,
}

impl RenameRule {
    fn parse(s: &str, span: Span) -> syn::Result<Self> {
        match s {
            "snake_case" => Ok(Self::SnakeCase),
            "camelCase" => Ok(Self::CamelCase),
            "PascalCase" => Ok(Self::PascalCase),
            "SCREAMING_SNAKE_CASE" => Ok(Self::ScreamingSnakeCase),
            "kebab-case" => Ok(Self::KebabCase),
            _ => Err(syn::Error::new(
                span,
                format!("unknown rename_all rule `{s}`"),
            )),
        }
    }

    fn apply(&self, ident: &str) -> String {
        match self {
            Self::None => ident.to_string(),
            Self::SnakeCase => to_snake(ident),
            Self::ScreamingSnakeCase => to_snake(ident).to_uppercase(),
            Self::KebabCase => to_snake(ident).replace('_', "-"),
            Self::PascalCase => split_words(ident).into_iter().map(capitalize).collect(),
            Self::CamelCase => {
                let mut words = split_words(ident).into_iter();
                let first = words.next().unwrap_or_default().to_lowercase();
                let rest: String = words.map(capitalize).collect();
                format!("{first}{rest}")
            }
        }
    }
}

#[derive(Default)]
struct ContainerOpts {
    deny_unknown_fields: bool,
    rename_all: RenameRule,
    tag: Option<String>,
}

struct FieldOpts {
    rename: Option<String>,
    aliases: Vec<String>,
    default: Option<Option<Expr>>,
    skip: bool,
}

fn parse_container_opts(attrs: &[Attribute]) -> syn::Result<ContainerOpts> {
    let mut opts = ContainerOpts::default();
    for attr in attrs {
        if !attr.path().is_ident("yaml") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("deny_unknown_fields") {
                opts.deny_unknown_fields = true;
                return Ok(());
            }
            if meta.path.is_ident("rename_all") {
                let s: LitStr = meta.value()?.parse()?;
                opts.rename_all = RenameRule::parse(&s.value(), s.span())?;
                return Ok(());
            }
            if meta.path.is_ident("tag") {
                let s: LitStr = meta.value()?.parse()?;
                opts.tag = Some(s.value());
                return Ok(());
            }
            Err(meta.error("unsupported container `#[yaml(...)]` attribute"))
        })?;
    }
    Ok(opts)
}

fn parse_field_opts(attrs: &[Attribute]) -> syn::Result<FieldOpts> {
    let mut opts = FieldOpts {
        rename: None,
        aliases: Vec::new(),
        default: None,
        skip: false,
    };
    for attr in attrs {
        if !attr.path().is_ident("yaml") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") {
                opts.skip = true;
                return Ok(());
            }
            if meta.path.is_ident("rename") {
                let s: LitStr = meta.value()?.parse()?;
                opts.rename = Some(s.value());
                return Ok(());
            }
            if meta.path.is_ident("alias") {
                let s: LitStr = meta.value()?.parse()?;
                opts.aliases.push(s.value());
                return Ok(());
            }
            if meta.path.is_ident("default") {
                if meta.input.peek(Token![=]) {
                    let expr: Expr = meta.value()?.parse()?;
                    opts.default = Some(Some(expr));
                } else {
                    opts.default = Some(None);
                }
                return Ok(());
            }
            Err(meta.error("unsupported field `#[yaml(...)]` attribute"))
        })?;
    }
    Ok(opts)
}

fn is_option_type(ty: &Type) -> bool {
    if let Type::Path(tp) = ty {
        if tp.qself.is_none() {
            if let Some(seg) = tp.path.segments.last() {
                return seg.ident == "Option";
            }
        }
    }
    false
}

fn expand_from_yaml(input: DeriveInput) -> syn::Result<TokenStream2> {
    let opts = parse_container_opts(&input.attrs)?;
    let name = &input.ident;
    let existing_lts: Vec<Lifetime> = input
        .generics
        .lifetimes()
        .map(|l| l.lifetime.clone())
        .collect();
    let (trait_lt, impl_generics, ty_generics, where_clause) =
        build_generics(&input.generics, &existing_lts)?;

    let body = match &input.data {
        Data::Struct(data) => expand_struct(name, data, &opts, &trait_lt)?,
        Data::Enum(data) => expand_enum(name, data, &opts, &trait_lt)?,
        Data::Union(u) => {
            return Err(syn::Error::new_spanned(
                u.union_token,
                "`FromYaml` cannot be derived for unions",
            ));
        }
    };

    Ok(quote! {
        impl #impl_generics ::yamlite::FromYaml<#trait_lt> for #name #ty_generics #where_clause {
            fn from_cursor(
                __yaml_cursor: &mut ::yamlite::TapeCursor<'_, #trait_lt>
            ) -> ::core::result::Result<Self, ::yamlite::YamlError> {
                if let ::core::option::Option::Some(mut __yaml_aliased) = __yaml_cursor.deref_alias()? {
                    return Self::from_cursor(&mut __yaml_aliased);
                }
                #body
            }
        }
    })
}

fn build_generics(
    generics: &syn::Generics,
    existing_lts: &[Lifetime],
) -> syn::Result<(Lifetime, syn::Generics, TokenStream2, TokenStream2)> {
    let mut impl_gens = generics.clone();
    let trait_lt = if existing_lts.len() == 1 {
        existing_lts[0].clone()
    } else {
        let lt = Lifetime::new("'__yaml", Span::call_site());
        let mut lt_param = LifetimeParam::new(lt.clone());
        for existing in existing_lts {
            lt_param.bounds.push(existing.clone());
        }
        impl_gens.params.insert(0, GenericParam::Lifetime(lt_param));
        lt
    };

    let (_, ty_gens, base_where) = generics.split_for_impl();
    let ty_generics = quote!(#ty_gens);

    let mut where_predicates: Vec<WherePredicate> = base_where
        .map(|w| w.predicates.iter().cloned().collect())
        .unwrap_or_default();

    for tp in generics.type_params() {
        let id = &tp.ident;
        where_predicates.push(syn::parse_quote!(#id: ::yamlite::FromYaml<#trait_lt>));
    }

    let where_clause = if where_predicates.is_empty() {
        quote!()
    } else {
        quote!(where #(#where_predicates),*)
    };

    let mut cleaned_impl = syn::Generics::default();
    cleaned_impl.params = impl_gens
        .params
        .into_iter()
        .map(|mut p| {
            if let GenericParam::Type(ref mut tp) = p {
                tp.default = None;
            }
            p
        })
        .collect();

    Ok((trait_lt, cleaned_impl, ty_generics, where_clause))
}

fn expand_struct(
    struct_name: &Ident,
    data: &DataStruct,
    opts: &ContainerOpts,
    trait_lt: &Lifetime,
) -> syn::Result<TokenStream2> {
    if opts.tag.is_some() {
        return Err(syn::Error::new_spanned(
            struct_name,
            "`#[yaml(tag = ...)]` is only valid on enums",
        ));
    }

    match &data.fields {
        Fields::Unit => Ok(quote! {
            __yaml_cursor.expect_unit()?;
            ::core::result::Result::Ok(Self)
        }),

        Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
            let ty = &fields.unnamed[0].ty;
            Ok(quote! {
                let __inner = <#ty as ::yamlite::FromYaml<#trait_lt>>::from_cursor(__yaml_cursor)?;
                ::core::result::Result::Ok(Self(__inner))
            })
        }

        Fields::Unnamed(fields) => {
            let count = fields.unnamed.len();
            let elems: Vec<_> = fields
                .unnamed
                .iter()
                .map(|f| {
                    let ty = &f.ty;
                    quote! {
                        if __yaml_cursor.idx + 1 >= __end_idx {
                            return ::core::result::Result::Err(
                                ::yamlite::YamlError::new(
                                    __start_offset,
                                    ::yamlite::__private::format!(
                                        "expected {} elements in tuple struct `{}`",
                                        #count,
                                        stringify!(#struct_name)
                                    )
                                )
                            );
                        }
                        <#ty as ::yamlite::FromYaml<#trait_lt>>::from_cursor(__yaml_cursor)?
                    }
                })
                .collect();

            Ok(quote! {
                __yaml_cursor.skip_tags();
                let __start = __yaml_cursor.next_node()?;
                let __start_offset = __start.offset as usize;
                let __end_idx = match __start.kind {
                    ::yamlite::TapeKind::SeqStart { end_idx } => end_idx as usize,
                    _ => return ::core::result::Result::Err(
                        ::yamlite::YamlError::new(__start_offset, "expected YAML sequence for tuple struct")
                    ),
                };
                let __val = Self(#(#elems),*);
                if __yaml_cursor.idx + 1 != __end_idx {
                    return ::core::result::Result::Err(
                        ::yamlite::YamlError::new(__start_offset, "too many elements for tuple struct")
                    );
                }
                __yaml_cursor.next_node()?;
                ::core::result::Result::Ok(__val)
            })
        }

        Fields::Named(fields) => {
            expand_named_fields(quote!(Self), &fields.named, opts, trait_lt, None)
        }
    }
}

fn expand_named_fields<'a>(
    constructor: TokenStream2,
    fields: impl IntoIterator<Item = &'a syn::Field>,
    opts: &ContainerOpts,
    trait_lt: &Lifetime,
    skip_tag_key: Option<&str>,
) -> syn::Result<TokenStream2> {
    let mut var_decls = Vec::new();
    let mut match_arms = Vec::new();
    let mut field_assigns = Vec::new();
    let mut null_assigns = Vec::new();
    let mut all_defaultable = true;

    for f in fields {
        let ident = f.ident.as_ref().expect("named field");
        let ty = &f.ty;
        let f_opts = parse_field_opts(&f.attrs)?;
        let var = format_ident!("__field_{}", ident);

        if f_opts.skip {
            let def = match f_opts.default {
                Some(Some(expr)) => quote!(#expr),
                _ => quote!(::core::default::Default::default()),
            };
            field_assigns.push(quote!(#ident: #def));
            null_assigns.push(quote!(#ident: #def));
            continue;
        }

        let primary_key = f_opts
            .rename
            .unwrap_or_else(|| opts.rename_all.apply(&ident.to_string()));
        let mut keys = vec![primary_key.clone()];
        keys.extend(f_opts.aliases);

        var_decls.push(quote! {
            let mut #var: ::core::option::Option<#ty> = ::core::option::Option::None;
        });

        match_arms.push(quote! {
            #(#keys)|* => {
                #var = ::core::option::Option::Some(
                    <#ty as ::yamlite::FromYaml<#trait_lt>>::from_cursor(__yaml_cursor)?
                );
            }
        });

        match f_opts.default {
            Some(Some(expr)) => {
                field_assigns.push(quote!(#ident: #var.unwrap_or_else(|| #expr)));
                null_assigns.push(quote!(#ident: #expr));
            }
            Some(None) => {
                field_assigns.push(quote!(#ident: #var.unwrap_or_default()));
                null_assigns.push(quote!(#ident: ::core::default::Default::default()));
            }
            None if is_option_type(ty) => {
                field_assigns.push(quote!(#ident: #var.unwrap_or(::core::option::Option::None)));
                null_assigns.push(quote!(#ident: ::core::option::Option::None));
            }
            None => {
                all_defaultable = false;
                field_assigns.push(quote! {
                    #ident: #var.ok_or_else(|| {
                        ::yamlite::YamlError::new(
                            __start_offset,
                            ::yamlite::__private::format!("missing required field `{}`", #primary_key)
                        )
                    })?
                });
            }
        }
    }

    let null_arm = if all_defaultable {
        quote! {
            ::yamlite::TapeKind::Null => {
                return ::core::result::Result::Ok(#constructor {
                    #(#null_assigns),*
                });
            }
        }
    } else {
        quote!()
    };

    let tag_skip_arm = skip_tag_key.map(|k| {
        quote! { #k => { __yaml_cursor.skip_value()?; } }
    });

    let (key_offset_decl, unknown_arm) = if opts.deny_unknown_fields {
        (
            quote!(let __key_offset = __yaml_cursor.peek()?.offset as usize;),
            quote! {
                __other => return ::core::result::Result::Err(
                    ::yamlite::YamlError::new(__key_offset, ::yamlite::__private::format!("unknown field `{__other}`"))
                ),
            },
        )
    } else {
        (
            quote!(),
            quote! {
                _ => { __yaml_cursor.skip_value()?; }
            },
        )
    };

    Ok(quote! {
        __yaml_cursor.skip_tags();
        let __start = __yaml_cursor.next_node()?;
        let __start_offset = __start.offset as usize;
        let __end_idx = match __start.kind {
            ::yamlite::TapeKind::MapStart { end_idx } => end_idx as usize,
            #null_arm
            _ => return ::core::result::Result::Err(
                ::yamlite::YamlError::new(__start_offset, "expected YAML mapping")
            ),
        };

        #(#var_decls)*

        while __yaml_cursor.idx + 1 < __end_idx {
            #key_offset_decl
            let __key = __yaml_cursor.read_cow_str()?;
            match __key.as_ref() {
                #tag_skip_arm
                #(#match_arms)*
                #unknown_arm
            }
        }
        __yaml_cursor.next_node()?;

        ::core::result::Result::Ok(#constructor {
            #(#field_assigns),*
        })
    })
}

fn expand_enum(
    enum_name: &Ident,
    data: &DataEnum,
    opts: &ContainerOpts,
    trait_lt: &Lifetime,
) -> syn::Result<TokenStream2> {
    if let Some(tag_key) = &opts.tag {
        return expand_internally_tagged_enum(enum_name, data, opts, tag_key, trait_lt);
    }

    let mut unit_arms = Vec::new();
    let mut map_arms = Vec::new();
    let mut tag_arms = Vec::new();

    for v in &data.variants {
        let v_ident = &v.ident;
        let f_opts = parse_field_opts(&v.attrs)?;
        let v_name = f_opts
            .rename
            .unwrap_or_else(|| opts.rename_all.apply(&v_ident.to_string()));
        let yaml_tag = format!("!{v_name}");

        match &v.fields {
            Fields::Unit => {
                unit_arms.push(quote!(#v_name => ::core::result::Result::Ok(Self::#v_ident),));
                let unit_decode = quote! {
                    __yaml_cursor.expect_unit()?;
                    ::core::result::Result::Ok(Self::#v_ident)
                };
                tag_arms.push(quote!(#yaml_tag => { #unit_decode }));
                map_arms.push(quote!(#v_name => { #unit_decode }));
            }
            Fields::Unnamed(fields) if fields.unnamed.len() == 1 => {
                let ty = &fields.unnamed[0].ty;
                let decode = quote! {
                    ::core::result::Result::Ok(Self::#v_ident(
                        <#ty as ::yamlite::FromYaml<#trait_lt>>::from_cursor(__yaml_cursor)?
                    ))
                };
                tag_arms.push(quote!(#yaml_tag => #decode,));
                map_arms.push(quote!(#v_name => #decode,));
            }
            Fields::Unnamed(fields) => {
                let elems: Vec<_> = fields
                    .unnamed
                    .iter()
                    .map(|f| {
                        let ty = &f.ty;
                        quote!(<#ty as ::yamlite::FromYaml<#trait_lt>>::from_cursor(__yaml_cursor)?)
                    })
                    .collect();
                let decode_seq = quote! {{
                    let __s = __yaml_cursor.next_node()?;
                    let __end = match __s.kind {
                        ::yamlite::TapeKind::SeqStart { end_idx } => end_idx as usize,
                        _ => return ::core::result::Result::Err(
                            ::yamlite::YamlError::new(__s.offset as usize, "expected sequence for tuple variant")
                        ),
                    };
                    let __res = Self::#v_ident(#(#elems),*);
                    if __yaml_cursor.idx + 1 != __end {
                        return ::core::result::Result::Err(
                            ::yamlite::YamlError::new(__s.offset as usize, "variant tuple length mismatch")
                        );
                    }
                    __yaml_cursor.next_node()?;
                    ::core::result::Result::Ok(__res)
                }};
                tag_arms.push(quote!(#yaml_tag => #decode_seq,));
                map_arms.push(quote!(#v_name => #decode_seq,));
            }
            Fields::Named(fields) => {
                let decode_struct = expand_named_fields(
                    quote!(Self::#v_ident),
                    &fields.named,
                    opts,
                    trait_lt,
                    None,
                )?;
                tag_arms.push(quote!(#yaml_tag => { #decode_struct },));
                map_arms.push(quote!(#v_name => { #decode_struct },));
            }
        }
    }

    Ok(quote! {
        if let ::core::option::Option::Some(__tag) = __yaml_cursor.take_tag() {
            return match __tag {
                #(#tag_arms)*
                __other => ::core::result::Result::Err(
                    ::yamlite::YamlError::new(
                        __yaml_cursor.peek().map(|n| n.offset as usize).unwrap_or(0),
                        ::yamlite::__private::format!("unknown enum tag `{__other}` for `{}`", stringify!(#enum_name))
                    )
                ),
            };
        }

        if __yaml_cursor.is_map() {
            let __start = __yaml_cursor.next_node()?;
            let __off = __start.offset as usize;
            let __end_idx = match __start.kind {
                ::yamlite::TapeKind::MapStart { end_idx } => end_idx as usize,
                _ => unreachable!(),
            };
            if __yaml_cursor.idx + 1 >= __end_idx {
                return ::core::result::Result::Err(
                    ::yamlite::YamlError::new(__off, "empty mapping for enum")
                );
            }
            let __variant_key = __yaml_cursor.read_cow_str()?;
            let __val = match __variant_key.as_ref() {
                #(#map_arms)*
                __other => return ::core::result::Result::Err(
                    ::yamlite::YamlError::new(__off, ::yamlite::__private::format!("unknown variant `{__other}`"))
                ),
            }?;
            if __yaml_cursor.idx + 1 != __end_idx {
                return ::core::result::Result::Err(
                    ::yamlite::YamlError::new(__off, "expected single-key mapping for externally tagged enum")
                );
            }
            __yaml_cursor.next_node()?;
            return ::core::result::Result::Ok(__val);
        }

        let __off = __yaml_cursor.peek()?.offset as usize;
        let __s = __yaml_cursor.read_cow_str()?;
        match __s.as_ref() {
            #(#unit_arms)*
            __other => ::core::result::Result::Err(
                ::yamlite::YamlError::new(__off, ::yamlite::__private::format!("unknown variant `{__other}`"))
            ),
        }
    })
}

fn expand_internally_tagged_enum(
    enum_name: &Ident,
    data: &DataEnum,
    opts: &ContainerOpts,
    tag_key: &str,
    trait_lt: &Lifetime,
) -> syn::Result<TokenStream2> {
    let mut arms = Vec::new();
    for v in &data.variants {
        let v_ident = &v.ident;
        let f_opts = parse_field_opts(&v.attrs)?;
        let v_name = f_opts
            .rename
            .unwrap_or_else(|| opts.rename_all.apply(&v_ident.to_string()));

        match &v.fields {
            Fields::Unit => {
                arms.push(quote! {
                    #v_name => {
                        __yaml_cursor.skip_value()?;
                        ::core::result::Result::Ok(Self::#v_ident)
                    }
                });
            }
            Fields::Named(fields) => {
                let body = expand_named_fields(
                    quote!(Self::#v_ident),
                    &fields.named,
                    opts,
                    trait_lt,
                    Some(tag_key),
                )?;
                arms.push(quote!(#v_name => { #body }));
            }
            Fields::Unnamed(_) => {
                return Err(syn::Error::new_spanned(
                    v,
                    "internally tagged `#[yaml(tag = ...)]` enums only support unit and named-field variants",
                ));
            }
        }
    }

    Ok(quote! {
        __yaml_cursor.skip_tags();
        let __saved_idx = __yaml_cursor.idx;
        let __start = __yaml_cursor.next_node()?;
        let __off = __start.offset as usize;
        let __end_idx = match __start.kind {
            ::yamlite::TapeKind::MapStart { end_idx } => end_idx as usize,
            _ => return ::core::result::Result::Err(
                ::yamlite::YamlError::new(__off, "expected YAML mapping for internally tagged enum")
            ),
        };

        let mut __found_tag: ::core::option::Option<::yamlite::__private::Cow<#trait_lt, str>> = ::core::option::Option::None;
        while __yaml_cursor.idx + 1 < __end_idx {
            let __k = __yaml_cursor.read_cow_str()?;
            if __k.as_ref() == #tag_key {
                __found_tag = ::core::option::Option::Some(__yaml_cursor.read_cow_str()?);
                break;
            }
            __yaml_cursor.skip_value()?;
        }

        let __tag_val = __found_tag.ok_or_else(|| {
            ::yamlite::YamlError::new(
                __off,
                ::yamlite::__private::format!("missing tag field `{}` on `{}`", #tag_key, stringify!(#enum_name))
            )
        })?;

        __yaml_cursor.idx = __saved_idx;
        match __tag_val.as_ref() {
            #(#arms)*
            __other => ::core::result::Result::Err(
                ::yamlite::YamlError::new(__off, ::yamlite::__private::format!("unknown tag value `{__other}`"))
            ),
        }
    })
}

fn split_words(s: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = 0;
    let bytes = s.as_bytes();
    for i in 1..bytes.len() {
        if bytes[i] == b'_' || bytes[i] == b'-' {
            if start < i {
                words.push(&s[start..i]);
            }
            start = i + 1;
        } else if bytes[i].is_ascii_uppercase() && bytes[i - 1].is_ascii_lowercase() {
            words.push(&s[start..i]);
            start = i;
        }
    }
    if start < s.len() {
        words.push(&s[start..]);
    }
    words
}

fn to_snake(s: &str) -> String {
    split_words(s)
        .into_iter()
        .map(|w| w.to_lowercase())
        .collect::<Vec<_>>()
        .join("_")
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
    }
}
