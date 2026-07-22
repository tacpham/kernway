use proc_macro::TokenStream;
use quote::quote;
use syn::{
    parse_macro_input, punctuated::Punctuated, Attribute, Expr, ExprLit, Fields, ItemStruct, Lit,
    Meta, MetaNameValue, Token, Type,
};

#[proc_macro_attribute]
pub fn entity(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args with Punctuated::<Meta, Token![,]>::parse_terminated);
    let mut item = parse_macro_input!(input as ItemStruct);
    let struct_name = item.ident.clone();
    let table_name = extract_table_name(&args).unwrap_or_else(|| to_snake_case(&struct_name.to_string()));

    let fields = match &mut item.fields {
        Fields::Named(fields) => &mut fields.named,
        _ => {
            return syn::Error::new_spanned(&item, "#[entity] only supports structs with named fields")
                .to_compile_error()
                .into();
        }
    };

    let mut id_field = None;
    let mut id_type = None;
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
            if id_field.is_some() {
                return syn::Error::new_spanned(&field_ident, "#[entity] requires exactly one #[id] field")
                    .to_compile_error()
                    .into();
            }
            id_field = Some(field_ident.clone());
            id_type = Some(field_ty.clone());
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

    let id_field = match id_field {
        Some(field) => field,
        None => {
            return syn::Error::new_spanned(&item, "#[entity] requires exactly one #[id] field")
                .to_compile_error()
                .into();
        }
    };
    let id_type = id_type.expect("id type should exist when id field exists");

    let expanded = quote! {
        #item

        impl ::kernway_orm_core::Entity for #struct_name {
            type Id = #id_type;

            fn table_name() -> &'static str {
                #table_name
            }

            fn id(&self) -> &Self::Id {
                &self.#id_field
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
                if let Expr::Lit(ExprLit { lit: Lit::Str(lit), .. }) = value {
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
        Type::Path(type_path) => type_path.path.segments.last().map(|segment| segment.ident.to_string()),
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
