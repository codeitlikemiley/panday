//! `#[tool]` — a `Tool` impl and its JSON Schema from a function signature
//! (M10.4, docs/10 Layer 4).
//!
//! ```ignore
//! /// Look up an order by id.
//! #[panday_sdk::tool]
//! async fn lookup_order(ctx: &ToolCtx, order_id: String, limit: Option<u32>) -> Result<Order, String> {
//!     ...
//! }
//!
//! let mut registry = ToolRegistry::default();
//! registry.register(Box::new(LookupOrder));
//! ```
//!
//! ## What it generates, and the two names
//!
//! The macro leaves the function exactly as written and adds:
//!
//! - `struct LookupOrder` — a unit struct implementing `panday_harness::Tool`, named
//!   by PascalCasing the function.
//! - `struct LookupOrderArgs` — the deserialization target, one field per parameter
//!   after `ctx`, deriving `Deserialize` and `JsonSchema`. **This** is where the tool's
//!   JSON Schema comes from: the parameter list is the schema, so the two cannot
//!   disagree.
//!
//! docs/10's sketch writes `tools![lookup_order]`, i.e. the macro taking over the
//! function's own name. That is not done, and the divergence is recorded in docs/10:
//! a unit struct named `lookup_order` would occupy the value namespace the function
//! lives in, so the function would no longer be callable — including from its own unit
//! tests. A tool you cannot call directly is a tool you can only test through an
//! agent loop, which is a strictly worse place to test business logic.
//!
//! ## Why the description comes from the doc comment
//!
//! The description is what the model reads to decide whether to call the tool, and it
//! lands in the stable cached prefix (ADR-008). Taking it from `///` means the text a
//! developer maintains for humans is the text the model gets — one description, not
//! two that drift.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::spanned::Spanned;
use syn::{parse_macro_input, FnArg, ItemFn, Pat, PatType, ReturnType, Type};

/// Attribute arguments: `#[tool(side_effects = "idempotent")]`.
struct Args {
    side_effects: String,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            // The safe default: a tool that has not said otherwise is treated as a
            // pure read, which is the only claim we can make about a function whose
            // body we have not inspected — and `None` is the *least* permissive
            // reading in the sense that matters here, because a read-only tool is
            // allowed in every profile and a mutating one that lied would be a bug
            // in the manifest rather than in the gate.
            side_effects: "none".into(),
        }
    }
}

fn parse_args(attr: TokenStream) -> Result<Args, syn::Error> {
    let mut args = Args::default();
    if attr.is_empty() {
        return Ok(args);
    }
    let meta = syn::parse::<syn::MetaNameValue>(attr)?;
    if !meta.path.is_ident("side_effects") {
        return Err(syn::Error::new(
            meta.path.span(),
            "unknown option; `#[tool]` accepts `side_effects = \"none\" | \"idempotent\" | \"irreversible\"`",
        ));
    }
    let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Str(s),
        ..
    }) = meta.value
    else {
        return Err(syn::Error::new(
            meta.value.span(),
            "side_effects takes a string: `side_effects = \"idempotent\"`",
        ));
    };
    let value = s.value();
    if !["none", "idempotent", "irreversible"].contains(&value.as_str()) {
        return Err(syn::Error::new(
            s.span(),
            format!(
                "unknown side effect `{value}`; expected \"none\", \"idempotent\" or \
                 \"irreversible\" (docs/13 §permission engine)"
            ),
        ));
    }
    args.side_effects = value;
    Ok(args)
}

#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = match parse_args(attr) {
        Ok(a) => a,
        Err(e) => return e.to_compile_error().into(),
    };
    let func = parse_macro_input!(item as ItemFn);
    match expand(args, func) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(args: Args, func: ItemFn) -> Result<proc_macro2::TokenStream, syn::Error> {
    let sig = &func.sig;

    if sig.asyncness.is_none() {
        return Err(syn::Error::new(
            sig.fn_token.span(),
            "a tool must be `async fn`: the loop awaits it, and a blocking tool would \
             stall every other session on its runtime thread",
        ));
    }
    if !sig.generics.params.is_empty() {
        return Err(syn::Error::new(
            sig.generics.span(),
            "a tool cannot be generic: its arguments become one JSON Schema, and a \
             type parameter has no schema until it is chosen",
        ));
    }
    if matches!(sig.output, ReturnType::Default) {
        return Err(syn::Error::new(
            sig.span(),
            "a tool must return `Result<T, E>`: the model needs to be told when a call \
             failed, and a `()` return cannot say so",
        ));
    }

    let mut inputs = sig.inputs.iter();
    let ctx = inputs.next().ok_or_else(|| {
        syn::Error::new(
            sig.paren_token.span.join(),
            "a tool's first parameter must be `ctx: &ToolCtx` — it carries the account, \
             session and turn every call is attributed to (docs/10 §CallMeta)",
        )
    })?;
    if let FnArg::Receiver(r) = ctx {
        return Err(syn::Error::new(
            r.span(),
            "a tool is a free function, not a method: `self` has no place in a schema, \
             and the registry holds one instance per tool",
        ));
    }
    if !is_tool_ctx(ctx) {
        return Err(syn::Error::new(
            ctx.span(),
            "a tool's first parameter must be `ctx: &ToolCtx`",
        ));
    }

    // Every remaining parameter is a field of the arguments struct, which is what
    // becomes the JSON Schema.
    let mut fields = Vec::new();
    let mut field_names = Vec::new();
    for arg in inputs {
        let FnArg::Typed(PatType { pat, ty, .. }) = arg else {
            return Err(syn::Error::new(arg.span(), "unsupported parameter"));
        };
        let Pat::Ident(ident) = pat.as_ref() else {
            return Err(syn::Error::new(
                pat.span(),
                "a tool's parameters must be plain names: the name becomes the JSON \
                 field the model fills in, and a pattern has no name to use",
            ));
        };
        let name = &ident.ident;
        // A reference cannot be deserialized into an owned args struct, and the error
        // rustc would give for that is about lifetimes rather than about this rule.
        if let Type::Reference(r) = ty.as_ref() {
            return Err(syn::Error::new(
                r.span(),
                "a tool's arguments must be owned types: they are deserialized from \
                 JSON the model produced, and there is nothing for a reference to \
                 borrow from",
            ));
        }
        fields.push(quote! { pub #name: #ty });
        field_names.push(name.clone());
    }

    let fn_name = &sig.ident;
    let struct_name = format_ident!("{}", pascal_case(&fn_name.to_string()));
    let args_name = format_ident!("{}Args", struct_name);
    let tool_name = fn_name.to_string();
    let description = doc_comment(&func).unwrap_or_else(|| tool_name.clone());
    let side_effects = match args.side_effects.as_str() {
        "idempotent" => quote! { ::panday_harness::tools::SideEffects::Idempotent },
        "irreversible" => quote! { ::panday_harness::tools::SideEffects::Irreversible },
        _ => quote! { ::panday_harness::tools::SideEffects::None },
    };
    let replay = match args.side_effects.as_str() {
        "none" => quote! { ::panday_harness::tools::Replay::Safe },
        _ => quote! { ::panday_harness::tools::Replay::Unsafe },
    };

    Ok(quote! {
        #func

        #[doc = #description]
        #[derive(::std::fmt::Debug, ::std::clone::Clone, ::std::default::Default)]
        pub struct #struct_name;

        #[derive(::panday_harness::__private::serde::Deserialize, ::panday_harness::__private::schemars::JsonSchema)]
        #[serde(crate = "::panday_harness::__private::serde")]
        #[schemars(crate = "::panday_harness::__private::schemars")]
        #[serde(deny_unknown_fields)]
        #[doc = #description]
        pub struct #args_name {
            #(#fields),*
        }

        #[::panday_harness::__private::async_trait]
        impl ::panday_harness::tools::Tool for #struct_name {
            fn spec(&self) -> ::panday_harness::tools::ToolSpec {
                ::panday_harness::tools::ToolSpec {
                    name: #tool_name.to_string(),
                    description: #description.trim().to_string(),
                    // The schema is generated from the argument struct, so the
                    // parameter list and the schema cannot disagree.
                    parameters: ::panday_harness::__private::serde_json::to_value(
                        ::panday_harness::__private::schemars::schema_for!(#args_name)
                    ).unwrap_or_else(|_| ::panday_harness::__private::serde_json::json!({"type": "object"})),
                }
            }

            fn requirements(&self) -> ::panday_harness::tools::ToolReq {
                ::panday_harness::tools::ToolReq {
                    // In-process: this is a Rust function in the caller's own binary,
                    // which is exactly what docs/14 calls T0.
                    sandbox_tier: ::panday_harness::SandboxTier::T0InProcess,
                    side_effects: #side_effects,
                    independent: true,
                    replay: #replay,
                }
            }

            async fn call(
                &self,
                ctx: ::panday_harness::tools::ToolCtx,
                args: ::panday_harness::__private::Json,
            ) -> ::panday_harness::tools::ToolOutcome {
                // A bad argument is the model's mistake and it is recoverable: report
                // it as a tool error naming the problem, so the next turn can fix it.
                // `deny_unknown_fields` is deliberate — a misspelled argument that was
                // silently ignored would look like the tool disobeying.
                let parsed: #args_name = match ::panday_harness::__private::serde_json::from_value(args) {
                    Ok(v) => v,
                    Err(e) => {
                        return ::panday_harness::tools::ToolOutcome {
                            raw: format!("invalid arguments for `{}`: {}", #tool_name, e),
                            is_error: true,
                        }
                    }
                };
                match #fn_name(&ctx, #(parsed.#field_names),*).await {
                    Ok(value) => ::panday_harness::tools::ToolOutcome {
                        raw: ::panday_harness::__private::serde_json::to_string(&value)
                            .unwrap_or_else(|e| format!("unserializable result: {e}")),
                        is_error: false,
                    },
                    Err(e) => ::panday_harness::tools::ToolOutcome {
                        raw: format!("{e}"),
                        is_error: true,
                    },
                }
            }
        }
    })
}

/// `ctx: &ToolCtx` — checked by the last path segment so a caller may import it, or
/// write it out, or alias the module.
fn is_tool_ctx(arg: &FnArg) -> bool {
    let FnArg::Typed(PatType { ty, .. }) = arg else {
        return false;
    };
    let Type::Reference(r) = ty.as_ref() else {
        return false;
    };
    let Type::Path(path) = r.elem.as_ref() else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|s| s.ident == "ToolCtx")
}

fn doc_comment(func: &ItemFn) -> Option<String> {
    let lines: Vec<String> = func
        .attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .filter_map(|a| match &a.meta {
            syn::Meta::NameValue(nv) => match &nv.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(s),
                    ..
                }) => Some(s.value().trim().to_string()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn pascal_case(snake: &str) -> String {
    snake
        .split('_')
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}
