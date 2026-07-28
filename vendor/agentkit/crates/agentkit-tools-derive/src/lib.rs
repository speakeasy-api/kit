//! Procedural macros for declaring agentkit tools.
//!
//! Exposes one attribute macro, [`macro@tool`], that turns either an async
//! free function **or** an async method on a struct into an
//! `agentkit_tools_core::Tool` implementation. The free-function form
//! synthesises a unit struct named after the function; the method form
//! reuses the existing struct (so the tool can carry state like channels,
//! caches, or HTTP clients).
//!
//! The companion crate `agentkit-tools-core` provides the `tool_spec_for`
//! runtime helper that the generated code calls to derive the JSON Schema
//! from the input type via `schemars`.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    Attribute, Expr, ExprLit, FnArg, ImplItem, ImplItemFn, Item, ItemFn, ItemImpl, Lit, Meta,
    MetaNameValue, Pat, PatType, ReturnType, Signature, Token, Type, parse_macro_input,
    punctuated::Punctuated,
};

/// `#[tool]` attribute.
///
/// Apply to:
///
/// - **A free async function** whose first argument is a deserializable
///   input struct that implements `schemars::JsonSchema`. The macro
///   synthesises a `Default + Clone` unit struct with the same name as the
///   function, so registering it reads as `registry.with(my_tool)`.
/// - **An `impl` block on a struct**, containing exactly one async method
///   shaped `async fn <any_name>(&self, input: T) -> Result<ToolResult,
///   ToolError>`. The struct's existing fields are preserved, so the tool
///   can hold state (channels, clients, caches). The method's body becomes
///   the `Tool::invoke` body and the inherent impl block is replaced by
///   `impl Tool for SelfType`.
///
/// Recognised arguments:
///
/// - `name = "literal"` — overrides the tool's name. Defaults to the
///   function/method identifier.
/// - `description = "literal"` — sets the tool's description. Defaults to
///   the function/method's first doc comment line if present, otherwise
///   an empty string.
/// - Annotation flags — set the matching field on
///   [`ToolAnnotations`](agentkit_tools_core::ToolAnnotations). Each may
///   appear bare (sets `true`) or with an explicit boolean
///   (`destructive = false`):
///   - `read_only`
///   - `destructive`
///   - `idempotent`
///   - `needs_approval`
///   - `supports_streaming`
///
/// # Example — free function
///
/// ```rust,ignore
/// use agentkit_tools_core::{ToolError, ToolResult};
/// use agentkit_tools_derive::tool;
/// use schemars::JsonSchema;
/// use serde::Deserialize;
///
/// #[derive(JsonSchema, Deserialize)]
/// struct Input { city: String }
///
/// /// Fetch weather for a city.
/// #[tool(read_only)]
/// async fn get_weather(input: Input) -> Result<ToolResult, ToolError> {
///     # unimplemented!()
/// }
/// ```
///
/// # Example — stateful method
///
/// ```rust,ignore
/// use agentkit_tools_core::{ToolError, ToolResult};
/// use agentkit_tools_derive::tool;
/// use schemars::JsonSchema;
/// use serde::Deserialize;
/// use tokio::sync::mpsc;
///
/// #[derive(JsonSchema, Deserialize)]
/// struct ReconnectInput { server_id: String }
///
/// pub struct Reconnector { cmd_tx: mpsc::Sender<()> }
///
/// #[tool(idempotent)]
/// impl Reconnector {
///     /// Disconnect and reconnect a registered MCP server.
///     async fn run(&self, input: ReconnectInput) -> Result<ToolResult, ToolError> {
///         self.cmd_tx.send(()).await.unwrap();
///         # unimplemented!()
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attrs = parse_macro_input!(attr with Punctuated::<Meta, Token![,]>::parse_terminated);
    let item = parse_macro_input!(item as Item);

    let result = parse_attrs(attrs.into_iter().collect()).and_then(|parsed| match item {
        Item::Fn(func) => expand_tool_fn(parsed, func),
        Item::Impl(imp) => expand_tool_impl(parsed, imp),
        other => Err(syn::Error::new_spanned(
            other,
            "#[tool] applies to an async fn or an impl block",
        )),
    });

    match result {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

#[derive(Default)]
struct ParsedAttrs {
    name: Option<String>,
    description: Option<String>,
    annotations: AnnotationFlags,
}

#[derive(Default)]
struct AnnotationFlags {
    read_only: bool,
    destructive: bool,
    idempotent: bool,
    needs_approval: bool,
    supports_streaming: bool,
}

impl AnnotationFlags {
    fn is_default(&self) -> bool {
        !(self.read_only
            || self.destructive
            || self.idempotent
            || self.needs_approval
            || self.supports_streaming)
    }

    fn as_struct_literal(&self) -> TokenStream2 {
        let read_only = self.read_only;
        let destructive = self.destructive;
        let idempotent = self.idempotent;
        let needs_approval = self.needs_approval;
        let supports_streaming = self.supports_streaming;
        quote! {
            ::agentkit_tools_core::ToolAnnotations {
                read_only_hint: #read_only,
                destructive_hint: #destructive,
                idempotent_hint: #idempotent,
                needs_approval_hint: #needs_approval,
                supports_streaming_hint: #supports_streaming,
            }
        }
    }
}

fn parse_attrs(args: Vec<Meta>) -> syn::Result<ParsedAttrs> {
    let mut parsed = ParsedAttrs::default();

    for meta in args {
        match meta {
            Meta::NameValue(MetaNameValue {
                path,
                value:
                    Expr::Lit(ExprLit {
                        lit: Lit::Str(s), ..
                    }),
                ..
            }) if path.is_ident("name") => {
                parsed.name = Some(s.value());
            }
            Meta::NameValue(MetaNameValue {
                path,
                value:
                    Expr::Lit(ExprLit {
                        lit: Lit::Str(s), ..
                    }),
                ..
            }) if path.is_ident("description") => {
                parsed.description = Some(s.value());
            }
            Meta::NameValue(MetaNameValue {
                path,
                value:
                    Expr::Lit(ExprLit {
                        lit: Lit::Bool(b), ..
                    }),
                ..
            }) => {
                set_annotation(&mut parsed.annotations, &path, b.value)?;
            }
            Meta::Path(path) => {
                set_annotation(&mut parsed.annotations, &path, true)?;
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "expected `name = \"...\"`, `description = \"...\"`, or an annotation flag (e.g. `destructive`, `idempotent = true`)",
                ));
            }
        }
    }

    Ok(parsed)
}

fn set_annotation(flags: &mut AnnotationFlags, path: &syn::Path, value: bool) -> syn::Result<()> {
    let slot = if path.is_ident("read_only") {
        &mut flags.read_only
    } else if path.is_ident("destructive") {
        &mut flags.destructive
    } else if path.is_ident("idempotent") {
        &mut flags.idempotent
    } else if path.is_ident("needs_approval") {
        &mut flags.needs_approval
    } else if path.is_ident("supports_streaming") {
        &mut flags.supports_streaming
    } else {
        return Err(syn::Error::new_spanned(
            path,
            "unknown #[tool] argument; expected `name`, `description`, or one of \
             `read_only`, `destructive`, `idempotent`, `needs_approval`, `supports_streaming`",
        ));
    };
    *slot = value;
    Ok(())
}

fn build_spec_init(
    name_literal: &str,
    description_literal: &str,
    input_type: &Type,
    annotations: &AnnotationFlags,
) -> TokenStream2 {
    let base = quote! {
        ::agentkit_tools_core::tool_spec_for::<#input_type>(
            #name_literal,
            #description_literal,
        )
    };
    if annotations.is_default() {
        base
    } else {
        let ann = annotations.as_struct_literal();
        quote! { #base.with_annotations(#ann) }
    }
}

/// Emit the `impl Tool for #self_ty` block shared by both forms.
///
/// The user's body returns `Result<ToolResult, ToolError>`. The generated
/// invoke overwrites the result's `call_id` with the inbound
/// `request.call_id` so users can build results with placeholder ids; the
/// model's call_id is what ends up in the transcript.
fn emit_tool_impl(
    self_ty: &TokenStream2,
    spec_init: &TokenStream2,
    input_pat: &TokenStream2,
    input_type: &Type,
    user_return_type: &TokenStream2,
    body: &TokenStream2,
) -> TokenStream2 {
    quote! {
        #[::agentkit_tools_core::__private_async_trait::async_trait]
        impl ::agentkit_tools_core::Tool for #self_ty {
            fn spec(&self) -> &::agentkit_tools_core::ToolSpec {
                static SPEC: ::std::sync::OnceLock<::agentkit_tools_core::ToolSpec> =
                    ::std::sync::OnceLock::new();
                SPEC.get_or_init(|| #spec_init)
            }

            async fn invoke(
                &self,
                request: ::agentkit_tools_core::ToolRequest,
                _ctx: &mut ::agentkit_tools_core::ToolContext<'_>,
            ) -> ::std::result::Result<
                ::agentkit_tools_core::ToolResult,
                ::agentkit_tools_core::ToolError,
            > {
                let __call_id = request.call_id.clone();
                let #input_pat: #input_type = ::serde_json::from_value(request.input)
                    .map_err(|e| ::agentkit_tools_core::ToolError::InvalidInput(e.to_string()))?;
                let __body_fut = async move {
                    let __body_out: #user_return_type = #body;
                    __body_out
                };
                let mut __result: ::agentkit_tools_core::ToolResult = __body_fut.await?;
                __result.result.call_id = __call_id;
                ::std::result::Result::Ok(__result)
            }
        }
    }
}

fn expand_tool_fn(parsed: ParsedAttrs, func: ItemFn) -> syn::Result<TokenStream2> {
    if func.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            &func.sig,
            "#[tool] requires an async function",
        ));
    }

    let first = func.sig.inputs.first().ok_or_else(|| {
        syn::Error::new_spanned(
            &func.sig,
            "#[tool] requires at least one argument: the input type",
        )
    })?;
    if matches!(first, FnArg::Receiver(_)) {
        return Err(syn::Error::new_spanned(
            first,
            "#[tool] on a free function cannot take `self`; apply #[tool] to the impl block instead",
        ));
    }
    let (input_type, input_pat) = extract_typed_arg(first)?;
    let user_return_type = require_return_type(&func.sig)?;

    let fn_name = func.sig.ident.clone();
    let tool_name = parsed.name.unwrap_or_else(|| fn_name.to_string());
    let description = parsed
        .description
        .unwrap_or_else(|| extract_doc_comment(&func.attrs).unwrap_or_default());
    let spec_init = build_spec_init(&tool_name, &description, &input_type, &parsed.annotations);

    let body = &func.block;
    let body_tokens = quote! { #body };
    let self_ty = quote! { #fn_name };
    let tool_impl = emit_tool_impl(
        &self_ty,
        &spec_init,
        &input_pat,
        &input_type,
        &user_return_type,
        &body_tokens,
    );

    Ok(quote! {
        #[allow(non_camel_case_types)]
        #[derive(Default, Clone)]
        pub struct #fn_name;

        #tool_impl
    })
}

fn expand_tool_impl(parsed: ParsedAttrs, imp: ItemImpl) -> syn::Result<TokenStream2> {
    let ItemImpl {
        trait_,
        self_ty,
        items,
        ..
    } = imp;

    if let Some((_, trait_path, _)) = &trait_ {
        return Err(syn::Error::new_spanned(
            trait_path,
            "#[tool] applies to inherent impl blocks, not trait impls",
        ));
    }

    let method = take_single_method(items, &self_ty)?;

    if method.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            &method.sig,
            "#[tool] requires an async method",
        ));
    }

    let mut inputs = method.sig.inputs.iter();
    let receiver = inputs.next().ok_or_else(|| {
        syn::Error::new_spanned(
            &method.sig,
            "#[tool] method must take `&self` as its first argument",
        )
    })?;
    match receiver {
        FnArg::Receiver(rec) if rec.reference.is_some() && rec.mutability.is_none() => {}
        _ => {
            return Err(syn::Error::new_spanned(
                receiver,
                "#[tool] method must take `&self` (no `&mut self`, no owned `self`)",
            ));
        }
    }

    let input_arg = inputs.next().ok_or_else(|| {
        syn::Error::new_spanned(
            &method.sig,
            "#[tool] method must take an input argument after `&self`",
        )
    })?;
    let (input_type, input_pat) = extract_typed_arg(input_arg)?;

    if inputs.next().is_some() {
        return Err(syn::Error::new_spanned(
            &method.sig,
            "#[tool] method takes exactly one input argument after `&self`",
        ));
    }

    let user_return_type = require_return_type(&method.sig)?;
    let tool_name = parsed.name.unwrap_or_else(|| method.sig.ident.to_string());
    let description = parsed
        .description
        .unwrap_or_else(|| extract_doc_comment(&method.attrs).unwrap_or_default());
    let spec_init = build_spec_init(&tool_name, &description, &input_type, &parsed.annotations);

    let body = &method.block;
    let body_tokens = quote! { #body };
    let self_ty_tokens = quote! { #self_ty };

    Ok(emit_tool_impl(
        &self_ty_tokens,
        &spec_init,
        &input_pat,
        &input_type,
        &user_return_type,
        &body_tokens,
    ))
}

fn take_single_method(items: Vec<ImplItem>, self_ty: &Type) -> syn::Result<ImplItemFn> {
    let mut method: Option<ImplItemFn> = None;
    for item in items {
        match item {
            ImplItem::Fn(func) => {
                if method.is_some() {
                    return Err(syn::Error::new_spanned(
                        &func,
                        "#[tool] impl block must contain exactly one method",
                    ));
                }
                method = Some(func);
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "#[tool] impl block may only contain a single async method",
                ));
            }
        }
    }
    method.ok_or_else(|| {
        syn::Error::new_spanned(self_ty, "#[tool] impl block must contain an async method")
    })
}

fn extract_typed_arg(arg: &FnArg) -> syn::Result<(Type, TokenStream2)> {
    match arg {
        FnArg::Typed(PatType { pat, ty, .. }) => match pat.as_ref() {
            Pat::Ident(ident) => {
                let id = &ident.ident;
                Ok(((**ty).clone(), quote! { #id }))
            }
            _ => Err(syn::Error::new_spanned(
                pat,
                "#[tool] requires the input argument to be a simple identifier pattern (e.g. `input: MyInput`)",
            )),
        },
        FnArg::Receiver(_) => Err(syn::Error::new_spanned(
            arg,
            "#[tool] expects a typed input argument, not `self`",
        )),
    }
}

fn require_return_type(sig: &Signature) -> syn::Result<TokenStream2> {
    match &sig.output {
        ReturnType::Type(_, ty) => Ok(quote! { #ty }),
        ReturnType::Default => Err(syn::Error::new_spanned(
            sig,
            "#[tool] requires an explicit return type, e.g. `-> Result<ToolResult, ToolError>`",
        )),
    }
}

fn extract_doc_comment(attrs: &[Attribute]) -> Option<String> {
    let mut buf = String::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let Meta::NameValue(MetaNameValue {
            value: Expr::Lit(ExprLit {
                lit: Lit::Str(s), ..
            }),
            ..
        }) = &attr.meta
        {
            let line = s.value();
            let trimmed = line.trim();
            if trimmed.is_empty() {
                if !buf.is_empty() {
                    break;
                }
                continue;
            }
            if !buf.is_empty() {
                buf.push(' ');
            }
            buf.push_str(trimmed);
        }
    }
    if buf.is_empty() { None } else { Some(buf) }
}
