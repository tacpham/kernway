//! Derive and attribute macros for `kernway-orm-core`.
//!
//! These read a struct at compile time and emit the `Entity` impl the runtime
//! needs — table name, primary key accessor, and column metadata. Nothing here
//! reaches a database; the generated code only describes the mapping, and a
//! backend crate (`kernway-orm-sqlite`, `kernway-orm-memory`, ...) acts on it.
//!
//! The emitted code refers to `::kernway_orm_core::` paths only, so this crate
//! does **not** depend on `kernway-orm-core` itself — that is what lets the ORM
//! subsystem be used without the rest of the framework.

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    parse_macro_input, punctuated::Punctuated, Attribute, Expr, ExprLit, FnArg, Fields,
    GenericArgument, Ident, ItemStruct, ItemTrait, Lit, Meta, MetaNameValue, Pat, PathArguments,
    ReturnType, Signature, Token, TraitItem, Type,
};

/// Maps a struct onto a database table — the JPA `@Entity` + `@Table` equivalent.
///
/// # Arguments
/// - `table = "name"` — table name; defaults to the struct name in snake_case.
///
/// # Field attributes
/// - `#[id]` — the primary key. Exactly one field must carry it.
/// - `#[column(name = "...")]` — override the column name.
///
/// # Generates
/// `impl Entity for TheStruct`, supplying `table_name`, `id`, and `columns`.
///
/// # Example
/// ```rust,ignore
/// #[entity(table = "users")]
/// pub struct User {
///     #[id] pub id: i64,
///     #[column] pub email: String,
/// }
/// ```
#[proc_macro_attribute]
pub fn entity(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args with Punctuated::<Meta, Token![,]>::parse_terminated);
    let mut item = parse_macro_input!(input as ItemStruct);
    let struct_name = item.ident.clone();
    let table_name =
        extract_table_name(&args).unwrap_or_else(|| to_snake_case(&struct_name.to_string()));

    let fields = match &mut item.fields {
        Fields::Named(fields) => &mut fields.named,
        _ => {
            return syn::Error::new_spanned(
                &item,
                "#[entity] only supports structs with named fields",
            )
            .to_compile_error()
            .into();
        }
    };

    let mut id_fields = Vec::new();
    let mut id_types = Vec::new();
    let mut columns = Vec::new();

    for field in fields.iter_mut() {
        let field_ident = match &field.ident {
            Some(ident) => ident.clone(),
            None => {
                return syn::Error::new_spanned(field, "#[entity] requires named fields")
                    .to_compile_error()
                    .into();
            }
        };
        let field_ty = field.ty.clone();
        let field_name = field_ident.to_string();
        let mut column_name = field_name.clone();
        let mut nullable = is_option_type(&field_ty);
        let mut unique = false;
        let mut auto = false;
        let mut is_id = false;
        let mut kept_attrs = Vec::new();

        for attr in std::mem::take(&mut field.attrs) {
            if attr.path().is_ident("id") {
                if is_id {
                    return syn::Error::new_spanned(attr, "duplicate #[id] attribute")
                        .to_compile_error()
                        .into();
                }
                is_id = true;
                match parse_id_attr(&attr) {
                    Ok(parsed_auto) => auto = parsed_auto,
                    Err(err) => return err.to_compile_error().into(),
                }
            } else if attr.path().is_ident("column") {
                match parse_column_attr(&attr, &mut column_name, &mut nullable, &mut unique) {
                    Ok(()) => {}
                    Err(err) => return err.to_compile_error().into(),
                }
            } else {
                kept_attrs.push(attr);
            }
        }

        field.attrs = kept_attrs;

        if is_id {
            // Multiple #[id] fields are allowed — they form a composite key.
            id_fields.push(field_ident.clone());
            id_types.push(field_ty.clone());
        }

        let col_type = column_type_tokens(&field_ty);
        let primary_key = is_id;
        let unique_flag = unique || primary_key;

        columns.push(quote! {
            ::kernway_orm_core::ColumnDef {
                name: #column_name,
                field: #field_name,
                col_type: #col_type,
                nullable: #nullable,
                primary_key: #primary_key,
                unique: #unique_flag,
                auto: #auto,
            }
        });
    }

    if id_fields.is_empty() {
        return syn::Error::new_spanned(&item, "#[entity] requires at least one #[id] field")
            .to_compile_error()
            .into();
    }
    // One #[id] → that field's type and value; several → a tuple of them, in
    // declaration order (a composite key). `Id: Clone` makes `id()` cheap.
    let (id_type, id_expr) = if id_fields.len() == 1 {
        let field = &id_fields[0];
        let ty = &id_types[0];
        (quote! { #ty }, quote! { self.#field.clone() })
    } else {
        (
            quote! { ( #(#id_types),* ) },
            quote! { ( #(self.#id_fields.clone()),* ) },
        )
    };

    let expanded = quote! {
        #item

        impl ::kernway_orm_core::Entity for #struct_name {
            type Id = #id_type;

            fn table_name() -> &'static str {
                #table_name
            }

            fn id(&self) -> Self::Id {
                #id_expr
            }

            fn columns() -> &'static [::kernway_orm_core::ColumnDef] {
                static COLS: ::std::sync::OnceLock<::std::vec::Vec<::kernway_orm_core::ColumnDef>> =
                    ::std::sync::OnceLock::new();
                COLS.get_or_init(|| vec![#(#columns),*]).as_slice()
            }
        }
    };

    expanded.into()
}

fn extract_table_name(args: &Punctuated<Meta, Token![,]>) -> Option<String> {
    for arg in args {
        if let Meta::NameValue(MetaNameValue { path, value, .. }) = arg {
            if path.is_ident("table") {
                if let Expr::Lit(ExprLit {
                    lit: Lit::Str(lit), ..
                }) = value
                {
                    return Some(lit.value());
                }
            }
        }
    }
    None
}

fn parse_id_attr(attr: &Attribute) -> syn::Result<bool> {
    let mut auto = false;
    attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("strategy") {
            let value = meta.value()?.parse::<syn::LitStr>()?;
            if value.value() == "auto" {
                auto = true;
            }
            return Ok(());
        }
        Err(meta.error("unsupported #[id] option"))
    })?;
    Ok(auto)
}

fn parse_column_attr(
    attr: &Attribute,
    column_name: &mut String,
    nullable: &mut bool,
    unique: &mut bool,
) -> syn::Result<()> {
    attr.parse_nested_meta(|meta| {
        if meta.path.is_ident("name") {
            *column_name = meta.value()?.parse::<syn::LitStr>()?.value();
            return Ok(());
        }
        if meta.path.is_ident("nullable") {
            *nullable = meta.value()?.parse::<syn::LitBool>()?.value();
            return Ok(());
        }
        if meta.path.is_ident("unique") {
            *unique = true;
            return Ok(());
        }
        Err(meta.error("unsupported #[column] option"))
    })
}

fn is_option_type(ty: &Type) -> bool {
    match ty {
        Type::Path(type_path) => type_path
            .path
            .segments
            .last()
            .map(|segment| segment.ident == "Option")
            .unwrap_or(false),
        _ => false,
    }
}

fn column_type_tokens(ty: &Type) -> proc_macro2::TokenStream {
    match rust_type_name(ty).as_deref() {
        Some("i64") | Some("u64") => quote! { ::kernway_orm_core::ColumnType::BigInt },
        Some("i32") | Some("u32") | Some("i16") | Some("i8") | Some("u8") => {
            quote! { ::kernway_orm_core::ColumnType::Integer }
        }
        Some("String") | Some("str") => quote! { ::kernway_orm_core::ColumnType::Text },
        Some("bool") => quote! { ::kernway_orm_core::ColumnType::Boolean },
        Some("f32") | Some("f64") => quote! { ::kernway_orm_core::ColumnType::Float },
        _ => quote! { ::kernway_orm_core::ColumnType::Unknown },
    }
}

fn rust_type_name(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(type_path) => type_path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string()),
        Type::Reference(reference) => rust_type_name(&reference.elem),
        _ => None,
    }
}

fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            result.push('_');
        }
        result.push(ch.to_lowercase().next().unwrap());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::to_snake_case;

    #[test]
    fn snake_case_conversion() {
        assert_eq!(to_snake_case("MyStruct"), "my_struct");
        assert_eq!(to_snake_case("Todo"), "todo");
    }
}

/// Derives a repository from method names — the Spring Data "derived query"
/// equivalent.
///
/// `#[repository(entity = User)]` on a trait of `async fn` methods generates a
/// `<Trait>Impl` struct wrapping a `Box<dyn Repository<User>>` and implements each
/// method by building a query from its name.
///
/// Grammar (snake_case): a prefix `find_by_` / `find_all_by_` (return `Vec<T>`, or
/// `Option<T>` for a single result) / `count_by_` (`u64`) / `exists_by_` (`bool`),
/// then conditions joined by `_and_`; each condition is `field` (equality) or
/// `field_<op>` with `op` in `ne` `gt` `lt` `gte` `lte` `like`. Parameters bind to
/// the conditions left to right.
///
/// ```ignore
/// #[repository(entity = User)]
/// #[allow(async_fn_in_trait)]
/// trait UserRepo {
///     async fn find_by_email(&self, email: &str) -> Result<Option<User>, OrmError>;
///     async fn find_by_role_and_age_gt(&self, role: &str, age: i64) -> Result<Vec<User>, OrmError>;
///     async fn count_by_role(&self, role: &str) -> Result<u64, OrmError>;
///     async fn exists_by_email(&self, email: &str) -> Result<bool, OrmError>;
/// }
/// ```
#[proc_macro_attribute]
pub fn repository(args: TokenStream, input: TokenStream) -> TokenStream {
    let metas = parse_macro_input!(args with Punctuated::<Meta, Token![,]>::parse_terminated);
    let entity = match metas.iter().find_map(|m| match m {
        Meta::NameValue(nv) if nv.path.is_ident("entity") => Some(nv.value.clone()),
        _ => None,
    }) {
        Some(e) => e,
        None => {
            return syn::Error::new(proc_macro2::Span::call_site(), "#[repository] requires `entity = TypeName`")
                .to_compile_error()
                .into();
        }
    };

    let item_trait = parse_macro_input!(input as ItemTrait);
    let trait_name = item_trait.ident.clone();
    let impl_name = Ident::new(&format!("{}Impl", trait_name), trait_name.span());

    let mut methods = Vec::new();
    for item in &item_trait.items {
        if let TraitItem::Fn(m) = item {
            match build_derived_method(&m.sig) {
                Ok(ts) => methods.push(ts),
                Err(err) => return err.to_compile_error().into(),
            }
        }
    }

    quote! {
        #item_trait

        /// Generated repository implementation (see the `#[repository]` trait).
        pub struct #impl_name {
            repository: ::std::boxed::Box<dyn ::kernway_orm_core::repository::Repository<#entity>>,
        }

        impl #impl_name {
            /// Wrap a repository obtained from a driver.
            pub fn new(
                repository: ::std::boxed::Box<dyn ::kernway_orm_core::repository::Repository<#entity>>,
            ) -> Self {
                Self { repository }
            }
        }

        impl #trait_name for #impl_name {
            #(#methods)*
        }
    }
    .into()
}

/// Build one generated method body from its signature (name-derived query).
fn build_derived_method(sig: &Signature) -> syn::Result<proc_macro2::TokenStream> {
    let name = sig.ident.to_string();

    #[derive(Clone, Copy)]
    enum Kind {
        Find,
        Count,
        Exists,
    }
    let (kind, rest) = if let Some(r) = name.strip_prefix("find_all_by_") {
        (Kind::Find, r)
    } else if let Some(r) = name.strip_prefix("find_by_") {
        (Kind::Find, r)
    } else if let Some(r) = name.strip_prefix("count_by_") {
        (Kind::Count, r)
    } else if let Some(r) = name.strip_prefix("exists_by_") {
        (Kind::Exists, r)
    } else {
        return Err(syn::Error::new(
            sig.ident.span(),
            "#[repository] method must start with find_by_ / find_all_by_ / count_by_ / exists_by_",
        ));
    };

    // Conditions, AND-joined.
    let conditions: Vec<(String, &'static str)> = rest.split("_and_").map(parse_condition).collect();

    // Parameter idents (skip the receiver), left to right.
    let params: Vec<&Ident> = sig
        .inputs
        .iter()
        .filter_map(|arg| match arg {
            FnArg::Typed(pt) => match &*pt.pat {
                Pat::Ident(pi) => Some(&pi.ident),
                _ => None,
            },
            FnArg::Receiver(_) => None,
        })
        .collect();

    if params.len() != conditions.len() {
        return Err(syn::Error::new(
            sig.ident.span(),
            format!(
                "derived query has {} condition(s) but {} parameter(s)",
                conditions.len(),
                params.len()
            ),
        ));
    }

    let mut chain = quote! { self.repository.query() };
    for ((field, op), param) in conditions.iter().zip(&params) {
        let method = Ident::new(&format!("filter_{}", op), sig.ident.span());
        chain = quote! { #chain.#method(#field, &#param.to_string()) };
    }

    let body = match kind {
        Kind::Find if returns_option(&sig.output) => quote! { #chain.fetch_one().await },
        Kind::Find => quote! { #chain.fetch_all().await },
        Kind::Count => quote! { #chain.fetch_count().await },
        Kind::Exists => quote! { ::core::result::Result::Ok(#chain.fetch_count().await? > 0) },
    };

    Ok(quote! { #sig { #body } })
}

/// Split a condition into `(field, operator)`; a trailing `_gt`/`_lt`/… is the
/// operator and the rest is the (possibly multi-word) field.
fn parse_condition(cond: &str) -> (String, &'static str) {
    const OPS: [(&str, &str); 6] = [
        ("_gte", "gte"),
        ("_lte", "lte"),
        ("_gt", "gt"),
        ("_lt", "lt"),
        ("_ne", "ne"),
        ("_like", "like"),
    ];
    for (suffix, op) in OPS {
        if let Some(field) = cond.strip_suffix(suffix) {
            return (field.to_string(), op);
        }
    }
    (cond.to_string(), "eq")
}

/// Whether a return type is `Result<Option<_>, _>` (→ `fetch_one`).
fn returns_option(output: &ReturnType) -> bool {
    let ReturnType::Type(_, ty) = output else {
        return false;
    };
    let Type::Path(tp) = &**ty else { return false };
    let Some(seg) = tp.path.segments.last() else {
        return false;
    };
    if seg.ident != "Result" {
        return false;
    }
    let PathArguments::AngleBracketed(ab) = &seg.arguments else {
        return false;
    };
    matches!(
        ab.args.first(),
        Some(GenericArgument::Type(Type::Path(ok)))
            if ok.path.segments.last().is_some_and(|s| s.ident == "Option")
    )
}
