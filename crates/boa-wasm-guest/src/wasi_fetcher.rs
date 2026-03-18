use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;

use boa_engine::{Context, JsData, JsError, JsResult, JsString, js_error};
use boa_gc::{Finalize, Trace};
use boa_runtime::fetch::Fetcher;
use boa_runtime::fetch::request::JsRequest;
use boa_runtime::fetch::response::JsResponse;

use wasi::http::outgoing_handler;
use wasi::http::types::{Fields, IncomingBody, OutgoingBody, OutgoingRequest, Scheme};

/// A [`Fetcher`] implementation that uses WASI HTTP (`wasi:http/outgoing-handler`)
/// to make HTTP requests from inside a WASM component.
#[derive(Debug, Clone, Trace, Finalize, JsData)]
pub struct WasiHttpFetcher;

impl Fetcher for WasiHttpFetcher {
    async fn fetch(
        self: Rc<Self>,
        request: JsRequest,
        _context: &RefCell<&mut Context>,
    ) -> JsResult<JsResponse> {
        let http_req = request.into_inner();
        let (parts, body) = http_req.into_parts();

        let url = parts.uri.to_string();

        // Build WASI outgoing request
        let headers = Fields::new();
        let mut has_user_agent = false;
        for (name, value) in &parts.headers {
            if name.as_str().eq_ignore_ascii_case("user-agent") {
                has_user_agent = true;
            }
            headers
                .append(&name.to_string(), &value.as_bytes().to_vec())
                .map_err(|e| js_error!(Error: "header error: {:?}", e))?;
        }
        if !has_user_agent {
            headers
                .append("user-agent", b"corevisor-cli")
                .map_err(|e| js_error!(Error: "header error: {:?}", e))?;
        }

        let outgoing = OutgoingRequest::new(headers);

        // Set method
        let method = match parts.method.as_str() {
            "GET" => wasi::http::types::Method::Get,
            "POST" => wasi::http::types::Method::Post,
            "PUT" => wasi::http::types::Method::Put,
            "DELETE" => wasi::http::types::Method::Delete,
            "HEAD" => wasi::http::types::Method::Head,
            "OPTIONS" => wasi::http::types::Method::Options,
            "PATCH" => wasi::http::types::Method::Patch,
            other => wasi::http::types::Method::Other(other.to_string()),
        };
        outgoing
            .set_method(&method)
            .map_err(|_| js_error!(Error: "failed to set method"))?;

        // Set scheme
        let scheme = match parts.uri.scheme_str() {
            Some("https") => Scheme::Https,
            Some("http") => Scheme::Http,
            Some(other) => Scheme::Other(other.to_string()),
            None => Scheme::Https,
        };
        outgoing
            .set_scheme(Some(&scheme))
            .map_err(|_| js_error!(Error: "failed to set scheme"))?;

        // Set authority (host + optional port)
        if let Some(authority) = parts.uri.authority() {
            outgoing
                .set_authority(Some(authority.as_str()))
                .map_err(|_| js_error!(Error: "failed to set authority"))?;
        }

        // Set path and query
        let path_and_query = parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");
        outgoing
            .set_path_with_query(Some(path_and_query))
            .map_err(|_| js_error!(Error: "failed to set path"))?;

        // Write body if present
        let outgoing_body = outgoing
            .body()
            .map_err(|_| js_error!(Error: "failed to get outgoing body"))?;
        if !body.is_empty() {
            let stream = outgoing_body
                .write()
                .map_err(|_| js_error!(Error: "failed to get body write stream"))?;
            stream
                .blocking_write_and_flush(&body)
                .map_err(|e| JsError::from_rust(e))?;
            drop(stream);
        }
        OutgoingBody::finish(outgoing_body, None)
            .map_err(|e| js_error!(Error: "finish body error: {:?}", e))?;

        // Send the request
        let future_response = outgoing_handler::handle(outgoing, None)
            .map_err(|e| js_error!(Error: "outgoing handler error: {:?}", e))?;

        // Block until response arrives
        let pollable = future_response.subscribe();
        pollable.block();

        let incoming_response = future_response
            .get()
            .ok_or_else(|| js_error!(Error: "response not ready after blocking"))?
            .map_err(|_| js_error!(Error: "response already consumed"))?
            .map_err(|e| js_error!(Error: "HTTP error: {:?}", e))?;

        let status = incoming_response.status();
        let resp_headers = incoming_response.headers();

        // Build http::Response
        let mut builder = http::Response::builder().status(status);

        let entries = resp_headers.entries();
        for (name, value) in &entries {
            builder = builder.header(name.as_str(), value.as_slice());
        }

        // Read body
        let incoming_body = incoming_response
            .consume()
            .map_err(|_| js_error!(Error: "failed to consume response body"))?;
        let mut body_stream = incoming_body
            .stream()
            .map_err(|_| js_error!(Error: "failed to get body stream"))?;

        let mut response_bytes = Vec::new();
        body_stream
            .read_to_end(&mut response_bytes)
            .map_err(|e| JsError::from_rust(e))?;

        drop(body_stream);
        let _ = IncomingBody::finish(incoming_body);

        let http_response = builder
            .body(response_bytes)
            .map_err(|e| JsError::from_rust(e))?;

        Ok(JsResponse::basic(JsString::from(url), http_response))
    }
}
