# IMWebSearch

Locally hosted meta web-search / web-fetch.

Simple to configure. Simple to operate.

Each web-search request is handled by **a single** provider, with automatic failover and cooldown. This strategy makes best use of scarce free web-search capacity. 

Four tiers:
1. High quality search providers with public access, no account/API key required (3 providers: [you](https://you.com), [exa](https://exa.ai), [parallel](https://parallel.ai))
2. High quality search providers, free account / API key required (9 providers: [you](https://you.com), [exa](https://exa.ai), [parallel](https://parallel.ai), [brave](https://search.brave.com), [brightdata](https://brightdata.com), [diffbot](https://diffbot.com), [firecrawl](https://firecrawl.dev), [serpapi](https://serpapi.com), [tavily](https://tavily.com))
3. Lower quality search providers (1 provider: [purili](https://puri.li))
4. Fallback to non-general but reliable/high volume search ([wikipedia]()/[hacker news](https://news.ycombinator.com) aggregated).

A search request is handled by the highest tier with available capacity.

Tier 1 has enough capacity to be useful (no configuration required, maximally private).

Use it for:
- HTTP MCP web-search/web-fetch tools for LLMs.
- Web search from your browser.
- Default browser navigation bar web search.

web-search does not use any undocumented APIs or tricks to intentionally evade throttling/blocking.

web-fetch extracts and returns page content (i.e. no HTML mark-up), prepended with heading:offset information and the API has offset/max_chars parameters. This strategy makes efficient use of valuable LLM context.

Includes a Chrome extension to fetch pages via a real browser with user take-over capability (similar to how OpenAI Operator and other automation tools work). Fallback to headless Chrome.

Connect to Infumap's [pdf text extraction](https://github.com/infumap/infumap/tree/master/tools/gpu) tool (or gpu tool gateway) to enable text extraction from pdfs.

## Running

Make sure you have [rust installed](https://rust-lang.org/tools/install/).

Then:

```bash
./run.sh
```

Builds imwebsearch, downloads Chrome if needed (a large download), and starts the server with automatic restarts on error.

Default configuration is created in `~/.imwebsearch/config.toml`.

Edit this to your requirements as per comments in the file.


## Usage

Open [http://127.0.0.1:8791/](http://127.0.0.1:8791/) to search.

[Status](http://127.0.0.1:8791/web/status) shows provider availability, quotas and browser relay status.

Chrome extension (optional, to fetch pages through your browser and handle verification challenges):

1. Set `browser_relay.token` in `config.toml` to a random secret of at least 32 characters (`openssl rand -hex 32`). Restart the server.
2. In a dedicated Chrome profile, open `chrome://extensions`, enable **Developer mode**, click **Load unpacked** and select this repo's `extension/` directory.
3. Open the extension popup. Enter `ws://127.0.0.1:8791/browser-relay` and the same token, tick **Connect**, then **Save**.

See [extension details](extension/README.md).

MCP endpoints:

- Web search: `http://127.0.0.1:8791/mcp/search`
- Web fetch: `http://127.0.0.1:8791/mcp/fetch`

Add either or both as Streamable HTTP MCP servers in [Infumap](https://github.com/infumap/infumap) or
your harness of choice.
