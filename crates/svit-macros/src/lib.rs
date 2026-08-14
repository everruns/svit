//! Implementation details for Svit's public script macros.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use ketos::module::NullModuleLoader;
use ketos::{Builder, GlobalIo, Interpreter, RestrictConfig};
use proc_macro::{TokenStream, TokenTree};
use syn::{LitStr, parse_macro_input};

#[proc_macro]
pub fn compile_svit_script(input: TokenStream) -> TokenStream {
    let source = match direct_source(input) {
        Ok(source) => source,
        Err(error) => return rust_compile_error(&error),
    };
    match compile_source(&source) {
        Ok(()) => format!("{source:?}")
            .parse()
            .expect("debug-formatted text is a valid Rust string literal"),
        Err(error) => rust_compile_error(&format!("failed to compile direct Svit script: {error}")),
    }
}

#[proc_macro]
pub fn validate_svit_script(input: TokenStream) -> TokenStream {
    let source = parse_macro_input!(input as LitStr);
    validation_result(&source.value(), "inline Svit script", &source)
}

#[proc_macro]
pub fn validate_svit_script_file(input: TokenStream) -> TokenStream {
    let path = parse_macro_input!(input as LitStr);
    let relative_path = PathBuf::from(path.value());
    if relative_path.is_absolute() {
        return compile_error(&path, "Svit script paths must be package-relative");
    }

    let manifest_dir = match env::var_os("CARGO_MANIFEST_DIR") {
        Some(value) => PathBuf::from(value),
        None => return compile_error(&path, "CARGO_MANIFEST_DIR is unavailable"),
    };
    let source = match fs::read_to_string(manifest_dir.join(&relative_path)) {
        Ok(source) => source,
        Err(error) => {
            return compile_error(
                &path,
                &format!(
                    "cannot read Svit script {}: {error}",
                    display_path(&relative_path)
                ),
            );
        }
    };

    validation_result(&source, &display_path(&relative_path), &path)
}

fn validation_result(source: &str, label: &str, literal: &LitStr) -> TokenStream {
    match compile_source(source) {
        Ok(()) => "()".parse().expect("unit expression is valid Rust"),
        Err(error) => syn::Error::new(
            literal.span(),
            format!("failed to compile {label}: {error}"),
        )
        .into_compile_error()
        .into(),
    }
}

fn compile_error(literal: &LitStr, message: &str) -> TokenStream {
    syn::Error::new(literal.span(), message)
        .into_compile_error()
        .into()
}

fn rust_compile_error(message: &str) -> TokenStream {
    format!("compile_error!({message:?})")
        .parse()
        .expect("debug-formatted text is valid in compile_error!")
}

fn direct_source(input: TokenStream) -> Result<String, String> {
    let mut source = String::new();
    for token in input {
        if !source.is_empty() {
            source.push('\n');
        }
        // TokenStream::to_string inserts spaces around punctuation and would
        // silently change Lisp names such as `value-map`. Call-site source
        // spans preserve the program exactly, including Lisp punctuation.
        let text = token_source(&token).ok_or_else(|| {
            "direct source is unavailable; use the string or file form from generated Rust code"
                .to_owned()
        })?;
        source.push_str(&text);
    }
    if source.is_empty() {
        return Err("direct Svit script is empty".into());
    }
    Ok(source)
}

fn token_source(token: &TokenTree) -> Option<String> {
    token.span().source_text()
}

fn compile_source(source: &str) -> Result<(), String> {
    // This checks the compiler-independent source contract without running
    // guest top-level forms. Process construction remains the authoritative
    // validation boundary for host-configured resource limits.
    // THREAT[TM-ESC-004]: Ketos compilation may evaluate guest macros and
    // constants, so build-time checks receive no I/O or module authority.
    // THREAT[TM-DOS-011]: The build-time compiler uses the standard Svit limit
    // profile so hostile macro expansion fails closed instead of hanging cargo.
    let interpreter = build_interpreter();
    interpreter
        .compile_exprs(source)
        .map(|_| ())
        .map_err(|error| interpreter.format_error(&error))
}

fn build_interpreter() -> Interpreter {
    Builder::new()
        .name("svit-lisp-build")
        .restrict(RestrictConfig {
            execution_time: Some(Duration::from_millis(100)),
            call_stack_size: 64,
            value_stack_size: 4096,
            namespace_size: 256,
            memory_limit: 8 * 1024 * 1024,
            max_integer_size: 64,
            max_syntax_nesting: 64,
        })
        .io(Rc::new(GlobalIo::null()))
        .module_loader(Box::new(NullModuleLoader))
        .finish()
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::compile_source;

    #[test]
    fn accepts_compilable_script_source() {
        compile_source("(define (main input) input)").unwrap();
    }

    #[test]
    fn rejects_invalid_script_source() {
        let error = compile_source("(define (main input)").unwrap_err();
        assert!(error.contains("parse error"), "{error}");
    }

    #[test]
    // THREAT[TM-ESC-004]
    fn build_time_compiler_denies_modules() {
        let error = compile_source("(use random :all) (define (main input) true)").unwrap_err();
        assert!(error.contains("module not found"), "{error}");
    }

    #[test]
    // THREAT[TM-DOS-011]
    fn build_time_macro_expansion_is_bounded() {
        let error = compile_source("(macro (spin) (spin)) (spin)").unwrap_err();
        assert!(
            error.contains("execution time exceeded") || error.contains("call stack exceeded"),
            "{error}"
        );
    }
}
