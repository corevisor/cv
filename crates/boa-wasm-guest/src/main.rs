mod wasi_fetcher;

use std::io::{self, Read, Write};

use boa_engine::builtins::promise::PromiseState;
use boa_engine::object::builtins::JsPromise;
use boa_engine::{Context, Source};
use boa_runtime::extensions::{ConsoleExtension, FetchExtension};

use wasi_fetcher::WasiHttpFetcher;

const HELPERS: &str = r#"
async function get(url, headers) {
  const resp = await fetch(url, { headers: headers || {} });
  return resp.json();
}
async function post(url, body, headers) {
  const resp = await fetch(url, {
    method: "POST",
    headers: Object.assign({ "Content-Type": "application/json" }, headers || {}),
    body: JSON.stringify(body),
  });
  return resp.json();
}
async function put(url, body, headers) {
  const resp = await fetch(url, {
    method: "PUT",
    headers: Object.assign({ "Content-Type": "application/json" }, headers || {}),
    body: JSON.stringify(body),
  });
  return resp.json();
}
async function patch(url, body, headers) {
  const resp = await fetch(url, {
    method: "PATCH",
    headers: Object.assign({ "Content-Type": "application/json" }, headers || {}),
    body: JSON.stringify(body),
  });
  return resp.json();
}
async function del(url, headers) {
  const resp = await fetch(url, { method: "DELETE", headers: headers || {} });
  return resp.json();
}
"#;

fn main() {
    let mut code = String::new();
    io::stdin()
        .read_to_string(&mut code)
        .expect("failed to read JS from stdin");

    if code.is_empty() {
        eprintln!("error: no JavaScript code provided on stdin");
        std::process::exit(1);
    }

    let mut context = Context::default();

    boa_runtime::register(
        (ConsoleExtension::default(), FetchExtension(WasiHttpFetcher)),
        None,
        &mut context,
    )
    .expect("failed to register runtime extensions");

    // Register JS helper functions (get, post, put, patch, del)
    context
        .eval(Source::from_bytes(HELPERS))
        .expect("failed to evaluate JS helpers");

    // Wrap in an async IIFE so top-level `await` works (eval parses as Script, not Module).
    // The .then() auto-stringifies object return values so callers don't need JSON.stringify().
    let wrapped = format!(
        "(async () => {{\n{code}\n}})().then(__cv => typeof __cv === 'object' && __cv !== null ? JSON.stringify(__cv, null, 2) : __cv)"
    );

    match context.eval(Source::from_bytes(&wrapped)) {
        Ok(value) => {
            // Run any pending async jobs (promises from fetch, etc.)
            let _ = context.run_jobs();

            // If the result is a Promise, extract the resolved/rejected value
            let final_value = if value.is_object() {
                if let Ok(promise) = JsPromise::from_object(value.as_object().unwrap().clone()) {
                    match promise.state() {
                        PromiseState::Fulfilled(v) => v,
                        PromiseState::Rejected(e) => {
                            eprintln!("Promise rejected: {}", e.display());
                            std::process::exit(1);
                        }
                        PromiseState::Pending => {
                            eprintln!("Promise still pending after run_jobs()");
                            std::process::exit(1);
                        }
                    }
                } else {
                    value
                }
            } else {
                value
            };

            let result = final_value.to_string(&mut context);
            match result {
                Ok(s) => {
                    let output = s.to_std_string_escaped();
                    io::stdout()
                        .write_all(output.as_bytes())
                        .expect("failed to write to stdout");
                }
                Err(e) => {
                    eprintln!("error converting result to string: {e}");
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("JavaScript error: {e}");
            std::process::exit(1);
        }
    }
}
