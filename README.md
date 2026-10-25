# IMWebSearch

Locally hosted meta web-search / web-fetch.

Simple to configure. Simple to operate.

Each web-search request is handled by **a single** provider, with automatic failover and cooldown. This strategy makes the best use of scarce free web-search capacity.

Four tiers:
1. High-quality search with public access, no account/API key required (3 providers: [you](https://you.com), [exa](https://exa.ai), [parallel](https://parallel.ai))
2. High-quality search, free account / API key required (9 providers: [you](https://you.com), [exa](https://exa.ai), [parallel](https://parallel.ai), [brave](https://search.brave.com), [brightdata](https://brightdata.com), [diffbot](https://diffbot.com), [firecrawl](https://firecrawl.dev), [serpapi](https://serpapi.com), [tavily](https://tavily.com))
3. Lower-quality search (1 provider: [purili](https://puri.li))
4. Fall back to narrow but reliable/high-volume providers (1 provider: [hacker news](https://news.ycombinator.com))

A search request is handled by the highest tier with available capacity.

Tier 1 has enough capacity to be useful (no configuration required, maximally private).

Use it for:
- MCP (over HTTP) web search tool for your LLMs.
- Web search from your browser.
- Address bar web search.

Does not use any undocumented APIs or tricks to intentionally evade throttling/blocking.

Web-fetch extracts and returns page content as plain text (no HTML markup), prepended with heading:offset information. The MCP function has optional offset/max_chars parameters. This strategy makes efficient use of valuable LLM context.

Includes a Chrome extension to fetch pages via a real browser with user take-over capability for captchas/login (similar to how OpenAI Operator and other automation tools work). Falls back to headless Chrome.

Connect to infumap's [PDF text extraction](https://github.com/infumap/infumap/tree/master/tools/gpu) tool to enable text extraction from PDFs.

## Running

Make sure you have [Rust installed](https://rust-lang.org/tools/install/).

Then `./run.sh`

This builds `imwebsearch`, downloads Chrome if needed, and starts the server with automatic restarts on failure. Run in `tmux`, or set it up as a service.

A default configuration is created at `~/.imwebsearch/config.toml` on first run. Edit it to suit your requirements, following the comments in the file.

### Chrome Extension

Optional - for fetching pages through your browser and manual handling of verification challenges:

1. Set `browser_relay.token` in `config.toml` to a random secret of at least 32 characters (`openssl rand -hex 32`). Restart the server.
2. In a dedicated Chrome profile, open `chrome://extensions`, enable **Developer mode**, click **Load unpacked** and select this repo's `extension/` directory.
3. Open the extension popup. Enter `ws://127.0.0.1:8791/browser-relay` and the same token, tick **Connect**, then **Save**.

For security, we recommend using a sandboxed browser. [Omarchy](https://omarchy.org/), which comes with Chromium pre-installed, is a good option for this.

### GPU tools

Optional - for PDF text extraction:

1. Run infumap's [GPU tools](https://github.com/infumap/infumap/tree/master/tools/gpu) service.
2. Set `fetch.gpu_tools_url` in `config.toml`. Restart the server.

## Usage

Assuming default host and port config:

- Open `http://127.0.0.1:8791/` in your browser to search.
- Open `http://127.0.0.1:8791/web/status` in your browser for provider availability, quotas and browser relay status.
- To make it Chrome's default search engine: Settings → Search engine → Manage search engines and site search → Site search → Add, with URL `http://127.0.0.1:8791/search?q=%s`, then choose ⋮ → Make default.

MCP endpoints:

- Search and fetch together: `http://127.0.0.1:8791/mcp`
- Web search: `http://127.0.0.1:8791/mcp/search`
- Web fetch: `http://127.0.0.1:8791/mcp/fetch`

Add `/mcp` as one Streamable HTTP MCP server in [infumap](https://github.com/infumap/infumap) or your harness
of choice to discover both `web_search` and `web_fetch`. Alternatively, add either or both separate endpoints
to select search and fetch independently. The separate endpoints expose only their respective tool.

All three endpoints support MCP `2026-07-28`, including `server/discover`, standard per-request metadata and
HTTP header validation. Clients using the 2025 revisions can continue using `initialize`, `tools/list` and
`tools/call`. The server returns JSON responses without requiring SSE streams or session IDs.

`web_search` accepts `query` and an optional `maxResults` (1–30, default 5). Providers may return fewer
results. `web_fetch` accepts `url`, with optional `maxChars` and `offset` for reading part of a page.

Callers choose search effort through `maxResults`. The server does not interpret caller modes, models or
per-turn call counts, and does not add search reminders.
