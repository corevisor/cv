# Corevisor CLI

Local credential management and MCP server for AI assistants. Corevisor lets AI agents execute JavaScript against external APIs while keeping credentials on your machine and enforcing approval policies via a central Hub.

## How it works

`cv` runs as an [MCP](https://modelcontextprotocol.io/) server (stdio transport) that exposes tools to AI assistants:

- **execute_javascript** — Run JS code in a sandboxed WASM environment (Boa engine). Outbound HTTP requests to configured domains get credentials injected automatically.
- **list_services** — Show which API domains are available and whether credentials are set.
- **search_api_docs** — Search API documentation for configured services by regex pattern.

Every outbound request is checked against the Corevisor Hub's approval rules. Requests can be allowed, denied, or held for human approval before proceeding.

For more information, see [corevisor.xyz](https://corevisor.xyz).

## Usage

```sh
# Authenticate with a Hub
cv login

# Sync profiles and services from the Hub
cv sync

# Store a credential locally
cv credential set api.notion.com

# Start the MCP server
cv serve
```
