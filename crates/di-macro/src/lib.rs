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
    parse_macro_input, punctuated::Punctuated, token::Comma, Attribute, Data, DataStruct,
    DeriveInput, Expr, Fields, FieldsNamed, FnArg, GenericArgument, ImplItem, ItemImpl, ItemStruct,
    Lit, LitStr, Meta, MetaList, Pat, PathArguments, Type,
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

/// HTTP controller — Spring's `@Controller`.
///
/// On an **`impl` block**, `#[controller("/prefix")]` turns each `#[route(METHOD,
/// "/path")]` method into a handler and implements `Controller`, so
/// `AppBuilder::controller(Arc::new(c))` mounts them all. A method is
/// `async fn name(&self, req: Request) -> Response`; `#[require_role("ROLE")]` on it
/// guards the route (403 when the request's `SecurityContext` lacks the role).
///
/// ```rust,ignore
/// #[controller("/users")]
/// impl UserController {
///     #[route(GET, "/{id}")]                 fn get(&self, req: Request) -> Response { todo!() }
///     #[route(DELETE, "/{id}")] #[require_role("ADMIN")]
///                                            fn delete(&self, req: Request) -> Response { todo!() }
/// }
/// ```
///
/// On a **struct**, it keeps the v0.1 marker behaviour (`KernwayComponent` +
/// `KernwayController`).
#[proc_macro_attribute]
pub fn controller(args: TokenStream, input: TokenStream) -> TokenStream {
    // An `impl` block → generate route registration; a struct → the old markers.
    if let Ok(item_impl) = syn::parse::<ItemImpl>(input.clone()) {
        return controller_impl(&args.to_string(), item_impl);
    }
    let input_struct = parse_macro_input!(input as ItemStruct);
    let struct_name = &input_struct.ident;
    let path_prefix = args.to_string();
    let path_prefix = path_prefix.trim().trim_matches('"');

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

/// Generate the `Controller` impl for a `#[controller("/prefix")] impl` block.
fn controller_impl(args: &str, mut item_impl: ItemImpl) -> TokenStream {
    let prefix = args.trim().trim_matches('"').to_string();
    let self_ty = item_impl.self_ty.clone();

    let mut registrations = Vec::new();
    for item in &mut item_impl.items {
        let ImplItem::Fn(method) = item else { continue };
        let route = extract_route(&method.attrs);
        let role = extract_role(&method.attrs);
        // Strip the helper attributes so they do not re-expand on the output.
        method.attrs.retain(|a| !a.path().is_ident("route") && !a.path().is_ident("require_role"));

        let Some((http, path)) = route else { continue };
        let name = method.sig.ident.clone();
        let inputs = method.sig.inputs.clone();
        match route_registration(&name, &inputs, &http, &format!("{prefix}{path}"), role.as_deref()) {
            Ok(tokens) => registrations.push(tokens),
            Err(e) => return e.to_compile_error().into(),
        }
    }

    let expanded = quote! {
        #item_impl

        impl ::kernway_server::Controller for #self_ty {
            fn register(self: ::std::sync::Arc<Self>, app: ::kernway_server::AppBuilder) -> ::kernway_server::AppBuilder {
                let mut __app = app;
                #(#registrations)*
                __app
            }
        }
    };
    expanded.into()
}

/// One route method → a `.get/.post/…(path, handler)` on the builder. The role
/// guard and each typed argument ([`Extract`]) are resolved **synchronously** (they
/// borrow the request and scope) before the `'static` handler future, which then
/// owns only the extracted values and the `Arc<Self>`. A `Request` parameter is the
/// raw request, passed by move.
fn route_registration(
    method: &syn::Ident,
    inputs: &Punctuated<FnArg, Comma>,
    http: &str,
    full_path: &str,
    role: Option<&str>,
) -> Result<TokenStream2, syn::Error> {
    let builder = match http.to_ascii_uppercase().as_str() {
        "GET" => quote! { get },
        "POST" => quote! { post },
        "PUT" => quote! { put },
        "DELETE" => quote! { delete },
        "PATCH" => quote! { patch },
        other => {
            return Err(syn::Error::new(
                method.span(),
                format!("unknown HTTP method `{other}` in #[route] (expected GET/POST/PUT/DELETE/PATCH)"),
            ));
        }
    };

    // Walk the method parameters (skipping `&self`): each is either the raw
    // `Request` (moved) or a typed `Extract` (resolved synchronously, short-
    // circuiting to its error response on failure).
    let mut extractions = Vec::new();
    let mut call_args = Vec::new();
    let mut has_request = false;
    for arg in inputs {
        let FnArg::Typed(pt) = arg else { continue };
        let Pat::Ident(pat) = &*pt.pat else {
            return Err(syn::Error::new_spanned(&pt.pat, "#[route] method parameters must be simple names"));
        };
        let name = &pat.ident;
        let ty = &pt.ty;
        if is_request_type(ty) {
            has_request = true;
            call_args.push(quote! { req });
        } else {
            extractions.push(quote! {
                let #name = match <#ty as ::kernway_server::Extract>::extract(&req, scope, ::std::stringify!(#name)) {
                    ::std::result::Result::Ok(__value) => __value,
                    ::std::result::Result::Err(__response) => {
                        return ::std::boxed::Box::pin(async move { __response });
                    }
                };
            });
            call_args.push(quote! { #name });
        }
    }

    // Only reference the closure params that are actually used, so the generated
    // code has no "unused variable" warnings.
    let req_used = has_request || !extractions.is_empty();
    let scope_used = role.is_some() || !extractions.is_empty();
    let req_param = if req_used { quote! { req } } else { quote! { _req } };
    let scope_param = if scope_used { quote! { scope } } else { quote! { _scope } };
    let guard = match role {
        Some(role) => quote! {
            if !::kernway_server::role_allowed(scope, #role) {
                return ::std::boxed::Box::pin(async move { ::kernway_server::forbidden() });
            }
        },
        None => quote! {},
    };

    Ok(quote! {
        __app = {
            let __this = ::std::sync::Arc::clone(&self);
            __app.#builder(
                #full_path,
                move |#req_param: ::kernway_server::Request, #scope_param: &::kernway_server::RequestScope|
                    -> ::kernway_server::BoxFuture<'static, ::kernway_server::Response>
                {
                    #guard
                    #(#extractions)*
                    let __this = ::std::sync::Arc::clone(&__this);
                    ::std::boxed::Box::pin(async move { __this.#method(#(#call_args),*).await })
                },
            )
        };
    })
}

/// Whether a parameter type is the raw `Request` (passed by move, not extracted).
fn is_request_type(ty: &Type) -> bool {
    matches!(ty, Type::Path(tp) if tp.path.segments.last().is_some_and(|s| s.ident == "Request"))
}

/// Read `#[route(METHOD, "/path")]` → `(method, path)`.
fn extract_route(attrs: &[Attribute]) -> Option<(String, String)> {
    let attr = attrs.iter().find(|a| a.path().is_ident("route"))?;
    let args = attr.parse_args_with(Punctuated::<Expr, Comma>::parse_terminated).ok()?;
    let mut it = args.iter();
    let method = match it.next()? {
        Expr::Path(p) => p.path.segments.last()?.ident.to_string(),
        _ => return None,
    };
    let path = match it.next()? {
        Expr::Lit(syn::ExprLit { lit: Lit::Str(s), .. }) => s.value(),
        _ => return None,
    };
    Some((method, path))
}

/// Read `#[require_role("ROLE")]` → the role name.
fn extract_role(attrs: &[Attribute]) -> Option<String> {
    let attr = attrs.iter().find(|a| a.path().is_ident("require_role"))?;
    attr.parse_args::<LitStr>().ok().map(|lit| lit.value())
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
///
/// **Custom message** — any built-in rule takes `message = "…"` to replace its
/// default: `#[validate(email(message = "please enter a valid email"))]`,
/// `#[validate(length(min = 3, message = "too short"))]`.
///
/// **Custom validator** — `custom = my_fn` calls `my_fn(&self.field) -> Result<(),
/// String>` (its own message on failure): `#[validate(custom = validate_username)]`.
///
/// For anything the derive does not cover (cross-field checks, a bespoke error
/// shape), hand-write `impl Validate` — the trait, `rules`, and `ValidationErrors`
/// are all public, so the derive is never the only path.
#[proc_macro_derive(Validate, attributes(validate))]
pub fn derive_validate(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let fields = match &input.data {
        Data::Struct(DataStruct { fields: Fields::Named(FieldsNamed { named, .. }), .. }) => named,
        _ => {
            return syn::Error::new_spanned(name, "#[derive(Validate)] requires a struct with named fields")
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
                let check = match &meta {
                    // A bare rule: `not_blank`, `email` — default message.
                    Meta::Path(path) => {
                        builtin_check(fname, &fstr, path.segments.last().map(|s| &s.ident), None, None, None)
                    }
                    // A rule with args: `length(min = .., max = .., message = "..")`,
                    // `range(...)`, or a message-only override `email(message = "..")`.
                    Meta::List(list) => {
                        let (min, max, message) = parse_rule_args(list);
                        builtin_check(fname, &fstr, list.path.segments.last().map(|s| &s.ident), min, max, message)
                    }
                    // A user validator: `custom = my_fn` — calls `my_fn(&self.field)`,
                    // which returns `Result<(), String>` (its own message on failure).
                    Meta::NameValue(nv) if nv.path.is_ident("custom") => {
                        let func = &nv.value;
                        Ok(quote! {
                            if let ::std::result::Result::Err(__m) = #func(&self.#fname) {
                                __errors.push(#fstr, __m);
                            }
                        })
                    }
                    other => Err(syn::Error::new_spanned(other, "unexpected `#[validate(...)]` entry")),
                };
                match check {
                    Ok(tokens) => checks.push(tokens),
                    Err(e) => return e.to_compile_error().into(),
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

/// Pull `min`/`max`/`message` expressions out of a `length(...)` / `range(...)` /
/// `email(message = "..")` attribute list; any may be absent.
fn parse_rule_args(list: &MetaList) -> (Option<syn::Expr>, Option<syn::Expr>, Option<syn::Expr>) {
    let (mut min, mut max, mut message) = (None, None, None);
    if let Ok(pairs) = list.parse_args_with(Punctuated::<syn::MetaNameValue, Comma>::parse_terminated) {
        for pair in pairs {
            if pair.path.is_ident("min") {
                min = Some(pair.value);
            } else if pair.path.is_ident("max") {
                max = Some(pair.value);
            } else if pair.path.is_ident("message") {
                message = Some(pair.value);
            }
        }
    }
    (min, max, message)
}

/// `Some(#e)` or `None` tokens for an optional bound.
fn opt_tokens(expr: Option<syn::Expr>) -> TokenStream2 {
    expr.map_or_else(|| quote! { None }, |e| quote! { Some(#e) })
}

/// The check for one built-in rule (`not_blank`/`email`/`length`/`range`) on a
/// field, with an optional `message` override replacing the rule's default.
fn builtin_check(
    fname: &syn::Ident,
    fstr: &str,
    rule: Option<&syn::Ident>,
    min: Option<syn::Expr>,
    max: Option<syn::Expr>,
    message: Option<syn::Expr>,
) -> Result<TokenStream2, syn::Error> {
    let Some(rule) = rule else {
        return Ok(quote! {});
    };
    let call = if rule == "not_blank" || rule == "email" {
        quote! { ::kernway_validation::rules::#rule(&self.#fname) }
    } else if rule == "length" {
        let (min, max) = (opt_tokens(min), opt_tokens(max));
        quote! { ::kernway_validation::rules::length(&self.#fname, #min, #max) }
    } else if rule == "range" {
        // range takes the numeric field by value.
        let (min, max) = (opt_tokens(min), opt_tokens(max));
        quote! { ::kernway_validation::rules::range(self.#fname, #min, #max) }
    } else {
        return Err(syn::Error::new_spanned(
            rule,
            format!("unknown validation rule `{rule}` — expected not_blank, email, length, range, or `custom = fn`"),
        ));
    };
    // With a `message`, ignore the rule's default and push the override.
    let push = match message {
        Some(message) => quote! {
            if #call.is_err() { __errors.push(#fstr, #message); }
        },
        None => quote! {
            if let ::std::result::Result::Err(__m) = #call { __errors.push(#fstr, __m); }
        },
    };
    Ok(push)
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
