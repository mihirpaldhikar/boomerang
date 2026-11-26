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
use proc_macro_crate::{FoundCrate, crate_name};
use quote::quote;
use syn::{
    ExprPath, Ident, ItemFn, LitStr, Path, Result, Token, Type, bracketed,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
};

fn resolve_crate(name: &str) -> Path {
    let path_str = match crate_name(name) {
        Ok(FoundCrate::Itself) => "crate".to_string(),
        Ok(FoundCrate::Name(resolved)) => format!("::{}", resolved.replace('-', "_")),
        Err(_) => format!("::{name}"),
    };

    syn::parse_str::<Path>(&path_str).unwrap_or_else(|_| {
        syn::parse_str::<Path>(&format!("::{name}"))
            .unwrap_or_else(|_| panic!("Failed to resolve `{name}` crate."))
    })
}

fn emit_route_registration(
    crate_path: &Path,
    method_ts: proc_macro2::TokenStream,
    route_lit: &LitStr,
    handler_fn_def: proc_macro2::TokenStream,
    middleware: &[ExprPath],
) -> proc_macro2::TokenStream {
    let mw_defs: Vec<proc_macro2::TokenStream> = middleware
        .iter()
        .enumerate()
        .map(|(idx, mw)| {
            let ident = quote::format_ident!("__mw_{idx}");
            quote! {
                fn #ident(
                    call: &mut #crate_path::HttpCall,
                ) -> #crate_path::SmallFuture<'_, ()> {
                    #crate_path::SmallFuture::new(#mw(call))
                }
            }
        })
        .collect();

    let mw_idents: Vec<Ident> = (0..middleware.len())
        .map(|idx| quote::format_ident!("__mw_{idx}"))
        .collect();

    quote! {
        const _: () = {
            #handler_fn_def
            #(#mw_defs)*

            static __MW_SLICE: &[#crate_path::MiddlewareHandler] = &[#(#mw_idents),*];

            static __ROUTE_ENTRY: #crate_path::RouteEntry =
                #crate_path::RouteEntry::new(#crate_path::RouteDefinition::__from_macro(
                    #method_ts,
                    #route_lit,
                    __handler_fn,
                    __MW_SLICE,
                ));

            #[cfg_attr(
                any(
                    target_os = "linux",
                    target_os = "android",
                    target_os = "freebsd",
                    target_os = "netbsd",
                    target_os = "openbsd",
                    target_os = "dragonfly",
                    target_os = "illumos",
                    target_os = "haiku"
                ),
                unsafe(link_section = ".text.startup")
            )]
            extern "C" fn __register_route_ctor() {
                #crate_path::__register_route(&__ROUTE_ENTRY);
            }

            #[used]
            #[cfg_attr(
                any(
                    target_os = "linux",
                    target_os = "android",
                    target_os = "freebsd",
                    target_os = "netbsd",
                    target_os = "openbsd",
                    target_os = "dragonfly",
                    target_os = "illumos",
                    target_os = "haiku"
                ),
                unsafe(link_section = ".init_array")
            )]
            #[cfg_attr(
                target_vendor = "apple",
                unsafe(link_section = "__DATA,__mod_init_func")
            )]
            #[cfg_attr(target_os = "windows", unsafe(link_section = ".CRT$XCU"))]
            static __ROUTE_INIT: extern "C" fn() = __register_route_ctor;
        };
    }
}

#[proc_macro_attribute]
pub fn route(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input_fn: ItemFn = match syn::parse(item) {
        Ok(f) => f,
        Err(e) => return e.to_compile_error().into(),
    };

    let parsed = match syn::parse::<RouteAttr>(attr) {
        Ok(v) => v,
        Err(e) => return e.to_compile_error().into(),
    };

    if input_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(input_fn.sig.fn_token, "#[route] requires an `async fn`")
            .to_compile_error()
            .into();
    }
    if !input_fn.sig.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &input_fn.sig.generics,
            "#[route] handler cannot have generic parameters",
        )
        .to_compile_error()
        .into();
    }
    if input_fn.sig.inputs.len() != 1 {
        return syn::Error::new_spanned(
            &input_fn.sig.inputs,
            "#[route] handler must take exactly one argument: `&mut HttpCall`",
        )
        .to_compile_error()
        .into();
    }

    let arg_ok = match input_fn.sig.inputs.first().unwrap() {
        syn::FnArg::Typed(pt) => match &*pt.ty {
            Type::Reference(r) => {
                r.mutability.is_some()
                    && matches!(
                        &*r.elem,
                        Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == "HttpCall")
                    )
            }
            _ => false,
        },
        _ => false,
    };
    if !arg_ok {
        return syn::Error::new_spanned(
            &input_fn.sig.inputs,
            "#[route] handler argument must be `&mut HttpCall`",
        )
        .to_compile_error()
        .into();
    }

    if let syn::ReturnType::Type(_, ty) = &input_fn.sig.output
        && !matches!(ty.as_ref(), Type::Tuple(t) if t.elems.is_empty())
    {
        return syn::Error::new_spanned(
            &input_fn.sig.output,
            "#[route] handler must return `()` (future output = ())",
        )
        .to_compile_error()
        .into();
    }

    if let Err(e) = validate_route_literal(&parsed.path) {
        return e.to_compile_error().into();
    }

    let fn_name = &input_fn.sig.ident;
    let route_lit = &parsed.path;
    let crate_path = resolve_crate("http");
    let method_ts = parsed.method_tokens(&crate_path);

    let registration = emit_route_registration(
        &crate_path,
        method_ts,
        route_lit,
        quote! {
            fn __handler_fn(
                call: &mut #crate_path::HttpCall,
            ) -> #crate_path::SmallFuture<'_, ()> {
                #crate_path::SmallFuture::new(#fn_name(call))
            }
        },
        &parsed.middleware,
    );

    quote! {
        #input_fn
        #registration
    }
    .into()
}

struct RouteAttr {
    path: LitStr,
    method: Option<MethodSpec>,
    middleware: Vec<ExprPath>,
}

enum MethodSpec {
    Ident(Ident),
    Path(Path),
}

impl RouteAttr {
    fn method_tokens(&self, crate_path: &Path) -> proc_macro2::TokenStream {
        match &self.method {
            None => quote!(#crate_path::HttpMethod::GET),
            Some(MethodSpec::Ident(ident)) => quote!(#crate_path::HttpMethod::#ident),
            Some(MethodSpec::Path(p)) => quote!(#p),
        }
    }
}

impl Parse for RouteAttr {
    fn parse(input: ParseStream) -> Result<Self> {
        let path: LitStr = input.parse()?;

        let mut method: Option<MethodSpec> = None;
        let mut middleware: Vec<ExprPath> = Vec::new();

        while input.peek(Token![,]) {
            let _comma: Token![,] = input.parse()?;
            if input.is_empty() {
                break;
            }
            let key: Ident = input.parse()?;
            let _eq: Token![=] = input.parse()?;

            if key == "method" {
                if method.is_some() {
                    return Err(syn::Error::new(key.span(), "duplicate `method` argument"));
                }
                let p: ExprPath = input.parse()?;
                if p.qself.is_some() {
                    return Err(syn::Error::new_spanned(p, "invalid HTTP method path"));
                }
                let segs = &p.path.segments;
                if segs.len() == 1 {
                    let ident = &segs.first().unwrap().ident;
                    let valid = matches!(
                        ident.to_string().as_str(),
                        "GET"
                            | "POST"
                            | "PUT"
                            | "DELETE"
                            | "PATCH"
                            | "OPTIONS"
                            | "HEAD"
                            | "CONNECT"
                            | "TRACE"
                    );
                    if !valid {
                        return Err(syn::Error::new_spanned(
                            ident,
                            "unknown HTTP method; expected GET, POST, PUT, DELETE, PATCH, OPTIONS, HEAD, CONNECT, or TRACE",
                        ));
                    }
                    method = Some(MethodSpec::Ident(ident.clone()));
                } else {
                    method = Some(MethodSpec::Path(p.path));
                }
            } else if key == "middleware" {
                if input.peek(syn::token::Bracket) {
                    let content;
                    bracketed!(content in input);
                    let elems: Punctuated<ExprPath, Token![,]> =
                        content.parse_terminated(ExprPath::parse, Token![,])?;
                    middleware.extend(elems);
                } else {
                    let p: ExprPath = input.parse()?;
                    middleware.push(p);
                }
            } else {
                return Err(syn::Error::new(
                    key.span(),
                    "unknown #[route] argument; expected `method = ...` or `middleware = [...]`",
                ));
            }
        }

        Ok(Self {
            path,
            method,
            middleware,
        })
    }
}

fn validate_route_literal(path: &LitStr) -> Result<()> {
    let s = path.value();

    if !s.starts_with('/') {
        return Err(syn::Error::new(
            path.span(),
            "route must start with '/' (e.g., '/user/{id}')",
        ));
    }
    if s.contains('?') || s.contains('#') || s.bytes().any(|b| b.is_ascii_whitespace()) {
        return Err(syn::Error::new(
            path.span(),
            "route pattern must not contain query strings ('?'), fragments ('#'), or whitespace",
        ));
    }

    let bytes = s.as_bytes();
    let mut seen_params: Vec<&str> = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                let start = i;
                i += 1;
                let name_start = i;
                while i < bytes.len() && bytes[i] != b'}' {
                    if bytes[i] == b'{' {
                        return Err(syn::Error::new(
                            path.span(),
                            format!("unexpected '{{' inside parameter at byte index {}", i),
                        ));
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return Err(syn::Error::new(
                        path.span(),
                        format!("unclosed '{{' at byte index {}", start),
                    ));
                }

                let name = &s[name_start..i];
                if name.is_empty() {
                    return Err(syn::Error::new(
                        path.span(),
                        format!("empty parameter name at byte index {}", start),
                    ));
                }

                if name.starts_with("__") && name.ends_with("__") {
                    return Err(syn::Error::new(
                        path.span(),
                        format!(
                            "parameter name '{}' is reserved (names wrapped in `__` are for internal use only)",
                            name
                        ),
                    ));
                }

                let name_bytes = name.as_bytes();
                if !(name_bytes[0].is_ascii_alphabetic() || name_bytes[0] == b'_')
                    || !name_bytes[1..]
                        .iter()
                        .all(|&b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    return Err(syn::Error::new(
                        path.span(),
                        format!(
                            "invalid parameter name '{}': use [A-Za-z_][A-Za-z0-9_]*",
                            name
                        ),
                    ));
                }

                if seen_params.contains(&name) {
                    return Err(syn::Error::new(
                        path.span(),
                        format!("duplicate parameter name '{}' in route pattern", name),
                    ));
                }
                seen_params.push(name);
            }
            b'}' => {
                return Err(syn::Error::new(
                    path.span(),
                    format!("unexpected '}}' at byte index {}", i),
                ));
            }
            _ => {}
        }
        i += 1;
    }

    Ok(())
}

struct StaticFilesAttr {
    path: LitStr,
    dir: LitStr,
    index: Option<LitStr>,
    middleware: Vec<ExprPath>,
}

impl Parse for StaticFilesAttr {
    fn parse(input: ParseStream) -> Result<Self> {
        let path: LitStr = input.parse()?;
        let mut dir: Option<LitStr> = None;
        let mut index: Option<LitStr> = None;
        let mut middleware: Vec<ExprPath> = Vec::new();

        while input.peek(Token![,]) {
            let _: Token![,] = input.parse()?;
            if input.is_empty() {
                break;
            }

            let key: Ident = input.parse()?;
            let _: Token![=] = input.parse()?;

            if key == "dir" {
                if dir.is_some() {
                    return Err(syn::Error::new(key.span(), "duplicate `dir` argument"));
                }
                dir = Some(input.parse()?);
            } else if key == "index" {
                if index.is_some() {
                    return Err(syn::Error::new(key.span(), "duplicate `index` argument"));
                }
                index = Some(input.parse()?);
            } else if key == "middleware" {
                if input.peek(syn::token::Bracket) {
                    let content;
                    bracketed!(content in input);
                    let elems: Punctuated<ExprPath, Token![,]> =
                        content.parse_terminated(ExprPath::parse, Token![,])?;
                    middleware.extend(elems);
                } else {
                    middleware.push(input.parse()?);
                }
            } else {
                return Err(syn::Error::new(
                    key.span(),
                    "unknown argument; expected `dir`, `index`, or `middleware`",
                ));
            }
        }

        Ok(Self {
            path,
            dir: dir.ok_or_else(|| {
                syn::Error::new(proc_macro2::Span::call_site(), "`dir` is required")
            })?,
            index,
            middleware,
        })
    }
}

#[proc_macro_attribute]
pub fn static_files(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input_fn: ItemFn = match syn::parse(item) {
        Ok(f) => f,
        Err(e) => return e.to_compile_error().into(),
    };

    let parsed = match syn::parse::<StaticFilesAttr>(attr) {
        Ok(v) => v,
        Err(e) => return e.to_compile_error().into(),
    };

    let route_str = parsed.path.value();
    if !route_str.starts_with('/') {
        return syn::Error::new(
            parsed.path.span(),
            "#[static_files] route must start with '/'",
        )
        .to_compile_error()
        .into();
    }
    if route_str.contains('?')
        || route_str.contains('#')
        || route_str.bytes().any(|b| b.is_ascii_whitespace())
    {
        return syn::Error::new(
            parsed.path.span(),
            "#[static_files] route must not contain '?', '#', or whitespace",
        )
        .to_compile_error()
        .into();
    }
    if route_str.contains('{') || route_str.contains('}') {
        return syn::Error::new(
            parsed.path.span(),
            "#[static_files] route must not contain path parameters — `{__path__}` is appended automatically",
        )
            .to_compile_error()
            .into();
    }

    let dir_str = parsed.dir.value();
    if dir_str.split(['/', '\\']).any(|seg| seg == "..") {
        return syn::Error::new(
            parsed.dir.span(),
            "`dir` must not contain `..` — use an absolute path or a path relative to the project root",
        )
            .to_compile_error()
            .into();
    }

    if input_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            input_fn.sig.fn_token,
            "#[static_files] requires an `async fn`",
        )
        .to_compile_error()
        .into();
    }

    let final_route = if route_str.ends_with('/') {
        format!("{route_str}{{__path__}}")
    } else {
        format!("{route_str}/{{__path__}}")
    };
    let final_route_lit = syn::LitStr::new(&final_route, parsed.path.span());

    let crate_path = resolve_crate("http");
    let dir_lit = &parsed.dir;
    let index_expr = match &parsed.index {
        Some(lit) => quote!(::std::option::Option::Some(#lit)),
        None => quote!(::std::option::Option::None),
    };

    let registration = emit_route_registration(
        &crate_path,
        quote!(#crate_path::HttpMethod::GET),
        &final_route_lit,
        quote! {
            fn __handler_fn(
                call: &mut #crate_path::HttpCall,
            ) -> #crate_path::SmallFuture<'_, ()> {
                #crate_path::SmallFuture::new(#crate_path::static_files::serve_static(
                    call,
                    #crate_path::static_files::StaticFileOptions {
                        dir: #dir_lit,
                        index: #index_expr,
                    },
                ))
            }
        },
        &parsed.middleware,
    );

    quote! {
        #[allow(dead_code)]
        #input_fn
        #registration
    }
    .into()
}

enum JsonKey {
    Lit(LitStr),
    Expr(syn::Expr),
}

enum JsonAst {
    Null,
    Bool(bool),
    Array(Vec<JsonAst>),
    Object(Vec<(JsonKey, JsonAst)>),
    Expr(syn::Expr),
}

impl Parse for JsonAst {
    fn parse(input: ParseStream) -> Result<Self> {
        if input.peek(syn::token::Brace) {
            let content;
            syn::braced!(content in input);
            let mut entries = Vec::new();

            while !content.is_empty() {
                let key = if content.peek(LitStr) {
                    JsonKey::Lit(content.parse()?)
                } else if content.peek(syn::token::Paren) {
                    let inner;
                    syn::parenthesized!(inner in content);
                    JsonKey::Expr(inner.parse()?)
                } else {
                    return Err(
                        content.error("expected string literal or `(expr)` key in `json!` object")
                    );
                };

                let _: Token![:] = content.parse()?;
                let val: JsonAst = content.parse()?;
                entries.push((key, val));

                if content.peek(Token![,]) {
                    let _: Token![,] = content.parse()?;
                } else if !content.is_empty() {
                    return Err(content.error("expected `,` or `}` in `json!` object"));
                }
            }

            return Ok(JsonAst::Object(entries));
        }

        if input.peek(syn::token::Bracket) {
            let content;
            bracketed!(content in input);
            let mut items = Vec::new();

            while !content.is_empty() {
                let val: JsonAst = content.parse()?;
                items.push(val);

                if content.peek(Token![,]) {
                    let _: Token![,] = content.parse()?;
                } else if !content.is_empty() {
                    return Err(content.error("expected `,` or `]` in `json!` array"));
                }
            }

            return Ok(JsonAst::Array(items));
        }

        if input.peek(Ident) {
            let fork = input.fork();
            if let Ok(ident) = fork.parse::<Ident>() {
                if ident == "null" && (fork.is_empty() || fork.peek(Token![,])) {
                    let _: Ident = input.parse()?;
                    return Ok(JsonAst::Null);
                }
            }
        }
        if input.peek(syn::LitBool) {
            let fork = input.fork();
            if let Ok(b) = fork.parse::<syn::LitBool>() {
                if fork.is_empty() || fork.peek(Token![,]) {
                    let b: syn::LitBool = input.parse()?;
                    return Ok(JsonAst::Bool(b.value));
                }
            }
        }

        Ok(JsonAst::Expr(input.parse::<syn::Expr>()?))
    }
}

impl JsonAst {
    fn into_tokens(self, crate_path: &Path) -> proc_macro2::TokenStream {
        match self {
            JsonAst::Null => quote!(#crate_path::__json::JsonValue::Null),
            JsonAst::Bool(b) => quote!(#crate_path::__json::JsonValue::Bool(#b)),
            JsonAst::Array(items) => {
                if items.is_empty() {
                    quote!(#crate_path::__json::JsonValue::Array(::std::vec::Vec::new()))
                } else {
                    let cap = items.len();
                    let elems = items.into_iter().map(|it| it.into_tokens(crate_path));
                    quote! {{
                        let mut __arr = ::std::vec::Vec::with_capacity(#cap);
                        #(__arr.push(#elems);)*
                        #crate_path::__json::JsonValue::Array(__arr)
                    }}
                }
            }
            JsonAst::Object(entries) => {
                if entries.is_empty() {
                    quote!(#crate_path::__json::JsonValue::Object(::std::vec::Vec::new()))
                } else {
                    let cap = entries.len();
                    let pushes = entries.into_iter().map(|(k, v)| {
                        let v_ts = v.into_tokens(crate_path);
                        let k_ts = match k {
                            JsonKey::Lit(lit) => quote!(::std::string::String::from(#lit)),
                            JsonKey::Expr(expr) => quote!(::std::string::String::from(#expr)),
                        };
                        quote!(__obj.push((#k_ts, #v_ts));)
                    });
                    quote! {{
                        let mut __obj = ::std::vec::Vec::with_capacity(#cap);
                        #(#pushes)*
                        #crate_path::__json::JsonValue::Object(__obj)
                    }}
                }
            }
            JsonAst::Expr(expr) => {
                quote!(#crate_path::__json::IntoJsonValue::into_json_value(#expr))
            }
        }
    }
}

#[proc_macro]
pub fn json(input: TokenStream) -> TokenStream {
    let ast = match syn::parse::<JsonAst>(input) {
        Ok(ast) => ast,
        Err(e) => return e.to_compile_error().into(),
    };

    let crate_path = resolve_crate("http");
    ast.into_tokens(&crate_path).into()
}
