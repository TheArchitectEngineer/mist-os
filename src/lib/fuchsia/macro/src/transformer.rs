// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_diagnostics::Severity;
use proc_macro2::TokenStream;
use quote::{quote, quote_spanned, TokenStreamExt};
use syn::parse::{Parse, ParseStream, Parser};
use syn::{Attribute, Block, Error, Expr, ItemFn, LitBool, LitStr, Signature, Token, Visibility};

#[derive(Clone, Copy)]
enum FunctionType {
    Component,
    Test,
}

// How should code be executed?
#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
enum Executor {
    // Directly by calling it
    None { thread_role: Option<Expr> },
    // fasync::run_singlethreaded
    Singlethreaded { thread_role: Option<Expr> },
    // fasync::run
    Multithreaded { threads: Expr, thread_role: Option<Expr> },
    // #[test]
    Test,
    // fasync::run_singlethreaded(test)
    SinglethreadedTest,
    // fasync::run(test)
    MultithreadedTest { threads: Expr },
    // fasync::run_until_stalled
    UntilStalledTest,
}

impl Executor {
    fn is_test(&self) -> bool {
        match self {
            Executor::Test
            | Executor::SinglethreadedTest
            | Executor::MultithreadedTest { .. }
            | Executor::UntilStalledTest => true,
            Executor::None { .. }
            | Executor::Singlethreaded { .. }
            | Executor::Multithreaded { .. } => false,
        }
    }

    fn is_some(&self) -> bool {
        !matches!(self, Executor::Test | Executor::None { .. })
    }
}

impl quote::ToTokens for Executor {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        tokens.extend(match self {
            Executor::None { thread_role } => {
                if let Some(role) = thread_role {
                    quote! { ::fuchsia::main_not_async_with_role(func, #role) }
                } else {
                    quote! { ::fuchsia::main_not_async(func) }
                }
            }
            Executor::Test => quote! { ::fuchsia::test_not_async(func) },
            Executor::Singlethreaded { thread_role } => {
                if let Some(role) = thread_role {
                    quote! { ::fuchsia::main_singlethreaded_with_role(func, #role) }
                } else {
                    quote! { ::fuchsia::main_singlethreaded(func) }
                }
            }
            Executor::Multithreaded { threads, thread_role } => {
                if let Some(role) = thread_role {
                    quote! { ::fuchsia::main_multithreaded_with_role(func, #threads, #role) }
                } else {
                    quote! { ::fuchsia::main_multithreaded(func, #threads) }
                }
            }
            Executor::SinglethreadedTest => quote! { ::fuchsia::test_singlethreaded(func) },
            Executor::MultithreadedTest { threads } => {
                quote! { ::fuchsia::test_multithreaded(func, #threads) }
            }
            Executor::UntilStalledTest => quote! { ::fuchsia::test_until_stalled(func) },
        })
    }
}

// Helper trait for things that can generate the final token stream
trait Finish {
    fn finish(self) -> TokenStream
    where
        Self: Sized;
}

pub struct Transformer {
    executor: Executor,
    attrs: Vec<Attribute>,
    vis: Visibility,
    sig: Signature,
    block: Box<Block>,
    logging: bool,
    logging_blocking: bool,
    logging_tags: LoggingTags,
    logging_include_file_line: bool,
    panic_prefix: LitStr,
    interest: Interest,
    add_test_attr: bool,
}

struct Args {
    threads: Option<Expr>,
    thread_role: Option<Expr>,
    allow_stalls: Option<bool>,
    logging: bool,
    logging_blocking: bool,
    logging_tags: LoggingTags,
    logging_include_file_line: bool,
    interest: Interest,
    panic_prefix: Option<LitStr>,
    add_test_attr: bool,
}

#[derive(Default)]
struct LoggingTags {
    tags: Vec<String>,
}

impl quote::ToTokens for LoggingTags {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        for tag in &self.tags {
            tag.as_str().to_tokens(tokens);
            tokens.append(proc_macro2::Punct::new(',', proc_macro2::Spacing::Alone));
        }
    }
}

impl Parse for LoggingTags {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut tags = vec![];
        while !input.is_empty() {
            tags.push(input.parse::<LitStr>()?.value());
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }
        Ok(Self { tags })
    }
}

#[derive(Default)]
struct Interest {
    min_severity: Option<Severity>,
}

impl Parse for Interest {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let str_token = input.parse::<LitStr>()?;
        let min_severity = match str_token.value().to_lowercase().as_str() {
            "trace" => Severity::Trace,
            "debug" => Severity::Debug,
            "info" => Severity::Info,
            "warn" => Severity::Warn,
            "error" => Severity::Error,
            "fatal" => Severity::Fatal,
            other => {
                return Err(syn::Error::new(
                    str_token.span(),
                    format!("invalid severity: {}", other),
                ))
            }
        };
        Ok(Interest { min_severity: Some(min_severity) })
    }
}

impl quote::ToTokens for Interest {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        tokens.extend(match self.min_severity {
            None => quote! { ::fuchsia::Interest::default() },
            Some(severity) => {
                let severity_tok = match severity {
                    Severity::Trace => quote!(::fuchsia::Severity::Trace),
                    Severity::Debug => quote!(::fuchsia::Severity::Debug),
                    Severity::Info => quote!(::fuchsia::Severity::Info),
                    Severity::Warn => quote!(::fuchsia::Severity::Warn),
                    Severity::Error => quote!(::fuchsia::Severity::Error),
                    Severity::Fatal => quote!(::fuchsia::Severity::Fatal),
                };
                quote! {
                    ::fuchsia::Interest {
                        min_severity: Some(#severity_tok),
                        ..Default::default()
                    }
                }
            }
        });
    }
}

fn get_arg<T: Parse>(p: &ParseStream<'_>) -> syn::Result<T> {
    p.parse::<Token![=]>()?;
    p.parse()
}

fn get_bool_arg(p: &ParseStream<'_>, if_present: bool) -> syn::Result<bool> {
    if p.peek(Token![=]) {
        Ok(get_arg::<LitBool>(p)?.value)
    } else {
        Ok(if_present)
    }
}

fn get_logging_tags(p: &ParseStream<'_>) -> syn::Result<LoggingTags> {
    p.parse::<Token![=]>()?;
    let content;
    syn::bracketed!(content in p);
    let logging_tags = content.parse::<LoggingTags>()?;
    Ok(logging_tags)
}

fn get_interest_arg(input: &ParseStream<'_>) -> syn::Result<Interest> {
    input.parse::<Token![=]>()?;
    input.parse::<Interest>()
}

impl Args {
    fn parse(input: TokenStream) -> syn::Result<Self> {
        let mut args = Self {
            threads: None,
            thread_role: None,
            allow_stalls: None,
            logging: true,
            logging_blocking: false,
            logging_tags: LoggingTags::default(),
            logging_include_file_line: false,
            panic_prefix: None,
            interest: Interest::default(),
            add_test_attr: true,
        };

        let arg_parser = syn::meta::parser(|meta| {
            let ident =
                meta.path.get_ident().ok_or_else(|| meta.error("arguments must have a key"))?;
            match ident.to_string().as_ref() {
                "threads" => args.threads = Some(get_arg::<Expr>(&meta.input)?),
                "thread_role" => args.thread_role = Some(get_arg::<Expr>(&meta.input)?),
                "allow_stalls" => args.allow_stalls = Some(get_bool_arg(&meta.input, true)?),
                "logging" => args.logging = get_bool_arg(&meta.input, true)?,
                "logging_blocking" => args.logging_blocking = get_bool_arg(&meta.input, true)?,
                "logging_tags" => args.logging_tags = get_logging_tags(&meta.input)?,
                "always_log_file_line" => {
                    args.logging_include_file_line = get_bool_arg(&meta.input, true)?
                }
                "logging_minimum_severity" => args.interest = get_interest_arg(&meta.input)?,
                "logging_panic_prefix" => args.panic_prefix = Some(get_arg(&meta.input)?),
                "add_test_attr" => args.add_test_attr = get_bool_arg(&meta.input, true)?,
                _ => return Err(meta.error("unrecognized argument")),
            }

            Ok(())
        });

        arg_parser.parse2(input)?;
        Ok(args)
    }
}

impl Transformer {
    pub fn parse_main(args: TokenStream, input: TokenStream) -> Result<Self, Error> {
        Self::parse(FunctionType::Component, args, input)
    }

    pub fn parse_test(args: TokenStream, input: TokenStream) -> Result<Self, Error> {
        Self::parse(FunctionType::Test, args, input)
    }

    pub fn finish(self) -> TokenStream {
        Finish::finish(self)
    }

    // Construct a new Transformer, verifying correctness.
    fn parse(
        function_type: FunctionType,
        args: TokenStream,
        input: TokenStream,
    ) -> Result<Transformer, Error> {
        let args = Args::parse(args)?;
        let ItemFn { attrs, vis, sig, block } = syn::parse2(input)?;
        let is_async = sig.asyncness.is_some();

        let err = |message| Err(Error::new(sig.ident.span(), message));

        let executor =
            match (args.threads, args.allow_stalls, args.thread_role, is_async, function_type) {
                (_, _, Some(_), _, FunctionType::Test) => {
                    return err("thread_role cannot be applied to tests")
                }
                (_, Some(_), _, _, FunctionType::Component) => {
                    return err("allow_stalls only applies to tests")
                }
                (None, _, thread_role, false, FunctionType::Component) => {
                    Executor::None { thread_role }
                }
                (None, None, thread_role, true, FunctionType::Component) => {
                    Executor::Singlethreaded { thread_role }
                }
                (Some(threads), None, thread_role, true, FunctionType::Component) => {
                    Executor::Multithreaded { threads, thread_role }
                }
                (None, Some(_), _, false, FunctionType::Test) => {
                    return err("allow_stalls only applies to async tests")
                }
                (None, None, _, false, FunctionType::Test) => Executor::Test,
                (None, Some(true) | None, _, true, FunctionType::Test) => {
                    Executor::SinglethreadedTest
                }
                (Some(threads), Some(true) | None, _, true, FunctionType::Test) => {
                    Executor::MultithreadedTest { threads }
                }
                (None, Some(false), _, true, FunctionType::Test) => Executor::UntilStalledTest,
                (_, Some(false), _, _, FunctionType::Test) => {
                    return err("allow_stalls=false tests must be single threaded")
                }
                (_, Some(true) | None, _, false, _) => {
                    return err("must be async to use >1 thread")
                }
            };

        let panic_prefix =
            args.panic_prefix.unwrap_or_else(|| LitStr::new("PANIC", sig.ident.span()));
        Ok(Transformer {
            executor,
            attrs,
            vis,
            sig,
            block,
            logging: args.logging,
            logging_blocking: args.logging_blocking,
            logging_tags: args.logging_tags,
            logging_include_file_line: args.logging_include_file_line,
            panic_prefix,
            interest: args.interest,
            add_test_attr: args.add_test_attr,
        })
    }
}

impl Finish for Transformer {
    // Build the transformed code, knowing that everything is ok because we proved that in parse.
    fn finish(self) -> TokenStream {
        let ident = self.sig.ident;
        let span = ident.span();
        let ret_type = self.sig.output;
        let attrs = self.attrs;
        let visibility = self.vis;
        let asyncness = self.sig.asyncness;
        let block = self.block;
        let inputs = self.sig.inputs;
        let logging_blocking = self.logging_blocking;
        let always_log_file_line = self.logging_include_file_line;
        let mut logging_tags = self.logging_tags;
        let panic_prefix = self.panic_prefix;
        let interest = self.interest;

        let mut func_attrs = Vec::new();

        let should_panic = attrs.iter().any(|attr| {
            attr.path().segments.len() == 1 && attr.path().segments[0].ident == "should_panic"
        });
        let maybe_disable_lsan = if should_panic {
            quote! { ::fuchsia::disable_lsan_for_should_panic(); }
        } else {
            quote! {}
        };

        // Initialize logging
        let init_logging = if !self.logging {
            quote! { func }
        } else if self.executor.is_test() {
            logging_tags.tags.insert(0, format!("{ident}"));
            let logging_options = quote! {
                ::fuchsia::LoggingOptions {
                    blocking: #logging_blocking,
                    interest: #interest,
                    always_log_file_line: #always_log_file_line,
                    tags: &[#logging_tags],
                    panic_prefix: #panic_prefix,
                }
            };
            if self.executor.is_some() {
                quote!(::fuchsia::init_logging_for_test_with_executor(func, #logging_options))
            } else {
                quote!(::fuchsia::init_logging_for_test_with_threads(func, #logging_options))
            }
        } else {
            let logging_options = quote! {
                ::fuchsia::LoggingOptions {
                    blocking: #logging_blocking,
                    interest: #interest,
                    always_log_file_line: #always_log_file_line,
                    tags: &[#logging_tags],
                    panic_prefix: #panic_prefix,
                }
            };
            if self.executor.is_some() {
                quote!(::fuchsia::init_logging_for_component_with_executor(func, #logging_options))
            } else {
                quote!(::fuchsia::init_logging_for_component_with_threads(func, #logging_options))
            }
        };

        if self.executor.is_test() && self.add_test_attr {
            // Add test attribute to outer function.
            func_attrs.push(quote!(#[test]));
        }

        let func = if self.executor.is_test() {
            quote! { test_entry_point }
        } else {
            quote! { component_entry_point }
        };

        // Adapt the runner function based on whether it's a test and argument count
        // by providing needed arguments.
        let adapt_main = match (self.executor.is_test(), inputs.len()) {
            // Main function, no arguments - no adaption needed.
            (false, 0) => quote! { #func },
            // Main function, one arguemnt - adapt by parsing command line arguments.
            (false, 1) => quote! { ::fuchsia::adapt_to_parse_arguments(#func) },
            // Test function, no arguments - adapt by taking the run number and discarding it.
            (true, 0) => quote! { ::fuchsia::adapt_to_take_test_run_number(#func) },
            // Test function, one argument - no adaption needed.
            (true, 1) => quote! { #func },
            // Anything with more than one argument: error.
            (_, n) => panic!("Too many ({}) arguments to function", n),
        };
        let tokenized_executor = &self.executor;
        let is_nonempty_ret_type = !matches!(ret_type, syn::ReturnType::Default);

        // Select executor
        let (run_executor, modified_ret_type) =
            match (self.executor.is_test(), self.logging, is_nonempty_ret_type) {
                (_, true, false) | (_, false, false) => {
                    (quote!(#tokenized_executor), quote!(#ret_type))
                }
                (_, false, true) => (quote!(#tokenized_executor), quote!(#ret_type)),
                (true, _, _) => (quote!(#tokenized_executor), quote!(#ret_type)),
                (false, _, _) => (
                    quote! {
                        let result = #tokenized_executor;
                        match result {
                            std::result::Result::Ok(val) => {
                                use std::process::Termination;
                                val.report()
                            },
                            std::result::Result::Err(err) => {
                                ::fuchsia::error!("{err:?}");
                                std::process::ExitCode::FAILURE
                            }
                        }
                    },
                    quote!(-> std::process::ExitCode),
                ),
            };

        // Finally build output.
        let output = quote_spanned! {span =>
            #(#attrs)* #(#func_attrs)*
            #visibility fn #ident () #modified_ret_type {
                // Note: `ItemFn::block` includes the function body braces. Do
                // not add additional braces (will break source code coverage
                // analysis).
                // TODO(https://fxbug.dev/42157203): Try to improve the Rust compiler to
                // ease this restriction.
                #asyncness fn #func(#inputs) #ret_type #block
                #maybe_disable_lsan
                let func = #adapt_main;
                let func = #init_logging;
                #run_executor
            }
        };
        output.into()
    }
}

impl Finish for Error {
    fn finish(self) -> TokenStream {
        self.to_compile_error().into()
    }
}

impl<R: Finish, E: Finish> Finish for Result<R, E> {
    fn finish(self) -> TokenStream {
        match self {
            Ok(r) => r.finish(),
            Err(e) => e.finish(),
        }
    }
}
