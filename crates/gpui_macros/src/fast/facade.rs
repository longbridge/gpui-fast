//! Resolves the `gpui::` paths GPUI's macros emit through gpui-kit.
//!
//! GPUI's macros write `gpui::IntoElement`, `gpui::App` and so on, which only
//! resolve in a crate that depends on `gpui` itself. Applications built on
//! gpui-kit depend on the kit alone and reach GPUI through its re-export, so
//! when the calling crate depends on `gpui-kit` every `gpui::` (and
//! `gpui_platform::`) path head in the expansion is rewritten to the kit:
//! `::gpui_kit::` under the name the caller gave it, or `crate::` inside the
//! kit itself. Crates that don't depend on gpui-kit — GPUI, gpui-component —
//! get the expansion unchanged.
//!
//! This is the same rewrite the crates.io `gpui-pre-macros` snapshots carry,
//! so a gpui-kit application builds against either GPUI unchanged.

use proc_macro::{Group, Ident, Literal, Punct, Spacing, TokenStream, TokenTree};
use proc_macro_crate::{FoundCrate, crate_name};

enum Facade {
    Itself,
    Name(String),
}

pub(crate) fn rewrite(stream: TokenStream) -> TokenStream {
    let facade = match crate_name("gpui-kit") {
        Ok(FoundCrate::Name(name)) => Facade::Name(name),
        Ok(FoundCrate::Itself) => Facade::Itself,
        Err(_) => return stream,
    };
    rewrite_stream(stream, &facade)
}

fn rewrite_stream(stream: TokenStream, facade: &Facade) -> TokenStream {
    let tokens: Vec<_> = stream.into_iter().collect();
    let mut output = TokenStream::new();
    for (index, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Group(group) => {
                let mut rewritten =
                    Group::new(group.delimiter(), rewrite_stream(group.stream(), facade));
                rewritten.set_span(group.span());
                output.extend([TokenTree::Group(rewritten)]);
            }
            TokenTree::Ident(ident)
                if is_path_head(&tokens, index)
                    && matches!(ident.to_string().as_str(), "gpui" | "gpui_platform") =>
            {
                append_path_head(&mut output, ident, facade);
            }
            TokenTree::Literal(literal) => {
                output.extend([TokenTree::Literal(rewrite_literal(literal, facade))]);
            }
            token => output.extend([token.clone()]),
        }
    }
    output
}

fn is_path_head(tokens: &[TokenTree], index: usize) -> bool {
    matches!(tokens.get(index + 1), Some(TokenTree::Punct(first)) if first.as_char() == ':')
        && matches!(tokens.get(index + 2), Some(TokenTree::Punct(second)) if second.as_char() == ':')
}

/// Writes the kit in place of `original`; `gpui_platform` becomes the kit's
/// `platform` module.
fn append_path_head(output: &mut TokenStream, original: &Ident, facade: &Facade) {
    let name = match facade {
        Facade::Itself => "crate",
        Facade::Name(name) => name,
    };
    output.extend([TokenTree::Ident(Ident::new(name, original.span()))]);
    if original.to_string() == "gpui_platform" {
        output.extend([
            TokenTree::Punct(Punct::new(':', Spacing::Joint)),
            TokenTree::Punct(Punct::new(':', Spacing::Alone)),
            TokenTree::Ident(Ident::new("platform", original.span())),
        ]);
    }
}

/// Rewrites `::gpui::` and `::gpui_platform::` paths spelled inside string
/// literals of the expansion.
fn rewrite_literal(literal: &Literal, facade: &Facade) -> Literal {
    let (gpui, platform) = match facade {
        Facade::Itself => ("crate::".to_string(), "crate::platform::".to_string()),
        Facade::Name(name) => (format!("::{name}::"), format!("::{name}::platform::")),
    };
    let text = literal.to_string();
    let rewritten = text
        .replace("::gpui_platform::", &platform)
        .replace("::gpui::", &gpui);
    if rewritten == text {
        return literal.clone();
    }
    let Ok(parsed) = rewritten.parse::<TokenStream>() else {
        return literal.clone();
    };
    let mut tokens = parsed.into_iter();
    match (tokens.next(), tokens.next()) {
        (Some(TokenTree::Literal(literal)), None) => literal,
        _ => literal.clone(),
    }
}
