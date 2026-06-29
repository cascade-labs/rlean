use anyhow::{Context, Result};
use proc_macro2::TokenStream;
use quote::{format_ident, quote, ToTokens};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use syn::{
    parse_file, Attribute, Expr, FnArg, ImplItem, ImplItemFn, Item, ItemImpl, Lit, LitStr, Meta,
    Pat, PatIdent, ReturnType, Type,
};

#[derive(Debug, Clone)]
pub struct BindClass {
    pub sdk_type: String,
    pub py_type: String,
    pub py_name: String,
    pub module_path: String,
    pub callback_adapter: bool,
    pub subclass: bool,
    pub constructor: Option<BindConstructor>,
    pub constructor_style: ConstructorStyle,
    pub wrapped_type: Option<String>,
    pub wrap_constructor: Option<String>,
    pub protocols: BindProtocols,
    pub mutable: bool,
    pub cloneable: bool,
    pub getters: Vec<BindGetter>,
    pub methods: Vec<BindMethod>,
    pub statics: Vec<BindStaticMethod>,
    pub setters: Vec<BindSetter>,
}

#[derive(Debug, Clone)]
pub struct BindEnum {
    pub sdk_type: String,
    pub py_type: String,
    pub py_name: String,
    pub rust_type: Option<String>,
    pub reverse: bool,
    pub variants: Vec<BindEnumVariant>,
}

#[derive(Debug, Clone)]
pub struct BindEnumVariant {
    pub name: String,
    pub value: i64,
}

#[derive(Debug, Clone)]
pub struct BindConstructor {
    pub sdk_method: String,
    pub args: Vec<BindArg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstructorStyle {
    Normal,
    Variadic,
}

impl Default for ConstructorStyle {
    fn default() -> Self {
        Self::Normal
    }
}

#[derive(Debug, Clone, Default)]
pub struct BindProtocols {
    pub str_method: Option<String>,
    pub repr_method: Option<String>,
    pub hash_method: Option<String>,
    pub richcmp_method: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BindGetter {
    pub sdk_method: String,
    pub py_name: String,
    pub aliases: Vec<String>,
    pub return_type: String,
}

#[derive(Debug, Clone)]
pub struct BindMethod {
    pub sdk_method: String,
    pub py_name: String,
    pub aliases: Vec<String>,
    pub args: Vec<BindArg>,
    pub return_type: String,
    pub mutates: bool,
}

#[derive(Debug, Clone)]
pub struct BindStaticMethod {
    pub sdk_method: String,
    pub py_name: String,
    pub aliases: Vec<String>,
    pub args: Vec<BindArg>,
    pub return_type: String,
}

#[derive(Debug, Clone)]
pub struct BindSetter {
    pub sdk_method: String,
    pub property: String,
    pub args: Vec<BindArg>,
}

#[derive(Debug, Clone)]
pub struct BindArg {
    pub name: String,
    pub ty: String,
}

pub fn build_pyo3_bindings() -> Result<()> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let workspace_crates =
        if manifest_dir.file_name().and_then(|name| name.to_str()) == Some("lean-python") {
            manifest_dir
                .parent()
                .context("lean-python should live below crates/lean-python")?
                .to_path_buf()
        } else {
            manifest_dir
                .parent()
                .and_then(|path| path.parent())
                .context("lean-sdk/python should live below crates/lean-sdk/python")?
                .to_path_buf()
        };
    let sdk_src = workspace_crates.join("lean-sdk/src");
    let python_src = manifest_dir.join("src");
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);

    println!("cargo:rerun-if-changed={}", sdk_src.display());
    println!("cargo:rerun-if-changed={}", python_src.display());

    let mut classes = Vec::new();
    let mut enums = Vec::new();
    for entry in fs::read_dir(&sdk_src)
        .with_context(|| format!("failed to read SDK source dir {}", sdk_src.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        println!("cargo:rerun-if-changed={}", path.display());
        let module_path = format!("lean_sdk::{stem}");
        let parsed = parse_sdk_file(&path, &module_path)?;
        classes.extend(parsed.classes);
        enums.extend(parsed.enums);
    }

    classes.sort_by(|a, b| {
        a.module_path
            .cmp(&b.module_path)
            .then_with(|| a.py_type.cmp(&b.py_type))
    });
    let mut seen_class_types = std::collections::HashSet::new();
    classes.retain(|class| seen_class_types.insert(class.py_type.clone()));

    enums.sort_by(|a, b| a.py_type.cmp(&b.py_type));
    let mut seen_enum_types = std::collections::HashSet::new();
    enums.retain(|bind_enum| seen_enum_types.insert(bind_enum.py_type.clone()));

    let generated = generate_pyo3_module(&classes, &enums)?;
    fs::write(out_dir.join("sdk_bindings.rs"), generated)?;
    fs::write(
        out_dir.join("python_lib.rs"),
        generate_python_lib(&python_src, &classes, &enums)?,
    )?;
    Ok(())
}

pub fn generate_algorithm_imports_pyi(workspace_root: impl AsRef<Path>) -> Result<String> {
    let workspace_root = workspace_root.as_ref();
    let sdk_src = workspace_root.join("crates/lean-sdk/src");
    let mut classes = Vec::new();
    let mut enums = Vec::new();

    for entry in fs::read_dir(&sdk_src)
        .with_context(|| format!("failed to read SDK source dir {}", sdk_src.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let module_path = format!("lean_sdk::{stem}");
        let parsed = parse_sdk_file(&path, &module_path)?;
        classes.extend(parsed.classes);
        enums.extend(parsed.enums);
    }

    classes.sort_by(|a, b| a.py_name.cmp(&b.py_name));
    let mut seen_class_names = std::collections::HashSet::new();
    classes.retain(|class| seen_class_names.insert(class.py_name.clone()));
    enums.sort_by(|a, b| a.py_name.cmp(&b.py_name));
    let mut seen_enum_names = std::collections::HashSet::new();
    enums.retain(|bind_enum| seen_enum_names.insert(bind_enum.py_name.clone()));

    Ok(render_algorithm_imports_pyi(&classes, &enums))
}

fn render_algorithm_imports_pyi(classes: &[BindClass], enums: &[BindEnum]) -> String {
    let mut out = String::from(
        "# AlgorithmImports.pyi\n\
         # Auto-generated by `rlean stubs create` from lean-sdk annotations.\n\
         # Do not edit by hand.\n\
         from __future__ import annotations\n\
         from datetime import date, datetime, timedelta\n\
         from typing import Any, Optional\n\n",
    );

    for bind_enum in enums {
        out.push_str(&format!("class {}:\n", bind_enum.py_name));
        if bind_enum.variants.is_empty() {
            out.push_str("    pass\n\n");
            continue;
        }
        for variant in &bind_enum.variants {
            out.push_str(&format!("    {}: {}\n", variant.name, bind_enum.py_name));
            let alias = screaming_snake_string(&variant.name);
            if alias != variant.name {
                out.push_str(&format!("    {}: {}\n", alias, bind_enum.py_name));
            }
        }
        out.push('\n');
    }

    for class in classes {
        out.push_str(&format!("class {}:\n", class.py_name));
        let mut wrote = false;
        if let Some(constructor) = &class.constructor {
            out.push_str(&format!(
                "    def __init__(self{}) -> None: ...\n",
                pyi_args(&constructor.args)
            ));
            wrote = true;
        }
        for getter in &class.getters {
            out.push_str("    @property\n");
            out.push_str(&format!(
                "    def {}(self) -> {}: ...\n",
                getter.py_name,
                pyi_type(&getter.return_type)
            ));
            wrote = true;
        }
        for method in &class.methods {
            out.push_str(&format!(
                "    def {}(self{}) -> {}: ...\n",
                method.py_name,
                pyi_args(&method.args),
                pyi_type(&method.return_type)
            ));
            wrote = true;
        }
        for method in &class.statics {
            out.push_str("    @staticmethod\n");
            out.push_str(&format!(
                "    def {}({}) -> {}: ...\n",
                method.py_name,
                pyi_args_without_self(&method.args),
                pyi_type(&method.return_type)
            ));
            wrote = true;
        }
        if !wrote {
            out.push_str("    pass\n");
        }
        out.push('\n');
    }

    out
}

fn pyi_args(args: &[BindArg]) -> String {
    if args.is_empty() {
        String::new()
    } else {
        format!(", {}", pyi_args_without_self(args))
    }
}

fn pyi_args_without_self(args: &[BindArg]) -> String {
    args.iter()
        .map(|arg| format!("{}: {}", arg.name, pyi_type(&arg.ty)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn pyi_type(ty: &str) -> String {
    let normalized = ty.replace(' ', "");

    if let Some(inner) = normalized
        .strip_prefix("Option<")
        .and_then(|rest| rest.strip_suffix('>'))
    {
        return format!("Optional[{}]", pyi_type(inner));
    }
    if let Some(inner) = normalized
        .strip_prefix("Vec<")
        .and_then(|rest| rest.strip_suffix('>'))
    {
        return format!("list[{}]", pyi_type(inner));
    }
    if let Some(inner) = normalized
        .strip_prefix("HashMap<")
        .and_then(|rest| rest.strip_suffix('>'))
    {
        if let Some((key, value)) = split_top_level_comma(inner) {
            return format!("dict[{}, {}]", pyi_type(key), pyi_type(value));
        }
    }

    let leaf = normalized
        .trim_start_matches('&')
        .rsplit("::")
        .next()
        .unwrap_or(&normalized);

    match leaf {
        "()" => "None".to_string(),
        "bool" => "bool".to_string(),
        "f64" | "f32" | "Price" | "Decimal" => "float".to_string(),
        "usize" | "u64" | "u32" | "u16" | "u8" | "i64" | "i32" | "i16" | "i8" => "int".to_string(),
        "String" | "str" => "str".to_string(),
        "NaiveDate" => "date".to_string(),
        "NaiveDateTime" | "DateTime" => "datetime".to_string(),
        "Self" => "Self".to_string(),
        other => other
            .strip_suffix("View")
            .or_else(|| other.strip_suffix("Handle"))
            .unwrap_or(other)
            .to_string(),
    }
}

fn split_top_level_comma(value: &str) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    for (idx, ch) in value.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => return Some((&value[..idx], &value[idx + 1..])),
            _ => {}
        }
    }
    None
}

fn screaming_snake_string(name: &str) -> String {
    let mut out = String::new();
    for (idx, ch) in name.chars().enumerate() {
        if ch.is_uppercase() && idx > 0 {
            out.push('_');
        }
        out.push(ch.to_ascii_uppercase());
    }
    out
}

fn generate_python_lib(
    python_src: &Path,
    classes: &[BindClass],
    enums: &[BindEnum],
) -> Result<String> {
    let mut module_names = Vec::new();
    for entry in fs::read_dir(python_src)
        .with_context(|| format!("failed to read Python source dir {}", python_src.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if stem == "lib" || stem == "generated_bindings" {
            continue;
        }
        module_names.push(stem.to_string());
    }
    module_names.sort();

    let module_tokens = module_names
        .iter()
        .map(|module| {
            let ident = format_ident!("{}", module);
            let path = python_src
                .join(format!("{module}.rs"))
                .display()
                .to_string();
            quote! {
                #[path = #path]
                pub mod #ident;
            }
        })
        .collect::<Vec<_>>();
    let generated_reexports = classes
        .iter()
        .map(|class| format_ident!("{}", class.py_type))
        .chain(
            enums
                .iter()
                .map(|bind_enum| format_ident!("{}", bind_enum.py_type)),
        )
        .collect::<Vec<_>>();

    let tokens = quote! {
        pub mod sdk_bindings {
            include!(concat!(env!("OUT_DIR"), "/sdk_bindings.rs"));
        }
        #(#module_tokens)*

        use pyo3::prelude::*;

        pub use sdk_bindings::{#(#generated_reexports),*};


        #[pymodule]
        #[pyo3(name = "AlgorithmImports")]
        pub fn algorithm_imports(m: &Bound<'_, PyModule>) -> PyResult<()> {
            sdk_bindings::register_generated_bindings(m)?;
            Ok(())
        }

        pub use algorithm_imports as AlgorithmImports;
        #[cfg(test)]
        pub(crate) mod test_python {
            use super::AlgorithmImports;
            use std::sync::Once;

            static INIT: Once = Once::new();

            pub(crate) fn init() {
                INIT.call_once(|| {
                    pyo3::append_to_inittab!(AlgorithmImports);
                    pyo3::Python::initialize();
                });
            }
        }
    };

    Ok(tokens.to_string())
}

#[derive(Debug, Clone, Default)]
pub struct ParsedSdkFile {
    pub classes: Vec<BindClass>,
    pub enums: Vec<BindEnum>,
}

pub fn parse_sdk_file(path: &Path, module_path: &str) -> Result<ParsedSdkFile> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read SDK source {}", path.display()))?;
    let file =
        parse_file(&source).with_context(|| format!("failed to parse {}", path.display()))?;

    let mut classes = Vec::new();
    let mut enums = Vec::new();
    for item in &file.items {
        match item {
            Item::Struct(item_struct) => {
                if let Some(bind) = sdk_bind_meta(&item_struct.attrs)? {
                    let sdk_type = item_struct.ident.to_string();
                    classes.push(BindClass {
                        py_type: format!("Py{}", bind.py_name),
                        py_name: bind.py_name,
                        sdk_type,
                        module_path: module_path.to_string(),
                        callback_adapter: has_struct_attr(
                            &item_struct.attrs,
                            "sdk_callback_adapter",
                        ),
                        subclass: bind.subclass,
                        constructor_style: bind.constructor_style,
                        wrapped_type: bind.wrapped_type,
                        wrap_constructor: bind.wrap_constructor,
                        protocols: bind.protocols,
                        mutable: false,
                        cloneable: derives_trait(&item_struct.attrs, "Clone"),
                        getters: Vec::new(),
                        methods: Vec::new(),
                        statics: Vec::new(),
                        constructor: None,
                        setters: Vec::new(),
                    });
                }
            }
            Item::Enum(item_enum) => {
                if let Some(bind) = sdk_bind_meta(&item_enum.attrs)? {
                    let variants = item_enum
                        .variants
                        .iter()
                        .map(|variant| {
                            let value = match &variant.discriminant {
                                Some((_, expr)) => literal_int_expr(expr)?,
                                None => anyhow::bail!(
                                    "sdk_bind enum {} variant {} must have integer discriminant",
                                    item_enum.ident,
                                    variant.ident
                                ),
                            };
                            Ok(BindEnumVariant {
                                name: variant.ident.to_string(),
                                value,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    enums.push(BindEnum {
                        sdk_type: item_enum.ident.to_string(),
                        py_type: format!("Py{}", bind.py_name),
                        py_name: bind.py_name,
                        rust_type: bind.rust_type,
                        reverse: bind.reverse,
                        variants,
                    });
                }
            }
            _ => {}
        }
    }

    for item in &file.items {
        let Item::Impl(item_impl) = item else {
            continue;
        };
        let Some(type_name) = impl_self_type(item_impl) else {
            continue;
        };
        let Some(class) = classes.iter_mut().find(|class| class.sdk_type == type_name) else {
            continue;
        };
        for impl_item in &item_impl.items {
            let ImplItem::Fn(method) = impl_item else {
                continue;
            };
            if has_attr(method, "sdk_new") {
                class.constructor = Some(BindConstructor {
                    sdk_method: method.sig.ident.to_string(),
                    args: method_args_static(method)?,
                });
            } else if let Some(meta) = sdk_static_meta(method)? {
                class.statics.push(BindStaticMethod {
                    sdk_method: method.sig.ident.to_string(),
                    py_name: meta
                        .py_name
                        .unwrap_or_else(|| method.sig.ident.to_string()),
                    aliases: meta.aliases,
                    args: method_args_static(method)?,
                    return_type: method_return_type_any_args(method)?,
                });
            } else if let Some(meta) = sdk_getter_meta(method)? {
                let return_type = method_return_type(method)?;
                class.getters.push(BindGetter {
                    sdk_method: method.sig.ident.to_string(),
                    py_name: meta
                        .py_name
                        .unwrap_or_else(|| method.sig.ident.to_string()),
                    aliases: meta.aliases,
                    return_type,
                });
            } else if let Some(meta) = sdk_method_meta(method)? {
                let mutates = method_takes_mut_self(method);
                if mutates {
                    class.mutable = true;
                }
                if let Some(setter_meta) = sdk_setter_meta(method)? {
                    class.setters.push(BindSetter {
                        sdk_method: method.sig.ident.to_string(),
                        property: setter_meta.property,
                        args: method_args(method)?,
                    });
                }
                class.methods.push(BindMethod {
                    sdk_method: method.sig.ident.to_string(),
                    py_name: meta
                        .py_name
                        .unwrap_or_else(|| method.sig.ident.to_string()),
                    aliases: meta.aliases,
                    args: method_args(method)?,
                    return_type: method_return_type_any_args(method)?,
                    mutates,
                });
            }
        }
    }

    Ok(ParsedSdkFile { classes, enums })
}

pub fn generate_pyo3_module(classes: &[BindClass], enums: &[BindEnum]) -> Result<String> {
    let mut tokens = TokenStream::new();
    tokens.extend(quote! {
        use pyo3::prelude::*;
    });

    for bind_enum in enums {
        tokens.extend(render_enum(bind_enum)?);
    }

    let type_registry = TypeRegistry::new(classes, enums);

    for class in classes {
        let sdk_path: TokenStream = class
            .module_path
            .parse()
            .map_err(|err| anyhow::anyhow!("invalid module path {}: {err}", class.module_path))?;
        let sdk_type = format_ident!("{}", class.sdk_type);
        let py_type = format_ident!("{}", class.py_type);
        let py_name = &class.py_name;

        if class.callback_adapter {
            tokens.extend(render_callback_adapter(class)?);
            continue;
        }

        let getter_tokens: Vec<TokenStream> = class
            .getters
            .iter()
            .filter(|getter| can_render_type(&getter.return_type, &type_registry))
            .flat_map(|getter| {
                std::iter::once(getter.py_name.clone())
                    .chain(getter.aliases.clone())
                    .map(move |py_name| (getter, py_name))
            })
            .map(|(getter, py_name)| render_getter_with_name(getter, &py_name, &type_registry))
            .collect::<Result<Vec<_>>>()?;
        let method_tokens: Vec<TokenStream> = class
            .methods
            .iter()
            .filter(|method| {
                can_render_signature(&method.args, &method.return_type, &type_registry)
            })
            .flat_map(|method| {
                std::iter::once(method.py_name.clone())
                    .chain(method.aliases.clone())
                    .map(move |py_name| (method, py_name))
            })
            .map(|(method, py_name)| {
                render_method_alias(method, &py_name, &class.py_type, &type_registry)
            })
            .collect::<Result<Vec<_>>>()?;
        let static_tokens: Vec<TokenStream> = class
            .statics
            .iter()
            .filter(|method| {
                can_render_signature(&method.args, &method.return_type, &type_registry)
            })
            .flat_map(|method| {
                std::iter::once(method.py_name.clone())
                    .chain(method.aliases.clone())
                    .map(move |py_name| (method, py_name))
            })
            .map(|(method, py_name)| {
                render_static_method_alias(
                    method,
                    &py_name,
                    &sdk_path,
                    &sdk_type,
                    &class.sdk_type,
                    &class.py_type,
                    &type_registry,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let constructor_tokens = match (&class.constructor, class.constructor_style) {
            (Some(constructor), ConstructorStyle::Variadic) => {
                render_subclass_constructor(constructor, &sdk_path, &sdk_type)?
            }
            (Some(constructor), ConstructorStyle::Normal)
                if can_render_args(&constructor.args, &type_registry) =>
            {
                render_constructor(constructor, &sdk_path, &sdk_type, &type_registry)?
            }
            (None, _) => TokenStream::new(),
            _ => TokenStream::new(),
        };
        let protocol_tokens = render_protocols(class, &type_registry)?;
        let extension_tokens = render_python_runtime_extensions(py_name);
        let setter_tokens: Vec<TokenStream> = class
            .setters
            .iter()
            .filter(|setter| can_render_args(&setter.args, &type_registry))
            .map(|setter| render_setter(setter, &type_registry))
            .collect::<Result<Vec<_>>>()?;
        let pyclass_attrs = if class.subclass {
            quote! { #[pyclass(name = #py_name, subclass)] }
        } else if class.mutable {
            quote! { #[pyclass(name = #py_name)] }
        } else {
            quote! { #[pyclass(name = #py_name, frozen)] }
        };
        let derive_clone = class.cloneable.then(|| quote! { #[derive(Clone)] });

        tokens.extend(quote! {
            #pyclass_attrs
            #derive_clone
            pub struct #py_type {
                pub(crate) inner: #sdk_path::#sdk_type,
            }

            impl #py_type {
                pub fn from_view(inner: #sdk_path::#sdk_type) -> Self {
                    Self { inner }
                }

                pub fn sdk(&self) -> &#sdk_path::#sdk_type {
                    &self.inner
                }
            }

            #[pymethods]
            impl #py_type {
                #constructor_tokens
                #(#getter_tokens)*
                #(#method_tokens)*
                #(#static_tokens)*
                #(#setter_tokens)*
                #protocol_tokens
                #extension_tokens
            }
        });
    }

    let class_idents = classes
        .iter()
        .map(|class| format_ident!("{}", class.py_type))
        .collect::<Vec<_>>();
    let enum_idents = enums
        .iter()
        .map(|bind_enum| format_ident!("{}", bind_enum.py_type))
        .collect::<Vec<_>>();

    tokens.extend(quote! {


        pub fn register_generated_bindings(m: &Bound<'_, PyModule>) -> PyResult<()> {
            #(m.add_class::<#enum_idents>()?;)*
            #(m.add_class::<#class_idents>()?;)*
            Ok(())
        }
    });

    Ok(tokens.to_string())
}

fn render_python_runtime_extensions(py_name: &str) -> TokenStream {
    if py_name != "QCAlgorithm" {
        return quote! {};
    }
    quote! {
        fn add_alpha(slf: ::pyo3::PyRef<'_, Self>, py: ::pyo3::Python<'_>, model: ::pyo3::Py<::pyo3::PyAny>) -> ::pyo3::PyResult<()> {
            crate::compat::qc_add_alpha(slf, py, model)
        }

        fn set_portfolio_construction(
            slf: ::pyo3::PyRef<'_, Self>,
            py: ::pyo3::Python<'_>,
            model: ::pyo3::Py<::pyo3::PyAny>,
        ) -> ::pyo3::PyResult<()> {
            crate::compat::qc_set_portfolio_construction(slf, py, model)
        }

        fn set_execution(&self, model: ::pyo3::Py<::pyo3::PyAny>) {
            crate::compat::qc_set_execution(self, model);
        }

        fn set_risk_management(&self, model: ::pyo3::Py<::pyo3::PyAny>) {
            crate::compat::qc_set_risk_management(self, model);
        }

        #[getter]
        fn insights(&self) -> crate::compat::PyInsightManagerCompat {
            crate::compat::qc_insights(self)
        }

        #[getter]
        fn securities(&self) -> crate::compat::PySecurityManagerCompat {
            crate::compat::qc_securities(self)
        }

        #[getter]
        fn settings(&self) -> crate::compat::PySettingsCompat {
            crate::compat::qc_settings(self)
        }
    }
}

fn render_constructor(
    constructor: &BindConstructor,
    sdk_path: &TokenStream,
    sdk_type: &proc_macro2::Ident,
    registry: &TypeRegistry,
) -> Result<TokenStream> {
    let sdk_method = format_ident!("{}", constructor.sdk_method);
    let args = render_args(&constructor.args, registry)?;
    let arg_names = constructor
        .args
        .iter()
        .map(|arg| sdk_arg_expr(arg, registry))
        .collect::<Vec<_>>();
    let signature_attr = signature_attr_for_args(&constructor.args);

    Ok(quote! {
        #[new]
        #signature_attr
        fn new(#(#args),*) -> Self {
            Self { inner: #sdk_path::#sdk_type::#sdk_method(#(#arg_names),*) }
        }
    })
}

fn render_subclass_constructor(
    constructor: &BindConstructor,
    sdk_path: &TokenStream,
    sdk_type: &proc_macro2::Ident,
) -> Result<TokenStream> {
    if !constructor.args.is_empty() {
        anyhow::bail!(
            "subclass-compatible constructor {} must not have SDK arguments",
            constructor.sdk_method
        );
    }
    let sdk_method = format_ident!("{}", constructor.sdk_method);
    Ok(quote! {
        #[new]
        #[pyo3(signature = (*_args, **_kwargs))]
        fn new(
            _args: &Bound<'_, pyo3::types::PyTuple>,
            _kwargs: Option<&Bound<'_, pyo3::types::PyDict>>,
        ) -> Self {
            Self { inner: #sdk_path::#sdk_type::#sdk_method() }
        }
    })
}

fn render_method_with_name(
    method: &BindMethod,
    py_name: &str,
    class_py_type: &str,
    registry: &TypeRegistry,
) -> Result<TokenStream> {
    let sdk_method = format_ident!("{}", method.sdk_method);
    let rust_method = if py_name == method.sdk_method {
        rust_ident(py_name)
    } else {
        rust_ident(&format!(
            "{}_alias_{}",
            method.sdk_method,
            safe_ident_suffix(py_name)
        ))
    };
    let pyo3_name = (py_name != method.sdk_method).then(|| quote! { #[pyo3(name = #py_name)] });
    let return_type = registry.py_return_type(&method.return_type)?;
    let args = render_args(&method.args, registry)?;
    let arg_names = method
        .args
        .iter()
        .map(|arg| sdk_arg_expr(arg, registry))
        .collect::<Vec<_>>();
    let call = quote! { self.inner.#sdk_method(#(#arg_names),*) };
    let body = if method.return_type == "Self" {
        let py_type = format_ident!("{}", class_py_type);
        quote! { #py_type::from_view(#call) }
    } else {
        method_return_expr(&call, &method.return_type, registry)
    };
    let receiver = if method.mutates {
        quote! { &mut self }
    } else {
        quote! { &self }
    };
    let signature_attr = signature_attr_for_args(&method.args);

    if method.py_name == "__iter__" {
        return Ok(quote! {
            #pyo3_name
            #signature_attr
            fn #rust_method<'py>(
                #receiver,
                py: pyo3::Python<'py>,
                #(#args),*
            ) -> pyo3::PyResult<pyo3::Bound<'py, pyo3::types::PyIterator>> {
                let items: #return_type = #body;
                let list = pyo3::types::PyList::new(py, items)?;
                pyo3::types::PyIterator::from_object(&list)
            }
        });
    }

    if method.return_type == "()" {
        Ok(quote! {
            #pyo3_name
            #signature_attr
            fn #rust_method(#receiver, #(#args),*) {
                #body;
            }
        })
    } else {
        Ok(quote! {
            #pyo3_name
            #signature_attr
            fn #rust_method(#receiver, #(#args),*) -> #return_type {
                #body
            }
        })
    }
}

fn render_method_alias(
    method: &BindMethod,
    alias: &str,
    class_py_type: &str,
    registry: &TypeRegistry,
) -> Result<TokenStream> {
    let mut alias_method = method.clone();
    alias_method.py_name = alias.to_string();
    render_method_with_name(&alias_method, alias, class_py_type, registry)
}

fn render_static_method_with_name(
    method: &BindStaticMethod,
    py_name: &str,
    sdk_path: &TokenStream,
    sdk_type: &proc_macro2::Ident,
    class_sdk_type: &str,
    class_py_type: &str,
    registry: &TypeRegistry,
) -> Result<TokenStream> {
    let sdk_method = format_ident!("{}", method.sdk_method);
    let rust_method = if py_name == method.sdk_method {
        rust_ident(py_name)
    } else {
        rust_ident(&format!(
            "{}_alias_{}",
            method.sdk_method,
            safe_ident_suffix(py_name)
        ))
    };
    let pyo3_name = (py_name != method.sdk_method).then(|| quote! { #[pyo3(name = #py_name)] });
    let return_type =
        py_return_type_for_class(&method.return_type, class_sdk_type, class_py_type, registry)?;
    let args = render_args(&method.args, registry)?;
    let arg_names = method
        .args
        .iter()
        .map(|arg| sdk_arg_expr(arg, registry))
        .collect::<Vec<_>>();
    let body = if method.return_type == class_sdk_type || method.return_type == "Self" {
        quote! { Self::from_view(#sdk_path::#sdk_type::#sdk_method(#(#arg_names),*)) }
    } else {
        quote! { #sdk_path::#sdk_type::#sdk_method(#(#arg_names),*) }
    };
    let signature_attr = signature_attr_for_args(&method.args);
    let fallible = method_args_need_py_result(&method.args);

    if method.return_type == "()" {
        Ok(quote! {
            #[staticmethod]
            #pyo3_name
            #signature_attr
            fn #rust_method(#(#args),*) {
                #body;
            }
        })
    } else if fallible {
        Ok(quote! {
            #[staticmethod]
            #pyo3_name
            #signature_attr
            fn #rust_method(#(#args),*) -> ::pyo3::PyResult<#return_type> {
                Ok(#body)
            }
        })
    } else {
        Ok(quote! {
            #[staticmethod]
            #pyo3_name
            #signature_attr
            fn #rust_method(#(#args),*) -> #return_type {
                #body
            }
        })
    }
}

fn render_static_method_alias(
    method: &BindStaticMethod,
    alias: &str,
    sdk_path: &TokenStream,
    sdk_type: &proc_macro2::Ident,
    class_sdk_type: &str,
    class_py_type: &str,
    registry: &TypeRegistry,
) -> Result<TokenStream> {
    render_static_method_with_name(
        method,
        alias,
        sdk_path,
        sdk_type,
        class_sdk_type,
        class_py_type,
        registry,
    )
}

fn signature_attr_for_args(args: &[BindArg]) -> TokenStream {
    if args.is_empty() {
        return TokenStream::new();
    }

    let mut seen_optional = false;
    for arg in args {
        let is_optional = option_inner(&normalize_type(&arg.ty)).is_some();
        if is_optional {
            seen_optional = true;
        } else if seen_optional {
            return TokenStream::new();
        }
    }
    if !seen_optional {
        return TokenStream::new();
    }

    let parts = args.iter().map(|arg| {
        let name = format_ident!("{}", arg.name);
        if option_inner(&normalize_type(&arg.ty)).is_some() {
            quote! { #name = None }
        } else {
            quote! { #name }
        }
    });
    quote! { #[pyo3(signature = (#(#parts),*))] }
}

fn render_callback_adapter(class: &BindClass) -> Result<TokenStream> {
    let py_type = format_ident!("{}", class.py_type);
    let py_name = &class.py_name;
    let callback_methods = class
        .methods
        .iter()
        .map(|method| {
            if !method.args.is_empty() || method.return_type != "()" {
                anyhow::bail!(
                    "sdk_callback_adapter method {} must have no generated args and return ()",
                    method.sdk_method
                );
            }
            let method_name = &method.py_name;
            let ident = format_ident!("{}", method.py_name);
            Ok(quote! {
                pub fn #ident(&self, py: pyo3::Python<'_>) -> pyo3::PyResult<()> {
                    match self.strategy.call_method0(py, #method_name) {
                        Ok(_) => Ok(()),
                        Err(err) if err.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) => Ok(()),
                        Err(err) => Err(err),
                    }
                }
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(quote! {
        #[pyclass(name = #py_name)]
        pub struct #py_type {
            strategy: pyo3::Py<pyo3::PyAny>,
        }

        impl #py_type {
            pub fn from_strategy(strategy: pyo3::Py<pyo3::PyAny>) -> Self {
                Self { strategy }
            }

            #(#callback_methods)*
        }

        #[pymethods]
        impl #py_type {
            #[new]
            fn new(strategy: pyo3::Py<pyo3::PyAny>) -> Self {
                Self::from_strategy(strategy)
            }
        }
    })
}

fn render_args(args: &[BindArg], registry: &TypeRegistry) -> Result<Vec<TokenStream>> {
    args.iter()
        .map(|arg| {
            let name = format_ident!("{}", arg.name);
            let ty = py_primitive_or_generated_type(&arg.ty, Some(registry))?;
            Ok(quote! { #name: #ty })
        })
        .collect()
}

fn sdk_arg_expr(arg: &BindArg, registry: &TypeRegistry) -> TokenStream {
    let name = format_ident!("{}", arg.name);
    let normalized = normalize_type(&arg.ty);

    if is_insight_period_type(&normalized) {
        return quote! {
            lean_sdk::framework::InsightPeriod(crate::compat::insight_period_from_py(#name)?)
        };
    }

    if let Some(inner) = option_inner(&normalized) {
        if registry.generated_enum(inner) {
            return quote! { #name.map(Into::into) };
        }
        if registry.wrappers.contains_key(inner) {
            return quote! { #name.map(|value| value.sdk().inner().clone()) };
        }
        if registry.generated.contains_key(inner) {
            return quote! { #name.map(|value| value.sdk().clone()) };
        }
        return quote! { #name };
    }

    if normalized == "&str" {
        return quote! { #name.as_str() };
    }
    if registry
        .wrappers
        .contains_key(normalized.trim_start_matches('&'))
    {
        if normalized.starts_with('&') {
            return quote! { #name.sdk().inner() };
        }
        return quote! { #name.sdk().inner().clone() };
    }
    if registry.generated_enum(&normalized) {
        return quote! { #name.into() };
    }
    if registry.generated.contains_key(normalized.as_str()) {
        return quote! { #name.sdk().clone() };
    }

    quote! { #name }
}

fn render_getter_with_name(
    getter: &BindGetter,
    py_name: &str,
    registry: &TypeRegistry,
) -> Result<TokenStream> {
    let sdk_method = format_ident!("{}", getter.sdk_method);
    let rust_method = if py_name == getter.py_name {
        rust_ident(&format!(
            "{}_{}_getter",
            getter.sdk_method,
            safe_ident_suffix(py_name)
        ))
    } else {
        rust_ident(&format!(
            "{}_alias_{}_getter",
            getter.sdk_method,
            safe_ident_suffix(py_name)
        ))
    };
    let return_type = registry.py_return_type(&getter.return_type)?;
    let call = quote! { self.inner.#sdk_method() };
    let body = registry.return_expr(&call, &getter.return_type);

    Ok(quote! {
        #[getter(#py_name)]
        fn #rust_method(&self) -> #return_type {
            #body
        }
    })
}

fn render_setter(setter: &BindSetter, registry: &TypeRegistry) -> Result<TokenStream> {
    if setter.args.len() != 1 {
        anyhow::bail!(
            "sdk_setter {} must take exactly one value argument",
            setter.sdk_method
        );
    }
    let sdk_method = format_ident!("{}", setter.sdk_method);
    let rust_method = rust_ident(&format!("set_{}_property", setter.property));
    let property = &setter.property;
    let args = render_args(&setter.args, registry)?;
    let arg = &setter.args[0];
    let arg_expr = sdk_arg_expr(arg, registry);

    Ok(quote! {
        #[setter(#property)]
        fn #rust_method(&self, #(#args),*) {
            self.inner.#sdk_method(#arg_expr);
        }
    })
}

fn render_protocols(class: &BindClass, registry: &TypeRegistry) -> Result<TokenStream> {
    let mut tokens = TokenStream::new();

    if let Some(method) = &class.protocols.str_method {
        let method = format_ident!("{}", method);
        tokens.extend(quote! {
            fn __str__(&self) -> String {
                self.inner.#method().to_string()
            }
        });
    }

    if let Some(method) = &class.protocols.repr_method {
        let method = format_ident!("{}", method);
        let py_name = &class.py_name;
        tokens.extend(quote! {
            fn __repr__(&self) -> String {
                format!("{}({:?})", #py_name, self.inner.#method())
            }
        });
    }

    if let Some(method) = &class.protocols.hash_method {
        let method = format_ident!("{}", method);
        tokens.extend(quote! {
            fn __hash__(&self) -> isize {
                let hash = self.inner.#method() as isize;
                if hash == -1 { -2 } else { hash }
            }
        });
    }

    if let Some(method) = &class.protocols.richcmp_method {
        let method = format_ident!("{}", method);
        let py_type = format_ident!("{}", class.py_type);
        let return_type = registry.py_return_type("bool")?;
        let _ = return_type;
        tokens.extend(quote! {
            fn __richcmp__(
                &self,
                other: &Bound<'_, PyAny>,
                op: pyo3::pyclass::CompareOp,
            ) -> PyResult<Py<PyAny>> {
                let Ok(other) = other.extract::<PyRef<'_, #py_type>>() else {
                    return Ok(other.py().NotImplemented());
                };
                let left = self.inner.#method();
                let right = other.inner.#method();
                let result = match op {
                    pyo3::pyclass::CompareOp::Eq => left == right,
                    pyo3::pyclass::CompareOp::Ne => left != right,
                    pyo3::pyclass::CompareOp::Lt => left < right,
                    pyo3::pyclass::CompareOp::Le => left <= right,
                    pyo3::pyclass::CompareOp::Gt => left > right,
                    pyo3::pyclass::CompareOp::Ge => left >= right,
                };
                Ok(pyo3::types::PyBool::new(other.py(), result)
                    .to_owned()
                    .into_any()
                    .unbind())
            }
        });
    }

    Ok(tokens)
}

fn rust_ident(name: &str) -> proc_macro2::Ident {
    match name {
        "type" | "match" | "ref" | "self" | "super" | "crate" => format_ident!("r#{}", name),
        _ => format_ident!("{}", name),
    }
}

fn safe_ident_suffix(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "alias".to_string()
    } else {
        out
    }
}

#[derive(Debug, Clone)]
struct TypeRegistry {
    generated: std::collections::HashMap<String, String>,
    wrappers: std::collections::HashMap<String, WrappedType>,
    enums: std::collections::HashSet<String>,
}

#[derive(Debug, Clone)]
struct WrappedType {
    module_path: String,
    sdk_type: String,
    wrap_constructor: String,
}

impl TypeRegistry {
    fn new(classes: &[BindClass], enums: &[BindEnum]) -> Self {
        let mut generated = std::collections::HashMap::new();
        let mut wrappers = std::collections::HashMap::new();
        let mut enum_keys = std::collections::HashSet::new();
        for class in classes {
            generated.insert(normalize_type(&class.sdk_type), class.py_type.clone());
            generated.insert(
                normalize_type(&format!("{}::{}", class.module_path, class.sdk_type)),
                class.py_type.clone(),
            );
            if let Some(wrapped_type) = &class.wrapped_type {
                let wrapped_key = normalize_type(wrapped_type);
                let wrapped_leaf = wrapped_key
                    .rsplit("::")
                    .next()
                    .unwrap_or(&wrapped_key)
                    .to_string();
                let wrapper = WrappedType {
                        module_path: class.module_path.clone(),
                        sdk_type: class.sdk_type.clone(),
                        wrap_constructor: class
                            .wrap_constructor
                            .clone()
                            .unwrap_or_else(|| "new".to_string()),
                    };
                generated.insert(wrapped_key.clone(), class.py_type.clone());
                generated.insert(wrapped_leaf.clone(), class.py_type.clone());
                wrappers.insert(wrapped_key, wrapper.clone());
                wrappers.insert(wrapped_leaf, wrapper);
            }
        }
        for bind_enum in enums {
            let sdk_key = normalize_type(&bind_enum.sdk_type);
            enum_keys.insert(sdk_key.clone());
            generated.insert(sdk_key, bind_enum.py_type.clone());
            if let Some(rust_type) = &bind_enum.rust_type {
                let rust_key = normalize_type(rust_type);
                enum_keys.insert(rust_key.clone());
                generated.insert(rust_key, bind_enum.py_type.clone());
            }
        }
        Self {
            generated,
            wrappers,
            enums: enum_keys,
        }
    }

    fn py_return_type(&self, return_type: &str) -> Result<TokenStream> {
        py_primitive_or_generated_type(return_type, Some(self))
    }

    fn return_expr(&self, call: &TokenStream, return_type: &str) -> TokenStream {
        let normalized = normalize_type(return_type);
        if let Some(inner) = option_inner(&normalized) {
            if self.generated.contains_key(inner) {
                let py_type = format_ident!("{}", self.generated[inner]);
                if self.generated_enum(inner) {
                    return quote! { #call.map(Into::into) };
                }
                if let Some(wrapper) = self.wrappers.get(inner) {
                    let wrap_path = wrapper_path(wrapper);
                    return quote! { #call.map(#wrap_path).map(#py_type::from_view) };
                }
                return quote! { #call.map(#py_type::from_view) };
            }
        }
        if let Some(inner) = vec_inner(&normalized) {
            if self.generated.contains_key(inner) {
                let py_type = format_ident!("{}", self.generated[inner]);
                if self.generated_enum(inner) {
                    return quote! { #call.into_iter().map(Into::into).collect() };
                }
                if let Some(wrapper) = self.wrappers.get(inner) {
                    let wrap_path = wrapper_path(wrapper);
                    return quote! { #call.into_iter().map(#wrap_path).map(#py_type::from_view).collect() };
                }
                return quote! { #call.into_iter().map(#py_type::from_view).collect() };
            }
        }
        let bare = normalized.trim_start_matches('&');
        if self.generated.contains_key(bare) {
            let py_type = format_ident!("{}", self.generated[bare]);
            if self.generated_enum(bare) {
                quote! { #call.into() }
            } else if let Some(wrapper) = self.wrappers.get(bare) {
                let wrap_path = wrapper_path(wrapper);
                if normalized.starts_with('&') {
                    quote! { #py_type::from_view(#wrap_path(#call.clone())) }
                } else {
                    quote! { #py_type::from_view(#wrap_path(#call)) }
                }
            } else if normalized.starts_with('&') {
                quote! { #py_type::from_view(#call.clone()) }
            } else {
                quote! { #py_type::from_view(#call) }
            }
        } else if normalized == "&str" {
            quote! { #call.to_string() }
        } else {
            quote! { #call }
        }
    }

    fn generated_enum(&self, normalized_type: &str) -> bool {
        self.enums.contains(normalized_type)
    }
}

fn wrapper_path(wrapper: &WrappedType) -> TokenStream {
    let module_path: TokenStream = wrapper
        .module_path
        .parse()
        .expect("validated module path from SDK parser");
    let sdk_type = format_ident!("{}", wrapper.sdk_type);
    let wrap_constructor = format_ident!("{}", wrapper.wrap_constructor);
    quote! { #module_path::#sdk_type::#wrap_constructor }
}

fn py_return_type_for_class(
    return_type: &str,
    class_sdk_type: &str,
    class_py_type: &str,
    registry: &TypeRegistry,
) -> Result<TokenStream> {
    if normalize_type(return_type) == normalize_type(class_sdk_type) {
        let py_type = format_ident!("{}", class_py_type);
        return Ok(quote! { #py_type });
    }
    if return_type == "Self" {
        let py_type = format_ident!("{}", class_py_type);
        return Ok(quote! { #py_type });
    }
    registry.py_return_type(return_type)
}

fn method_return_expr(
    call: &TokenStream,
    return_type: &str,
    registry: &TypeRegistry,
) -> TokenStream {
    registry.return_expr(call, return_type)
}

fn method_args_need_py_result(args: &[BindArg]) -> bool {
    args.iter().any(|arg| is_insight_period_type(&arg.ty))
}

fn is_insight_period_type(ty: &str) -> bool {
    let normalized = normalize_type(ty);
    matches!(
        normalized.as_str(),
        "InsightPeriod" | "lean_sdk::framework::InsightPeriod" | "framework::InsightPeriod"
    )
}

fn py_primitive_or_generated_type(
    ty: &str,
    registry: Option<&TypeRegistry>,
) -> Result<TokenStream> {
    let normalized = normalize_type(ty);
    if let Some(inner) = option_inner(&normalized) {
        let inner = py_primitive_or_generated_type(inner, registry)?;
        return Ok(quote! { Option<#inner> });
    }
    if let Some(inner) = vec_inner(&normalized) {
        let inner = py_primitive_or_generated_type(inner, registry)?;
        return Ok(quote! { Vec<#inner> });
    }
    if let Some(inner) = hash_map_inner(&normalized) {
        let (key, value) = split_top_level_comma(inner)
            .ok_or_else(|| anyhow::anyhow!("unsupported HashMap type {ty}"))?;
        let key = py_primitive_or_generated_type(key, registry)?;
        let value = py_primitive_or_generated_type(value, registry)?;
        return Ok(quote! { std::collections::HashMap<#key, #value> });
    }
    if is_insight_period_type(&normalized) {
        return Ok(quote! { &::pyo3::Bound<'_, ::pyo3::PyAny> });
    }

    match normalized.as_str() {
        "()" => Ok(quote! { () }),
        "bool" => Ok(quote! { bool }),
        "f64" => Ok(quote! { f64 }),
        "f32" => Ok(quote! { f32 }),
        "usize" => Ok(quote! { usize }),
        "u64" => Ok(quote! { u64 }),
        "u32" => Ok(quote! { u32 }),
        "i64" => Ok(quote! { i64 }),
        "i32" => Ok(quote! { i32 }),
        "String" => Ok(quote! { String }),
        "&str" => Ok(quote! { String }),
        "chrono::NaiveDate" | "NaiveDate" => Ok(quote! { chrono::NaiveDate }),
        "chrono::NaiveDateTime" | "NaiveDateTime" => Ok(quote! { chrono::NaiveDateTime }),
        "Self" => Ok(quote! { Self }),
        other => {
            if let Some(registry) = registry {
                if let Some(py_type) = registry.generated.get(other.trim_start_matches('&')) {
                    let py_type = format_ident!("{}", py_type);
                    return Ok(quote! { #py_type });
                }
            }
            anyhow::bail!("unsupported type {ty}")
        }
    }
}

fn can_render_signature(args: &[BindArg], return_type: &str, registry: &TypeRegistry) -> bool {
    can_render_args(args, registry) && can_render_type(return_type, registry)
}

fn can_render_args(args: &[BindArg], registry: &TypeRegistry) -> bool {
    args.iter().all(|arg| can_render_type(&arg.ty, registry))
}

fn can_render_type(ty: &str, registry: &TypeRegistry) -> bool {
    py_primitive_or_generated_type(ty, Some(registry)).is_ok()
}

fn normalize_type(ty: &str) -> String {
    ty.split_whitespace().collect::<String>()
}

fn option_inner(ty: &str) -> Option<&str> {
    ty.strip_prefix("Option<")?.strip_suffix('>')
}

fn vec_inner(ty: &str) -> Option<&str> {
    ty.strip_prefix("Vec<")?.strip_suffix('>')
}

fn hash_map_inner(ty: &str) -> Option<&str> {
    ty.strip_prefix("HashMap<")?.strip_suffix('>')
}

fn render_enum(bind_enum: &BindEnum) -> Result<TokenStream> {
    let py_type = format_ident!("{}", bind_enum.py_type);
    let py_name = &bind_enum.py_name;
    let variants = bind_enum
        .variants
        .iter()
        .map(|variant| {
            let ident = format_ident!("{}", variant.name);
            let value: TokenStream = variant.value.to_string().parse().expect("integer literal");
            quote! { #ident = #value }
        })
        .collect::<Vec<_>>();
    let classattrs = bind_enum
        .variants
        .iter()
        .filter_map(|variant| {
            let attr = screaming_snake(&variant.name);
            if attr == format_ident!("{}", variant.name) {
                return None;
            }
            let ident = format_ident!("{}", variant.name);
            Some(quote! {
                #[classattr]
                const #attr: Self = Self::#ident;
            })
        })
        .collect::<Vec<_>>();
    let conversion_impls = if let Some(rust_type) = &bind_enum.rust_type {
        let rust_type: TokenStream = rust_type
            .parse()
            .map_err(|err| anyhow::anyhow!("invalid rust_type {rust_type}: {err}"))?;
        let py_to_rust_arms = bind_enum
            .variants
            .iter()
            .map(|variant| {
                let ident = format_ident!("{}", variant.name);
                quote! { #py_type::#ident => #rust_type::#ident }
            })
            .collect::<Vec<_>>();
        let reverse_impl = if bind_enum.reverse {
            let rust_to_py_arms = bind_enum
                .variants
                .iter()
                .map(|variant| {
                    let ident = format_ident!("{}", variant.name);
                    quote! { #rust_type::#ident => #py_type::#ident }
                })
                .collect::<Vec<_>>();
            quote! {
                impl From<#rust_type> for #py_type {
                    fn from(value: #rust_type) -> Self {
                        match value {
                            #(#rust_to_py_arms),*
                        }
                    }
                }
            }
        } else {
            TokenStream::new()
        };
        quote! {
            impl From<#py_type> for #rust_type {
                fn from(value: #py_type) -> Self {
                    match value {
                        #(#py_to_rust_arms),*
                    }
                }
            }

            #reverse_impl
        }
    } else {
        TokenStream::new()
    };

    Ok(quote! {
        #[pyclass(name = #py_name, eq, eq_int)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum #py_type {
            #(#variants),*
        }

        #[pymethods]
        impl #py_type {
            #(#classattrs)*
        }

        #conversion_impls
    })
}

#[derive(Debug, Clone, Default)]
struct SdkBindMeta {
    py_name: String,
    rust_type: Option<String>,
    reverse: bool,
    subclass: bool,
    constructor_style: ConstructorStyle,
    wrapped_type: Option<String>,
    wrap_constructor: Option<String>,
    protocols: BindProtocols,
}

fn sdk_bind_meta(attrs: &[Attribute]) -> Result<Option<SdkBindMeta>> {
    for attr in attrs {
        if !attr.path().is_ident("sdk_bind") {
            continue;
        }
        let mut py_name = None;
        let mut rust_type = None;
        let mut reverse = true;
        let mut subclass = false;
        let mut constructor_style = ConstructorStyle::Normal;
        let mut wrapped_type = None;
        let mut wrap_constructor = None;
        let mut protocols = BindProtocols::default();
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("py_name") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                py_name = Some(lit.value());
            } else if meta.path.is_ident("rust_type") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                rust_type = Some(lit.value());
            } else if meta.path.is_ident("reverse") {
                let value = meta.value()?;
                let lit: syn::LitBool = value.parse()?;
                reverse = lit.value;
            } else if meta.path.is_ident("subclass") {
                subclass = true;
            } else if meta.path.is_ident("constructor") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                constructor_style = match lit.value().as_str() {
                    "normal" => ConstructorStyle::Normal,
                    "variadic" => ConstructorStyle::Variadic,
                    other => {
                        return Err(meta.error(format!(
                            "unsupported sdk_bind constructor style {other}"
                        )))
                    }
                };
            } else if meta.path.is_ident("wraps") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                wrapped_type = Some(lit.value());
            } else if meta.path.is_ident("wrap_constructor") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                wrap_constructor = Some(lit.value());
            } else if meta.path.is_ident("str") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                protocols.str_method = Some(lit.value());
            } else if meta.path.is_ident("repr") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                protocols.repr_method = Some(lit.value());
            } else if meta.path.is_ident("hash") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                protocols.hash_method = Some(lit.value());
            } else if meta.path.is_ident("richcmp") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                protocols.richcmp_method = Some(lit.value());
            }
            Ok(())
        })?;
        return Ok(py_name.map(|py_name| SdkBindMeta {
            py_name,
            rust_type,
            reverse,
            subclass,
            constructor_style,
            wrapped_type,
            wrap_constructor,
            protocols,
        }));
    }
    Ok(None)
}

fn literal_int_expr(expr: &Expr) -> Result<i64> {
    match expr {
        Expr::Lit(expr_lit) => match &expr_lit.lit {
            Lit::Int(value) => value
                .base10_parse::<i64>()
                .context("failed to parse enum integer discriminant"),
            _ => anyhow::bail!("sdk_bind enum discriminant must be an integer literal"),
        },
        _ => anyhow::bail!("sdk_bind enum discriminant must be an integer literal"),
    }
}

fn screaming_snake(name: &str) -> proc_macro2::Ident {
    let mut out = String::new();
    for (idx, ch) in name.chars().enumerate() {
        if ch.is_uppercase() && idx > 0 {
            out.push('_');
        }
        out.push(ch.to_ascii_uppercase());
    }
    format_ident!("{}", out)
}

#[derive(Debug, Clone, Default)]
struct SdkCallableMeta {
    py_name: Option<String>,
    aliases: Vec<String>,
}

#[derive(Debug, Clone)]
struct SdkSetterMeta {
    property: String,
}

fn sdk_getter_meta(method: &ImplItemFn) -> Result<Option<SdkCallableMeta>> {
    for attr in &method.attrs {
        if !attr.path().is_ident("sdk_getter") {
            continue;
        }
        if matches!(attr.meta, Meta::Path(_)) {
            return Ok(Some(SdkCallableMeta::default()));
        }
        let mut meta_out = SdkCallableMeta::default();
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("py_name") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                meta_out.py_name = Some(lit.value());
            } else if meta.path.is_ident("alias") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                meta_out.aliases.push(lit.value());
            }
            Ok(())
        })?;
        return Ok(Some(meta_out));
    }
    Ok(None)
}

fn sdk_method_meta(method: &ImplItemFn) -> Result<Option<SdkCallableMeta>> {
    sdk_callable_meta(method, "sdk_method")
}

fn sdk_static_meta(method: &ImplItemFn) -> Result<Option<SdkCallableMeta>> {
    sdk_callable_meta(method, "sdk_static")
}

fn sdk_callable_meta(method: &ImplItemFn, attr_name: &str) -> Result<Option<SdkCallableMeta>> {
    for attr in &method.attrs {
        if !attr.path().is_ident(attr_name) {
            continue;
        }
        if matches!(attr.meta, Meta::Path(_)) {
            return Ok(Some(SdkCallableMeta::default()));
        }
        let mut meta_out = SdkCallableMeta::default();
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("py_name") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                meta_out.py_name = Some(lit.value());
            } else if meta.path.is_ident("alias") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                meta_out.aliases.push(lit.value());
            }
            Ok(())
        })?;
        return Ok(Some(meta_out));
    }
    Ok(None)
}

fn sdk_setter_meta(method: &ImplItemFn) -> Result<Option<SdkSetterMeta>> {
    for attr in &method.attrs {
        if !attr.path().is_ident("sdk_setter") {
            continue;
        }
        let mut property = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("property") {
                let value = meta.value()?;
                let lit: LitStr = value.parse()?;
                property = Some(lit.value());
            }
            Ok(())
        })?;
        let property = property
            .ok_or_else(|| anyhow::anyhow!("sdk_setter {} requires property", method.sig.ident))?;
        return Ok(Some(SdkSetterMeta { property }));
    }
    Ok(None)
}

fn has_attr(method: &ImplItemFn, attr_name: &str) -> bool {
    method
        .attrs
        .iter()
        .any(|attr| attr.path().is_ident(attr_name))
}

fn method_takes_mut_self(method: &ImplItemFn) -> bool {
    matches!(
        method.sig.inputs.first(),
        Some(FnArg::Receiver(receiver)) if receiver.mutability.is_some()
    )
}

fn has_struct_attr(attrs: &[Attribute], attr_name: &str) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident(attr_name))
}

fn derives_trait(attrs: &[Attribute], trait_name: &str) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("derive") {
            return false;
        }
        let mut found = false;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident(trait_name) {
                found = true;
            }
            Ok(())
        });
        found
    })
}

fn impl_self_type(item_impl: &ItemImpl) -> Option<String> {
    let Type::Path(type_path) = item_impl.self_ty.as_ref() else {
        return None;
    };
    type_path
        .path
        .segments
        .last()
        .map(|seg| seg.ident.to_string())
}

fn method_return_type(method: &ImplItemFn) -> Result<String> {
    if method.sig.inputs.len() != 1 {
        anyhow::bail!("sdk_getter {} must take only &self", method.sig.ident);
    }
    match method.sig.inputs.first() {
        Some(FnArg::Receiver(_)) => {}
        _ => anyhow::bail!("sdk_getter {} must be a method", method.sig.ident),
    }

    let ReturnType::Type(_, ty) = &method.sig.output else {
        anyhow::bail!("sdk_getter {} must return a value", method.sig.ident);
    };
    Ok(ty.to_token_stream().to_string())
}

fn method_return_type_any_args(method: &ImplItemFn) -> Result<String> {
    let ReturnType::Type(_, ty) = &method.sig.output else {
        return Ok("()".to_string());
    };
    Ok(ty.to_token_stream().to_string())
}

fn method_args(method: &ImplItemFn) -> Result<Vec<BindArg>> {
    let mut args = Vec::new();
    for input in method.sig.inputs.iter().skip(1) {
        let FnArg::Typed(pat_type) = input else {
            continue;
        };
        let Pat::Ident(PatIdent { ident, .. }) = pat_type.pat.as_ref() else {
            anyhow::bail!(
                "sdk_method {} has unsupported argument pattern",
                method.sig.ident
            );
        };
        args.push(BindArg {
            name: ident.to_string(),
            ty: pat_type.ty.to_token_stream().to_string(),
        });
    }
    Ok(args)
}

fn method_args_static(method: &ImplItemFn) -> Result<Vec<BindArg>> {
    method_args_from_inputs(method, method.sig.inputs.iter())
}

fn method_args_from_inputs<'a>(
    method: &ImplItemFn,
    inputs: impl Iterator<Item = &'a FnArg>,
) -> Result<Vec<BindArg>> {
    let mut args = Vec::new();
    for input in inputs {
        let FnArg::Typed(pat_type) = input else {
            continue;
        };
        let Pat::Ident(PatIdent { ident, .. }) = pat_type.pat.as_ref() else {
            anyhow::bail!(
                "sdk_method {} has unsupported argument pattern",
                method.sig.ident
            );
        };
        args.push(BindArg {
            name: ident.to_string(),
            ty: pat_type.ty.to_token_stream().to_string(),
        });
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_class() -> BindClass {
        BindClass {
            sdk_type: "ExampleHandle".to_string(),
            py_type: "PyExample".to_string(),
            py_name: "Example".to_string(),
            module_path: "example_sdk::sample".to_string(),
            callback_adapter: false,
            subclass: false,
            constructor_style: ConstructorStyle::Normal,
            wrapped_type: None,
            wrap_constructor: None,
            protocols: BindProtocols::default(),
            mutable: false,
            cloneable: true,
            getters: vec![BindGetter {
                sdk_method: "display_value".to_string(),
                py_name: "display_value".to_string(),
                aliases: Vec::new(),
                return_type: "f64".to_string(),
            }],
            methods: vec![
                BindMethod {
                    sdk_method: "set_value".to_string(),
                    py_name: "set_value".to_string(),
                    aliases: Vec::new(),
                    args: vec![BindArg {
                        name: "amount".to_string(),
                        ty: "f64".to_string(),
                    }],
                    return_type: "()".to_string(),
                    mutates: false,
                },
                BindMethod {
                    sdk_method: "add_item".to_string(),
                    py_name: "add_item".to_string(),
                    aliases: Vec::new(),
                    args: vec![
                        BindArg {
                            name: "name".to_string(),
                            ty: "String".to_string(),
                        },
                        BindArg {
                            name: "mode".to_string(),
                            ty: "ExampleMode".to_string(),
                        },
                    ],
                    return_type: "ItemHandle".to_string(),
                    mutates: false,
                },
            ],
            statics: Vec::new(),
            constructor: Some(BindConstructor {
                sdk_method: "new".to_string(),
                args: Vec::new(),
            }),
            setters: Vec::new(),
        }
    }

    fn example_enum() -> BindEnum {
        BindEnum {
            sdk_type: "ExampleMode".to_string(),
            py_type: "PyExampleMode".to_string(),
            py_name: "ExampleMode".to_string(),
            rust_type: Some("example_sdk::ExampleMode".to_string()),
            reverse: false,
            variants: vec![
                BindEnumVariant {
                    name: "FirstChoice".to_string(),
                    value: 0,
                },
                BindEnumVariant {
                    name: "Second".to_string(),
                    value: 1,
                },
            ],
        }
    }

    fn reversible_enum() -> BindEnum {
        BindEnum {
            sdk_type: "ExampleMode".to_string(),
            py_type: "PyExampleMode".to_string(),
            py_name: "ExampleMode".to_string(),
            rust_type: Some("example_core::ExampleMode".to_string()),
            reverse: true,
            variants: vec![BindEnumVariant {
                name: "Daily".to_string(),
                value: 4,
            }],
        }
    }

    fn wrapped_thing_class() -> BindClass {
        BindClass {
            sdk_type: "WrappedThingHandle".to_string(),
            py_type: "PyWrappedThing".to_string(),
            py_name: "WrappedThing".to_string(),
            module_path: "example_sdk::things".to_string(),
            callback_adapter: false,
            subclass: false,
            constructor_style: ConstructorStyle::Normal,
            wrapped_type: Some("example_core::WrappedThing".to_string()),
            wrap_constructor: Some("new".to_string()),
            protocols: BindProtocols::default(),
            mutable: false,
            cloneable: true,
            getters: Vec::new(),
            methods: Vec::new(),
            statics: Vec::new(),
            constructor: None,
            setters: Vec::new(),
        }
    }

    fn item_class() -> BindClass {
        BindClass {
            sdk_type: "ItemHandle".to_string(),
            py_type: "PyItem".to_string(),
            py_name: "Item".to_string(),
            module_path: "example_sdk::items".to_string(),
            callback_adapter: false,
            subclass: false,
            constructor_style: ConstructorStyle::Normal,
            wrapped_type: None,
            wrap_constructor: None,
            protocols: BindProtocols::default(),
            mutable: false,
            cloneable: true,
            getters: vec![BindGetter {
                sdk_method: "thing".to_string(),
                py_name: "thing".to_string(),
                aliases: Vec::new(),
                return_type: "WrappedThingHandle".to_string(),
            }],
            methods: Vec::new(),
            statics: Vec::new(),
            constructor: None,
            setters: Vec::new(),
        }
    }

    fn nested_view_class() -> BindClass {
        BindClass {
            sdk_type: "NestedView".to_string(),
            py_type: "PyNested".to_string(),
            py_name: "Nested".to_string(),
            module_path: "example_sdk::views".to_string(),
            callback_adapter: false,
            subclass: false,
            constructor_style: ConstructorStyle::Normal,
            wrapped_type: None,
            wrap_constructor: None,
            protocols: BindProtocols::default(),
            mutable: false,
            cloneable: true,
            getters: Vec::new(),
            methods: Vec::new(),
            statics: Vec::new(),
            constructor: None,
            setters: Vec::new(),
        }
    }

    #[test]
    fn pyi_uses_snake_case_methods_and_getter_overrides() {
        let pyi = render_algorithm_imports_pyi(&[example_class(), item_class()], &[]);

        assert!(pyi.contains("class Example:"));
        assert!(pyi.contains("class Item:"));
        assert!(pyi.contains("@property\n    def thing(self) -> WrappedThing: ..."));
        assert!(pyi.contains("def __init__(self) -> None: ..."));
        assert!(pyi.contains("@property\n    def display_value(self) -> float: ..."));
        assert!(pyi.contains("def set_value(self, amount: float) -> None: ..."));
        assert!(pyi.contains(
            "def add_item(self, name: str, mode: ExampleMode) -> Item: ..."
        ));
        assert!(!pyi.contains("SetValue"));
        assert!(!pyi.contains("DisplayValue"));
    }

    #[test]
    fn pyi_emits_screaming_snake_enum_aliases() {
        let pyi = render_algorithm_imports_pyi(&[], &[example_enum()]);

        assert!(pyi.contains("class ExampleMode:"));
        assert!(pyi.contains("    FirstChoice: ExampleMode"));
        assert!(pyi.contains("    FIRST_CHOICE: ExampleMode"));
        assert!(pyi.contains("    Second: ExampleMode"));
        assert!(pyi.contains("    SECOND: ExampleMode"));
    }

    #[test]
    fn pyi_renders_rust_maps_as_python_dicts() {
        assert_eq!(pyi_type("HashMap < String , String >"), "dict[str, str]");
        assert_eq!(
            pyi_type("Option < HashMap < String , Vec < String > > >"),
            "Optional[dict[str, list[str]]]"
        );
    }

    #[test]
    fn parsed_sdk_metadata_preserves_snake_case_python_names() {
        let path =
            std::env::temp_dir().join(format!("rlean-sdk-pyo3-gen-test-{}.rs", std::process::id()));
        fs::write(
            &path,
            r#"
                use lean_sdk_annotations::{sdk_bind, sdk_getter, sdk_method};

                #[sdk_bind(py_name = "Example")]
                pub struct ExampleHandle;

                impl ExampleHandle {
                    #[sdk_method]
                    pub fn set_value(&self, amount: f64) {}

                    #[sdk_getter(py_name = "display_value")]
                    pub fn total_value(&self) -> f64 { 0.0 }
                }
            "#,
        )
        .unwrap();

        let parsed = parse_sdk_file(&path, "lean_sdk::algorithm").unwrap();
        let _ = fs::remove_file(&path);
        let class = parsed
            .classes
            .iter()
            .find(|class| class.py_name == "Example")
            .unwrap();

        assert_eq!(class.methods[0].sdk_method, "set_value");
        assert_eq!(class.methods[0].py_name, "set_value");
        assert_eq!(class.getters[0].sdk_method, "total_value");
        assert_eq!(class.getters[0].py_name, "display_value");
    }

    #[test]
    fn generated_enum_args_use_into_not_sdk_wrapper_access() {
        let registry = TypeRegistry::new(&[], &[reversible_enum()]);

        let direct = sdk_arg_expr(
            &BindArg {
                name: "mode".to_string(),
                ty: "ExampleMode".to_string(),
            },
            &registry,
        )
        .to_string();
        let optional = sdk_arg_expr(
            &BindArg {
                name: "mode".to_string(),
                ty: "Option < ExampleMode >".to_string(),
            },
            &registry,
        )
        .to_string();

        assert_eq!(direct, "mode . into ()");
        assert_eq!(optional, "mode . map (Into :: into)");
        assert!(!direct.contains("sdk"));
        assert!(!optional.contains("sdk"));
    }

    #[test]
    fn generated_str_and_wrapped_args_match_sdk_method_signatures() {
        let registry = TypeRegistry::new(&[wrapped_thing_class()], &[]);

        let borrowed_str = sdk_arg_expr(
            &BindArg {
                name: "name".to_string(),
                ty: "& str".to_string(),
            },
            &registry,
        )
        .to_string();
        let borrowed_wrapped = sdk_arg_expr(
            &BindArg {
                name: "thing".to_string(),
                ty: "& example_core :: WrappedThing".to_string(),
            },
            &registry,
        )
        .to_string();
        let owned_wrapped = sdk_arg_expr(
            &BindArg {
                name: "thing".to_string(),
                ty: "example_core :: WrappedThing".to_string(),
            },
            &registry,
        )
        .to_string();

        assert_eq!(borrowed_str, "name . as_str ()");
        assert_eq!(borrowed_wrapped, "thing . sdk () . inner ()");
        assert_eq!(owned_wrapped, "thing . sdk () . inner () . clone ()");
    }

    #[test]
    fn generated_returns_wrap_declared_rust_types_and_nested_views() {
        let registry = TypeRegistry::new(&[wrapped_thing_class(), nested_view_class()], &[]);
        let wrapped_call = quote! { self.inner.thing() };
        let option_nested_call = quote! { self.inner.child() };
        let vec_wrapped_call = quote! { self.inner.things() };

        let wrapped = registry
            .return_expr(&wrapped_call, "& example_core :: WrappedThing")
            .to_string();
        let option_nested = registry
            .return_expr(&option_nested_call, "Option < NestedView >")
            .to_string();
        let vec_wrapped = registry
            .return_expr(&vec_wrapped_call, "Vec < example_core :: WrappedThing >")
            .to_string();

        assert_eq!(
            wrapped,
            "PyWrappedThing :: from_view (example_sdk :: things :: WrappedThingHandle :: new (self . inner . thing () . clone ()))"
        );
        assert_eq!(
            option_nested,
            "self . inner . child () . map (PyNested :: from_view)"
        );
        assert_eq!(
            vec_wrapped,
            "self . inner . things () . into_iter () . map (example_sdk :: things :: WrappedThingHandle :: new) . map (PyWrappedThing :: from_view) . collect ()"
        );
    }

    #[test]
    fn generator_source_does_not_hard_code_sdk_api_names() {
        let source_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
        let source = fs::read_to_string(source_path).unwrap();
        let forbidden = [
            "QCAlgorithm",
            "SymbolHandle",
            "PortfolioTarget",
            "TradeBar",
            "UniverseSettings",
            "SetCash",
            "lean_sdk::securities::SymbolHandle::new",
        ];
        let guard_start = source
            .find("fn generator_source_does_not_hard_code_sdk_api_names")
            .unwrap();
        let source_without_guard_literals = &source[..guard_start];

        for needle in forbidden {
            assert!(
                !source_without_guard_literals.contains(needle),
                "generic generator source must not hard-code SDK API name `{needle}`"
            );
        }
    }
}
