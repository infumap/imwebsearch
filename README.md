# IMWebSearch

Locally hosted meta web-search / web-fetch.

Use it for:
- HTTP MCP web-search/web-fetch tools for LLMs.
- Web search from your browser.
- Default browser navigation bar web search.

Simple to configure. Simple to operate.

Each web-search request is handled by **a single** provider, with automatic failover and cooldown. This methodology makes best use of scarce free web-search capacity. 

Four tiers:
1. High quality search providers with public access, no account/API key required (5 providers)
2. High quality search providers, free account / API key required (9 providers)
3. Lower quality search providers (1 provider)
4. Fallback to non-general but reliable/high volume search (wikipedia/hacker news aggregated).

A search request is handled by the highest tier with available capacity.

Tier 1 has enough capacity to be useful (no configuration required, maximally private).

Does not use any undocumented APIs or tricks to intentionally evade throttling/blocking.

**web-fetch MCP tool**

Extracts and returns page content, prepended with heading->offset information. offset/max_chars style API for efficient use of LLM context.

- Includes a Chrome extension to fetch pages via a real browser with user take-over capability (similar to how OpenAI Operator and other automation tools work). Workflow: give your query to the LLM. Watch your browser as your LLM fetches the pages it needs, and handle any login / "I am a human" checks etc. as they come up. Ideally, you should use a browser in a sandboxed environment for security reasons.
- Fallback to headless Chrome.


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
