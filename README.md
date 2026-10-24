# IMWebSearch

Locally hosted meta web-search / web-fetch.

Simple to configure. Simple to operate.

Each web-search request is handled by **a single** provider, with automatic failover and cooldown. This methodology makes best use of scarce free web-search capacity. 

Four tiers:
1. High quality search providers with public access, no account/API key required (5 providers)
2. High quality search providers, free account / API key required (9 providers)
3. Lower quality search providers (1 provider)
4. Fallback to non-general but reliable/high volume search (wikipedia/hacker news aggregated).

A search is handled by the highest tier with available capacity.

You can get a long way with just tier 1 (no configuration required, maximally private).

Use it for:
- Web search from your browser.
- Default browser navigation bar web search.
- LLM MCP web-search tool.

Does not use any undocumented API or tricks to intentionally evade throttling/blocking.

The web-fetch MCP tool for LLMs.

Extracts and returns page content, prepended with heading->offset information & maxChars/offset style API to optimize LLM context usage.

- Includes a Chrome extension to fetch pages via a real browser with user take-over capability (similar to how OpenAI Operator and other automation tools work). Workflow: give your query to the LLM. Watch your browser as your LLM fetches the pages it needs, and handle any login / "I am a human" checks etc. as they come up. Ideally, you should use a browser in a sandboxed environment for security reasons.
- Fallback to headless Chrome.


## Running

You need [rust installed](https://rust-lang.org/tools/install/).

Then:

```bash
./run.sh
```

Builds imwebsearch, downloads Chrome if needed (a large download), and starts the server with automatic restarts on error.

Default configuration is created in `~/.imwebsearch/config.toml`.

Edit this to your requirements as per comments in the file.

