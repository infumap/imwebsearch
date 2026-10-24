# Infumap integration

Sidecar MCP servers spoken over Streamable HTTP.

Currently:

- **IMWebSearch** (executable `imwebsearch`) — web search and page fetch through separate endpoints in one process

On first startup, the binary creates `~/.imwebsearch/config.toml` from its embedded
example with owner-only permissions. Edit it to enable providers and enter API keys. Monthly free allowances
and assumed daily routing weights are fixed in provider code; local minute
limits remain TOML settings. Routing targets are soft and do not block requests
when exceeded.
Existing installations must preserve their accounting files under
`<data_dir>/state/`.
Validate and start from the repository root:

```bash
./run.sh --check-config
./run.sh
```

`--config <path>` selects another file for any command. All sidecar settings,
including credentials and logging, come from that file;
environment configuration is not supported. See the
[configuration example](config.example.toml).

General search supports You.com, Exa MCP, Tavily MCP,
Firecrawl MCP, Exa, Tavily, Diffbot, Firecrawl and Brave. General web providers run first. If none returns
results, enabled Wikipedia and Hacker News fallback sources run in parallel
with the original query. Their results are interleaved and deduplicated without
classification or judging. See the
[routing flow](README.md#how-a-query-is-routed).
Reset rules are fixed provider behavior, with
conservative accounting where timing is unknown. See
[provider configuration example](config.example.toml) and the
verification guides alongside the [provider implementations](src/providers/).

Tavily MCP is a separate, opt-in keyless provider. Set
`providers.tavily_mcp.max_requests_per_minute` to a positive local rate to enable
it; omitted or zero disables it. It calls `https://mcp.tavily.com/mcp/` with
`X-Tavily-Access-Mode: keyless` automatically and uses no account credentials.
Tavily does not publish the keyless cap or reset timing, so this provider has
local pacing and persistent rate/quota cooldowns rather than a monthly credit
allowance. The existing `providers.tavily.api_key` configures authenticated REST
search independently.

Firecrawl MCP is also available independently of the authenticated Firecrawl
REST provider. Set `providers.firecrawl_mcp.max_requests_per_minute` to a positive
local rate to enable keyless search at `https://mcp.firecrawl.dev/v2/mcp`;
omitted or zero disables it. Its soft routing target is fixed at 100/day in
provider code. Persistent cooldowns handle the unpublished keyless request and
credit caps; no REST API key or monthly account credits are used.

`run.sh` resolves Chrome/Chromium or installs official Chrome for Testing in the
`<data_dir>/cache/browsers/`, once per wrapper invocation. Set `browser.path` for an
explicit executable; use `./run.sh --update-browser` for a
deliberate managed-browser update. It never installs global packages or weakens
Chrome's sandbox. Linux additionally requires OS network confinement; native
macOS uses proxy and URL checks without an outer OS network sandbox.
Setup failures leave search usable.

Browser fetches reuse one owned browser with a fresh isolated context per request.
PDFs require `fetch.gpu_tools_url` pointing to an updated GPU service/gateway
advertising native-only extraction and absolute request deadlines. No PDF fetch
performs OCR. Full results are cached privately in `<data_dir>/cache/pdf-native-v1/`
before `maxChars` is applied. Top-level `data_dir` selects the single root for all
service-generated files; relative paths resolve against the TOML file.
The config must be named `config.toml` and live directly inside `data_dir`.
The default is `~/.imwebsearch`.
See [fetch operations](FETCHING.md).

It listens on `127.0.0.1:8791` by default. Point Infumap at its two endpoints with:

```toml
chat_tool_servers = [
  { id = "web_search", url = "http://127.0.0.1:8791/mcp/search" },
  { id = "web_fetch", url = "http://127.0.0.1:8791/mcp/fetch", timeout_secs = 330 },
]
```

The example gives fetch 330 seconds at the MCP layer; configure
`fetch.timeout_secs = 300` when allowing native PDF conversion that budget.
The fetch default remains 30 seconds.

`/mcp/search` exposes only `web_search`; `/mcp/fetch` exposes only
`web_fetch`. Each endpoint rejects calls to the other tool. Enable either
source independently in Infumap, omit an entry to use only the other, or replace
either entry with another MCP server. There is no combined `/mcp` endpoint.

Deep research only receives MCP tools whose tool definition declares
`annotations.readOnlyHint: true`. Mark discovery, lookup, and fetch tools with
that annotation so they are available to research; mutation tools remain
available to ordinary chat under the configured approval policy.

Profiles select **Automatic** or **Always ask** tool approvals for both Chat and Deep research.
Automatic asks before external calls when Infumap tools are enabled or the chat contains
attachments; otherwise calls run automatically. Approval uses the current Infumap setting without
tracking earlier retrieval after tools are disabled; retained attachments still require approval.
All MCP sources default to `external_requests = true`, including the localhost web sidecar.
Only trusted internal sources should set it to `false`. Tool annotations
cannot change this classification. Set `require_approval = true` on a source to always require
approval, regardless of the profile; omitted or false values follow the profile's rules.

Infumap does not start this process. If the sidecar is down, the `web_search` and
`web_fetch` sources are unavailable. There is no in-process fallback.

A musl release binary can be built with `./build-musl.sh` from this project.
Infumap builds and audits run separately from IMWebSearch.
