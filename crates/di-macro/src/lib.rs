//! # di-macro
//!
//! Proc-macro crate for the Kernway DI system.
//!
//! ## Macros
//!
//! - `#[derive(Component)]` — auto-wiring: reads `#[inject]` fields, generates a `Buildable` impl
//! - `#[component]`         — v0.1 compatibility: marker attribute (no auto-wiring)
//! - `#[controller(path)]`  — HTTP controller bean
//! - `#[route(METHOD, path)]` — HTTP route handler

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    parse_macro_input, punctuated::Punctuated, token::Comma, Data, DataStruct, DeriveInput, Fields,
    FieldsNamed, GenericArgument, ItemStruct, Meta, MetaList, PathArguments, Type,
};

// ============================================================
// #[derive(Component)] — auto-wiring derive macro
// ============================================================

/// Derive macro for DI auto-wiring.
///
/// Reads fields with `#[inject]` and generates a `Buildable` impl.
/// Fields without `#[inject]` use `Default::default()`.
///
/// # Spring equivalent
/// `@Component` + `@Autowired` on fields
///
/// # Example
/// ```rust,ignore
/// #[derive(Component)]
/// pub struct UserService {
///     #[inject]
///     repo: Arc<UserRepository>,   // auto-resolved from AppContext
///
///     cache_ttl: u32,              // uses Default::default() = 0
/// }
///
/// // Generated:
/// impl Buildable for UserService {
///     fn build(ctx: &AppContext) -> Arc<Self> {
///         Arc::new(UserService {
///             repo: ctx.get::<UserRepository>()
///                      .expect("bean `UserRepository` not found — add #[derive(Component)]"),
///             cache_ttl: Default::default(),
///         })
///     }
/// }
/// ```
#[proc_macro_derive(
    Component,
    attributes(inject, provides, post_construct, primary, qualifier, default_impl)
)]
pub fn derive_component(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name  = &input.ident;

    // --- Bean metadata: `#[primary]`, `#[qualifier("…")]`, `#[default_impl]` ---
    let bean_qualifier: Option<String> = match parse_bean_qualifier(&input.attrs) {
        Ok(q) => q,
        Err(e) => return e.to_compile_error().into(),
    };
    let is_primary   = input.attrs.iter().any(|a| a.path().is_ident("primary"));
    let is_default   = input.attrs.iter().any(|a| a.path().is_ident("default_impl"));

    // --- Interface bindings: `#[provides(dyn Trait)]` on the struct ---
    let provided: Vec<Type> = match parse_provides(&input.attrs) {
        Ok(v) => v,
        Err(e) => return e.to_compile_error().into(),
    };
    // --- Lifecycle hook: `#[post_construct(method)]` on the struct ---
    let post_ctor: Option<syn::Ident> = match parse_post_construct(&input.attrs) {
        Ok(v) => v,
        Err(e) => return e.to_compile_error().into(),
    };

    // --- Fields: initializers + hard/soft dependency TypeIds ---
    let mut field_inits: Vec<TokenStream2> = Vec::new();
    let mut hard_deps:   Vec<TokenStream2> = Vec::new();
    let mut soft_deps:   Vec<TokenStream2> = Vec::new();
    let is_unit;

    match &input.data {
        Data::Struct(DataStruct { fields: Fields::Named(FieldsNamed { named, .. }), .. }) => {
            is_unit = false;
            for field in named {
                let fname = field.ident.as_ref().unwrap();
                let ftype = &field.ty;
                let Some(attr) = field.attrs.iter().find(|a| a.path().is_ident("inject")) else {
                    // Not injected → default-initialised.
                    field_inits.push(quote! { #fname: ::std::default::Default::default(), });
                    continue;
                };

                let qualifier = match parse_inject_qualifier(attr) {
                    Ok(q) => q,
                    Err(e) => { field_inits.push(e.to_compile_error()); continue; }
                };

                // Classify the wrapper: Arc<T> | Option<Arc<T>> | Vec<Arc<T>>.
                let (kind, arc_inner) = if let Some(opt) = extract_generic_inner(ftype, "Option") {
                    match extract_arc_inner(opt) {
                        Some(i) => (InjectKind::Optional, i),
                        None => { field_inits.push(inject_type_error(ftype)); continue; }
                    }
                } else if let Some(vec) = extract_generic_inner(ftype, "Vec") {
                    match extract_arc_inner(vec) {
                        Some(i) => (InjectKind::Collection, i),
                        None => { field_inits.push(inject_type_error(ftype)); continue; }
                    }
                } else if let Some(i) = extract_arc_inner(ftype) {
                    (InjectKind::Required, i)
                } else {
                    field_inits.push(inject_type_error(ftype));
                    continue;
                };

                let is_trait = matches!(arc_inner, Type::TraitObject(_));
                let inner = arc_inner;

                // Qualifier only applies to a single required bean.
                if qualifier.is_some() && kind != InjectKind::Required {
                    field_inits.push(syn::Error::new_spanned(
                        ftype,
                        "`qualifier` is only supported on a required `Arc<T>` field, not Option/Vec",
                    ).to_compile_error());
                    continue;
                }

                // Field initializer.
                let init = match kind {
                    InjectKind::Required => match (is_trait, &qualifier) {
                        (false, None)    => quote! { ctx.get::<#inner>()? },
                        (false, Some(q)) => quote! { ctx.get_qualified::<#inner>(#q)? },
                        (true,  None)    => quote! { ctx.get_as::<#inner>()? },
                        (true,  Some(q)) => quote! {{
                            let __bound = ctx.get_qualified::<::std::sync::Arc<#inner>>(#q)?;
                            (*__bound).clone()
                        }},
                    },
                    InjectKind::Optional => if is_trait {
                        quote! { ctx.get_as::<#inner>().ok() }
                    } else {
                        quote! { ctx.get::<#inner>().ok() }
                    },
                    InjectKind::Collection => if is_trait {
                        quote! { ctx.get_all_as::<#inner>() }
                    } else {
                        quote! { ctx.get_all::<#inner>() }
                    },
                };
                field_inits.push(quote! { #fname: #init, });

                // Dependency TypeId (concrete keyed by C, trait by Arc<dyn>).
                let dep = if is_trait {
                    quote! { ::std::any::TypeId::of::<::std::sync::Arc<#inner>>() }
                } else {
                    quote! { ::std::any::TypeId::of::<#inner>() }
                };
                match kind {
                    InjectKind::Required => hard_deps.push(dep),
                    InjectKind::Optional | InjectKind::Collection => soft_deps.push(dep),
                }
            }
        }
        // Unit struct: `pub struct Foo;`  or  empty: `pub struct Foo {}`
        Data::Struct(DataStruct { fields: Fields::Unit, .. })
        | Data::Struct(DataStruct { fields: Fields::Unnamed(_), .. }) => {
            is_unit = true;
        }
        _ => {
            return syn::Error::new_spanned(&input.ident, "#[derive(Component)] only supports structs")
                .to_compile_error()
                .into()
        }
    }

    // Build expression differs: unit structs use `Name` instead of `Name { ... }`.
    let build_expr = if is_unit {
        quote! { ::std::sync::Arc::new(#name) }
    } else {
        quote! { ::std::sync::Arc::new(#name { #(#field_inits)* }) }
    };

    // `#[provides(dyn X)]` → provides TypeIds + binding registration.
    let provide_ids: Vec<TokenStream2> = provided
        .iter()
        .map(|t| quote! { ::std::any::TypeId::of::<::std::sync::Arc<#t>>() })
        .collect();
    let bind_registrations: Vec<TokenStream2> = provided
        .iter()
        .map(|t| quote! {
            // Clone to the concrete `Arc<Self>`, then let-coerce to `Arc<dyn Trait>`.
            let __concrete = ::std::sync::Arc::clone(this);
            let __binding: ::std::sync::Arc<#t> = __concrete;
            // Carries this bean's origin/primary/qualifier onto the binding, so
            // `Arc<dyn Trait>` injection honours `#[primary]` / `#[qualifier]`.
            ctx.register_as_component::<Self, #t>(__binding)?;
        })
        .collect();

    // Bean metadata overrides on `RegistersComponent`.
    let origin_impl = is_default.then(|| quote! {
        fn bean_origin() -> ::di_core::BeanOrigin {
            ::di_core::BeanOrigin::FrameworkDefault
        }
    });
    let primary_impl = is_primary.then(|| quote! {
        fn is_primary() -> bool { true }
    });
    let qualifier_impl = bean_qualifier.map(|q| quote! {
        fn qualifier() -> ::std::option::Option<&'static str> {
            ::std::option::Option::Some(#q)
        }
    });

    // `#[post_construct(method)]` → override the lifecycle hook.
    let post_construct_impl = post_ctor.map(|m| quote! {
        fn post_construct(
            ctx: &::di_core::AppContext,
            this: &::std::sync::Arc<Self>,
        ) -> ::std::result::Result<(), ::di_core::DiError> {
            Self::#m(this, ctx)
        }
    });

    let expanded = quote! {
        impl ::di_core::Buildable for #name {
            fn build<__C: ::di_core::Container + ?Sized>(ctx: &__C)
                -> ::std::result::Result<::std::sync::Arc<Self>, ::di_core::DiError>
            {
                ::std::result::Result::Ok(#build_expr)
            }
        }

        impl ::di_core::RegistersComponent for #name {
            #origin_impl
            #primary_impl
            #qualifier_impl

            fn dependencies() -> ::std::vec::Vec<::std::any::TypeId> {
                ::std::vec![ #(#hard_deps),* ]
            }

            fn optional_dependencies() -> ::std::vec::Vec<::std::any::TypeId> {
                ::std::vec![ #(#soft_deps),* ]
            }

            fn provides() -> ::std::vec::Vec<::std::any::TypeId> {
                let mut __v = ::std::vec![ ::std::any::TypeId::of::<Self>() ];
                #( __v.push(#provide_ids); )*
                __v
            }

            fn register_bindings(
                ctx: &mut ::di_core::AppContext,
                this: &::std::sync::Arc<Self>,
            ) -> ::std::result::Result<(), ::di_core::DiError> {
                #(#bind_registrations)*
                ::std::result::Result::Ok(())
            }

            #post_construct_impl
        }

        impl ::di_core::KernwayComponent for #name {
            fn component_name() -> &'static str {
                stringify!(#name)
            }
        }
    };

    TokenStream::from(expanded)
}

/// Which flavour of `#[inject]` field this is.
#[derive(PartialEq, Eq, Clone, Copy)]
enum InjectKind {
    /// `Arc<T>` — required; missing is an error.
    Required,
    /// `Option<Arc<T>>` — soft; missing → `None`.
    Optional,
    /// `Vec<Arc<T>>` — all matching beans; missing → empty.
    Collection,
}

fn inject_type_error(ftype: &Type) -> TokenStream2 {
    syn::Error::new_spanned(
        ftype,
        "#[inject] field must be `Arc<T>`, `Option<Arc<T>>`, or `Vec<Arc<T>>` \
         (T may be a concrete type or `dyn Trait`)",
    )
    .to_compile_error()
}

// ============================================================
// Helper: extract T from Arc<T> (or any `Wrapper<T>`)
// ============================================================

fn extract_arc_inner(ty: &Type) -> Option<&Type> {
    extract_generic_inner(ty, "Arc")
}

/// Extract the first type argument of `Wrapper<T>` (matched by last path segment).
fn extract_generic_inner<'a>(ty: &'a Type, wrapper: &str) -> Option<&'a Type> {
    if let Type::Path(type_path) = ty {
        let last = type_path.path.segments.last()?;
        if last.ident == wrapper {
            if let PathArguments::AngleBracketed(args) = &last.arguments {
                if let Some(GenericArgument::Type(inner)) = args.args.first() {
                    return Some(inner);
                }
            }
        }
    }
    None
}

// ============================================================
// Helper: parse `#[provides(dyn Trait, dyn Other)]`
// ============================================================

fn parse_provides(attrs: &[syn::Attribute]) -> syn::Result<Vec<Type>> {
    let mut out = Vec::new();
    for attr in attrs.iter().filter(|a| a.path().is_ident("provides")) {
        // Accept a comma-separated list of trait-object types.
        let types = attr.parse_args_with(
            syn::punctuated::Punctuated::<Type, syn::Token![,]>::parse_terminated,
        )?;
        out.extend(types);
    }
    Ok(out)
}

// ============================================================
// Helper: parse `#[post_construct(method_name)]`
// ============================================================

fn parse_post_construct(attrs: &[syn::Attribute]) -> syn::Result<Option<syn::Ident>> {
    let Some(attr) = attrs.iter().find(|a| a.path().is_ident("post_construct")) else {
        return Ok(None);
    };
    // Expect exactly one method identifier: `#[post_construct(start)]`.
    let ident: syn::Ident = attr.parse_args()?;
    Ok(Some(ident))
}

// ============================================================
// Helper: parse `#[qualifier("name")]` on the struct
// ============================================================

/// The name this bean is registered under — applied to the concrete bean *and*
/// to every `#[provides]` trait binding (Spring's `@Qualifier` on the bean).
fn parse_bean_qualifier(attrs: &[syn::Attribute]) -> syn::Result<Option<String>> {
    let mut found: Option<String> = None;
    for attr in attrs.iter().filter(|a| a.path().is_ident("qualifier")) {
        if found.is_some() {
            return Err(syn::Error::new_spanned(attr, "duplicate `#[qualifier]` on this bean"));
        }
        let lit: syn::LitStr = attr.parse_args()?;
        found = Some(lit.value());
    }
    Ok(found)
}

// ============================================================
// Helper: parse the qualifier from `#[inject(qualifier = "name")]`
// ============================================================

fn parse_inject_qualifier(attr: &syn::Attribute) -> syn::Result<Option<String>> {
    // Bare `#[inject]` (path only) → no qualifier.
    if matches!(attr.meta, syn::Meta::Path(_)) {
        return Ok(None);
    }
    let mut qualifier = None;
    attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("qualifier") {
            let lit: syn::LitStr = meta.value()?.parse()?;
            qualifier = Some(lit.value());
            Ok(())
        } else {
            Err(meta.error("unknown #[inject] option — expected `qualifier = \"...\"`"))
        }
    })?;
    Ok(qualifier)
}

// ============================================================
// #[component] — v0.1 attribute (backward compat)
// ============================================================

/// Attribute macro — marks a struct as a DI bean (v0.1 compatibility).
/// Use `#[derive(Component)]` for auto-wiring.
#[proc_macro_attribute]
pub fn component(args: TokenStream, input: TokenStream) -> TokenStream {
    let _ = args;
    let input_struct = parse_macro_input!(input as ItemStruct);
    let struct_name  = &input_struct.ident;

    let expanded = quote! {
        #input_struct

        impl ::di_core::KernwayComponent for #struct_name {
            fn component_name() -> &'static str {
                stringify!(#struct_name)
            }
        }
    };

    TokenStream::from(expanded)
}

/// Field injection marker — used with `#[derive(Component)]`.
/// Place on an `Arc<T>` field so the DI container resolves it automatically.
#[proc_macro_attribute]
pub fn inject(args: TokenStream, input: TokenStream) -> TokenStream {
    let _ = args;
    input
}

/// HTTP Controller bean.
#[proc_macro_attribute]
pub fn controller(args: TokenStream, input: TokenStream) -> TokenStream {
    let input_struct = parse_macro_input!(input as ItemStruct);
    let struct_name  = &input_struct.ident;
    let path_prefix  = args.to_string();
    let path_prefix  = path_prefix.trim().trim_matches('"');

    let expanded = quote! {
        #input_struct

        impl ::di_core::KernwayComponent for #struct_name {
            fn component_name() -> &'static str { stringify!(#struct_name) }
        }

        impl ::di_core::KernwayController for #struct_name {
            fn route_prefix() -> &'static str { #path_prefix }
        }
    };

    TokenStream::from(expanded)
}

/// HTTP route handler.
#[proc_macro_attribute]
pub fn route(args: TokenStream, input: TokenStream) -> TokenStream {
    let _ = args;
    input
}

/// Require role middleware.
#[proc_macro_attribute]
pub fn require_role(args: TokenStream, input: TokenStream) -> TokenStream {
    let _ = args;
    input
}

/// Request validation.
#[proc_macro_attribute]
pub fn validated(args: TokenStream, input: TokenStream) -> TokenStream {
    let _ = args;
    input
}

/// Transaction management.
#[proc_macro_attribute]
pub fn transactional(args: TokenStream, input: TokenStream) -> TokenStream {
    let _ = args;
    input
}

/// Derive `Validate` from `#[validate(...)]` field constraints (Spring's Bean
/// Validation). Generates a `validate` that runs each field's rules and collects
/// **every** failure.
///
/// ```rust,ignore
/// #[derive(Validate)]
/// struct CreateUser {
///     #[validate(not_blank, length(min = 3, max = 50))]
///     name: String,
///     #[validate(email)]
///     email: String,
///     #[validate(range(min = 0, max = 150))]
///     age: u8,
/// }
/// ```
///
/// Rules: `not_blank`, `email` (no args, on `&str`/`String`); `length(min, max)`
/// (on `&str`/`String`); `range(min, max)` (on a numeric field, by value).
#[proc_macro_derive(Validate, attributes(validate))]
pub fn derive_validate(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let fields = match &input.data {
        Data::Struct(DataStruct { fields: Fields::Named(FieldsNamed { named, .. }), .. }) => named,
        _ => {
            return syn::Error::new_spanned(&name, "#[derive(Validate)] requires a struct with named fields")
                .to_compile_error()
                .into();
        }
    };

    let mut checks = Vec::new();
    for field in named_iter(fields) {
        let fname = field.ident.as_ref().unwrap();
        let fstr = fname.to_string();
        for attr in &field.attrs {
            if !attr.path().is_ident("validate") {
                continue;
            }
            let metas = match attr.parse_args_with(Punctuated::<Meta, Comma>::parse_terminated) {
                Ok(metas) => metas,
                Err(e) => return e.to_compile_error().into(),
            };
            for meta in metas {
                match meta {
                    // `not_blank`, `email` — no-arg rules on the string field (by ref).
                    Meta::Path(path) => {
                        let rule = path.segments.last().map(|s| &s.ident);
                        checks.push(quote! {
                            if let ::std::result::Result::Err(__m) =
                                ::kernway_validation::rules::#rule(&self.#fname)
                            {
                                __errors.push(#fstr, __m);
                            }
                        });
                    }
                    // `length(min, max)`, `range(min, max)` — bounded rules.
                    Meta::List(list) => {
                        let rule = list.path.segments.last().map(|s| s.ident.clone());
                        let (min, max) = parse_min_max(&list);
                        let min_tokens = min.map_or_else(|| quote! { None }, |e| quote! { Some(#e) });
                        let max_tokens = max.map_or_else(|| quote! { None }, |e| quote! { Some(#e) });
                        // range takes the numeric field by value; length by ref.
                        let is_range = rule.as_ref().is_some_and(|r| r == "range");
                        let arg = if is_range { quote! { self.#fname } } else { quote! { &self.#fname } };
                        checks.push(quote! {
                            if let ::std::result::Result::Err(__m) =
                                ::kernway_validation::rules::#rule(#arg, #min_tokens, #max_tokens)
                            {
                                __errors.push(#fstr, __m);
                            }
                        });
                    }
                    Meta::NameValue(_) => {}
                }
            }
        }
    }

    let expanded = quote! {
        impl ::kernway_validation::Validate for #name {
            fn validate(&self) -> ::std::result::Result<(), ::kernway_validation::ValidationErrors> {
                let mut __errors = ::kernway_validation::ValidationErrors::new();
                #(#checks)*
                __errors.into_result()
            }
        }
    };
    expanded.into()
}

/// Iterate named fields (small helper so the loop reads cleanly).
fn named_iter(named: &Punctuated<syn::Field, Comma>) -> impl Iterator<Item = &syn::Field> {
    named.iter()
}

/// Pull `min`/`max` expressions out of a `length(min = .., max = ..)` /
/// `range(...)` attribute list; either may be absent.
fn parse_min_max(list: &MetaList) -> (Option<syn::Expr>, Option<syn::Expr>) {
    let mut min = None;
    let mut max = None;
    if let Ok(pairs) = list.parse_args_with(Punctuated::<syn::MetaNameValue, Comma>::parse_terminated) {
        for pair in pairs {
            if pair.path.is_ident("min") {
                min = Some(pair.value);
            } else if pair.path.is_ident("max") {
                max = Some(pair.value);
            }
        }
    }
    (min, max)
}

/// Bind a configuration section to a struct — Spring's `@ConfigurationProperties`
/// (KEP-0007).
///
/// `#[configuration(prefix = "server")]` implements `kernway_config::FromConfig`:
/// each field is read from `server.{field}`, with the field's underscores turned
/// into hyphens (`token_ttl_secs` → `server.token-ttl-secs`). An `Option<T>` field
/// is `None` when the key is absent; any other field falls back to `Default`. Omit
/// the prefix to read top-level keys.
///
/// ```rust,ignore
/// #[configuration(prefix = "server")]
/// struct ServerConfig { port: u16, host: String }
/// // ServerConfig::from_config(&config) → { port: server.port, host: server.host }
/// ```
#[proc_macro_attribute]
pub fn configuration(args: TokenStream, input: TokenStream) -> TokenStream {
    let input_struct = parse_macro_input!(input as ItemStruct);
    let name = &input_struct.ident;

    // `prefix = "server"` → "server"; no arg → "" (top-level keys).
    let prefix = args
        .to_string()
        .split_once('=')
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
        .unwrap_or_default();

    let fields = match &input_struct.fields {
        Fields::Named(named) => &named.named,
        _ => {
            return syn::Error::new_spanned(
                &input_struct,
                "#[configuration] requires a struct with named fields",
            )
            .to_compile_error()
            .into();
        }
    };

    let assigns = fields.iter().map(|field| {
        let fname = field.ident.as_ref().unwrap();
        let ty = &field.ty;
        let suffix = fname.to_string().replace('_', "-");
        let key = if prefix.is_empty() { suffix } else { format!("{prefix}.{suffix}") };
        match extract_generic_inner(ty, "Option") {
            // Option<T> → present-or-absent, no default needed.
            Some(inner) => quote! { #fname: config.get::<#inner>(#key) },
            // Anything else → parse or fall back to Default.
            None => quote! { #fname: config.get::<#ty>(#key).unwrap_or_default() },
        }
    });

    let expanded = quote! {
        #input_struct

        impl ::kernway_config::FromConfig for #name {
            fn from_config(config: &::kernway_config::Config) -> Self {
                Self {
                    #(#assigns),*
                }
            }
        }
    };
    TokenStream::from(expanded)
}
