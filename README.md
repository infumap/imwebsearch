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

Does not use any tricks to intentionally evade throttling/blocking.


Also provides web-fetch MCP tool for LLMs.

Extracts and returns page content, prepended with heading->offset information & maxChars/offset style API to optimize LLM context usage.

- Includes a Chrome extension to fetch pages via a real browser with user take-over capability (similar to how OpenAI Operator and other automation tools work). Workflow: give your query to the LLM. Watch your browser as your LLM fetches the pages it needs, and handle any login / "I am a human" checks etc. as they come up. Ideally, you should use a browser in a sandboxed environment for security reasons.
- Fallback to headless Chrome.


## Running

You need [rust installed](https://rust-lang.org/tools/install/).

Then:

```bash
./run.sh
```

Default configuration is created in `~/.imwebsearch/config.toml`.

Edit as per comments in the config.


## Browser relay

Sites behind bot protection, most often Cloudflare, can refuse the headless
fetch browser. A desktop browser can register with the server. While it is
connected, it renders pages in place of the headless browser, and the user can
complete any verification challenge. See the
[extension README](extension/README.md).

## Browser interface

Open `http://127.0.0.1:8791/` after starting the service (or use the host and
port from your configuration). The interface is served by the same binary,
requires no frontend build or JavaScript, and supports a single page of up to
ten results. Searches use the existing provider routing, quota accounting,
failover and fallback sources. Titles open the original pages.

Each result also has a small **Fetch text** link. It opens `/fetch?url=…` in a
new tab and returns `text/plain; charset=utf-8`: exactly the JSON text payload
inside the MCP `web_fetch` result, without the JSON-RPC or MCP envelope. This
uses the same fetch pipeline, cache, errors, and default 8,000-character chunk
as MCP. Long pages include the usual truncation metadata; the endpoint also
accepts the MCP `offset` and `maxChars` parameters.

The **Status** link at the top right opens `/web/status`. Its overview shows
ready general-web providers, estimated searches remaining across tracked
allowances, 24-hour usage against soft routing targets, and remaining free
credit. Each enabled provider shows allowance usage, reservations, remaining
estimates, rolling-minute usage, pacing waits, and known retry/period timing.
Unknown quotas and reset times are explicit. Disabled providers are collapsed,
and fallback sources are shown separately and excluded from general-web totals.

Search equivalents use the reference request's result count and cost. Daily
estimates and account periods differ, so the remaining-search total is not a
combined daily quota or a guaranteed upstream balance. Soft routing targets
do not limit dispatch. Expand details for the underlying state and estimate
explanations. Refresh reads local state without issuing searches or fetching
pages. The status page and `/status` JSON endpoint accept any client that can
reach the server, like the search and fetch pages; cross-site browser requests
are rejected.

Remote rate/quota errors honor explicit retry hints, including values shorter
than a day. If both headers and an error payload supply hints, the longest
applies. Without a hint, the cooldown is 24 hours. Local request pacing and
other outstanding cooldowns still apply independently. Status and server logs
show bounded machine-readable rejection codes rather than raw upstream error
messages. The last rejection in status is recorded since server startup;
existing cooldown files contain no rejection code and are not rewritten by
this change. A restart preserves cooldown timing but clears that diagnostic.

# Web-search

Simple but highly effective. Easy to use. Easy to configure.

I built this because alternatives (SearXNG and ddgs) aggregate results, whereas economical
use of scarce free web-search capacity is the dominating priority for many.

Tavily keyless MCP search is available separately from the authenticated Tavily
REST provider. Enable it with a positive local request rate:

```toml
[providers.tavily_mcp]
max_requests_per_minute = 2
```

Omitted or zero disables it. The adapter sends the required
`X-Tavily-Access-Mode: keyless` header automatically. No API key is needed.
The upstream keyless cap and reset timing are undocumented; local pacing and
persistent cooldowns handle rate/quota rejections without assigning account
credits to this provider. See the [configuration example](config.example.toml)
and [verification guide](src/providers/tavily_mcp.verify.md).

Firecrawl keyless MCP search is available as a separate `firecrawl_mcp`
provider at `https://mcp.firecrawl.dev/v2/mcp`:

```toml
[providers.firecrawl_mcp]
max_requests_per_minute = 2
```

Omitted or zero disables it. It calls `firecrawl_search` for ordinary web results,
without credentials, page scraping, or follow-up tool execution. Firecrawl
[documents per-IP daily request and credit caps](https://docs.firecrawl.dev/rate-limits)
for keyless access but does not publish their numeric values or reset boundary.
The adapter uses a hardcoded soft routing estimate of 100 requests/day and
persistent rate/quota cooldowns. `providers.firecrawl.api_key` continues to
configure the independent authenticated REST provider. See the
[verification guide](src/providers/firecrawl_mcp.verify.md).

Parallel search has separate keyless MCP and authenticated REST providers:

```toml
[providers.parallel_mcp]
max_requests_per_minute = 2

[providers.parallel]
api_key = ""
```

The keyless provider calls `web_search` at `https://search.parallel.ai/mcp`.
Omitted or zero disables it; a blank API key disables the account provider.
Parallel [documents free anonymous search](https://docs.parallel.ai/integrations/mcp/search-mcp)
with unpublished limits. Result counts are controlled upstream and trimmed
locally to at most ten. Local pacing, a soft 100/day allocation target and
persistent rate/quota cooldowns govern its availability independently of account
credits.

The account provider calls `POST https://api.parallel.ai/v1/search` in Fast
mode, requesting up to ten results for $0.001/search. Accounts receive
[$5 in recurring monthly credits](https://parallel.ai/pricing), enough for up
to 5,000 such searches if those credits are not consumed elsewhere. Signup
grants are excluded from the local allowance estimate. Account balance and
credit-reset boundaries are unknown to the adapter; the existing budget
accounting and quota-recovery mechanism apply. These estimates are scheduling
weights; free-only billing must be enforced by the upstream account, as with
the other account providers. See the [REST](src/providers/parallel.verify.md)
and [MCP](src/providers/parallel_mcp.verify.md) verification guides.

Bright Data uses authenticated hosted MCP search under the provider name
`brightdata`:

```toml
[providers.brightdata]
api_key = ""
tier = 2
```

A blank key disables it. It calls `search_engine` with Google at
`https://mcp.brightdata.com/mcp`, passing the API token as an encoded query
parameter. One search is dispatched per attempt; results are deduplicated and
trimmed locally without pagination or scraping. Local pacing is five attempts
per minute with one concurrent request; these are local policies, not published
upstream limits.

Bright Data [documents 5,000 free requests/month](https://github.com/brightdata/brightdata-mcp#pricing-and-free-tier),
renewing on the first, shared across MCP tools and users on the account. The
budget ledger normalizes each search to $0.0015 and the allowance to $7.50 for
scheduling, using UTC as the local calendar convention. Upstream reset timezone
and account balance are unknown. Other account usage is invisible locally;
free-only billing depends on the upstream account settings, as with the other
account providers. See the [verification guide](src/providers/brightdata.verify.md).

SerpApi uses the authenticated [Google Search API](https://serpapi.com/search-api):

```toml
[providers.serpapi]
api_key = ""
tier = 2
```

A blank key disables it. Each attempt retrieves one synchronous result page,
normalizes organic links, titles and snippets, and trims locally to the requested
limit. Google controls the page size; no result-count parameter, pagination,
archive polling or retries are used. Keys are encoded in the required query
parameter and excluded from errors.

The [free plan](https://serpapi.com/) provides 250 searches/month with throughput
of 50/hour. The router enforces 50 attempts per rolling 66 minutes, including
10% timing headroom. The existing budget ledger normalizes a search to $0.025
and the allowance to $6.25 using Starter pricing, solely as scheduling estimates.
Failed searches are not billed; cached successes retain the conservative local
estimate. Account balances, usage by other clients and exact renewal timing are
unknown to the adapter. The shared persistent cooldown and recovery policy
applies; upstream account settings must enforce free-only usage. See the
[verification guide](src/providers/serpapi.verify.md).

Context.dev uses its authenticated [web search API](https://docs.context.dev/api-reference/web-scraping/search):

```toml
[providers.context]
api_key = ""
tier = 2
```

A blank key disables it. The [Free plan](https://www.context.dev/pricing) includes
1,000 recurring credits/month without a card or overage, shared across the
organization's APIs and keys. Search costs one credit per ten returned results,
rounded up. That covers about 1,000 searches/month at ten results each. Markdown,
highlights and query fan-out are disabled. The adapter requests at least ten
results to accommodate conflicting documented minimums, caps requests at 30,
and trims to the caller's limit.

The budget ledger reserves for the requested result count and settles using
reported request credits when available. It normalizes one credit to $0.0022
and the free allowance to $2.20 using Developer overage pricing, solely as
scheduling units. Local pacing is five attempts/minute; the Free plan's two
concurrent requests are enforced separately. Refills follow the account's grant
anchor, so no calendar reset is assumed. Quota errors, including HTTP 401
`USAGE_EXCEEDED`, use persistent cooldowns and honor supplied retry/refill timing.
See the [verification guide](src/providers/context.verify.md).

Purili uses the free [public Search API](https://puri.li/developer/documentation)
without credentials. It defaults to tier 3, after the other general-web tiers:

```toml
[providers.purili]
max_requests_per_minute = 2
tier = 3
```

Omitting the section or setting the rate to zero disables it. The example config
enables it at two requests per minute; this is local pacing policy. Each attempt
retrieves the first result page, disables query correction, maps `description`
to the snippet, and trims results locally. It does not fetch page context or
paginate. See the [verification guide](src/providers/purili.verify.md).

## How a query is routed

Each general-web provider has a positive integer `tier` in its TOML section.
An omitted `tier` defaults to 1 for keyless providers and 2 for API-key providers,
with Purili defaulting to 3. This also applies when the entire provider section
is omitted. Explicit `tier` values override these defaults. Lower-numbered tiers
are used first; tier numbers can have gaps. Every enabled provider in a lower
tier must have exhausted its quota
or be in an active cooldown or local rate-limit pause before a higher tier can
be used. A provider that merely returned empty results, rejected this query, or
is busy with other requests still prevents advancing unless it also has one of
those pauses.
Providers disabled in the configuration do not participate. Credential and
storage errors alone do not unlock a higher tier.

Eligible providers within the first available tier compete using the same score:
`usage over the last 24 hours / soft daily allocation target`. Lower scores win;
equal scores prefer the least recently selected provider. Unknown capacity has
no categorical priority over monthly credits within a tier. Equal-weight
providers rotate even at low volume, including when all their daily usage has
expired.

Tavily MCP, Firecrawl MCP, Parallel MCP and Purili each define
`ASSUMED_REQUESTS_PER_DAY = 100`
in their provider implementation. These soft targets are fixed in code, not
configurable in TOML. They are allocation weights, not claims about upstream
limits. You and Exa MCP use their existing
100/day and 50/day estimates. Monthly-credit providers use their allowance
divided by 30 and accumulate estimated request costs, so varying result counts
consume the corresponding allocation units.

Targets are soft: exceeding one increases routing pressure without disabling a
provider. Minute/concurrency limits, remote throttling, quota cooldowns and
recovery probes continue to govern availability. Wikipedia and Hacker News
remain separate fallback sources.

Allocation history and last-selection order persist in
`<data_dir>/state/allocation.json`, separately from existing budget and cooldown
files. A new history starts empty on first use. Selection and recording are
serialized before dispatch so outstanding attempts affect subsequent choices;
provider network requests can still run concurrently. Failed or cancelled
admitted attempts retain their allocation conservatively.

Usage is aggregated into minute buckets, retaining a bucket until every request
in it is at least 24 hours old. This bounds storage independently of request
volume and can retain usage for up to one extra minute. Status output includes
each general-web provider's `tier` and an `allocation` object with target source,
units, recent usage, pressure and last selection time. Status inspection does not
modify allocation history.

# Web-fetch

todo.

# License

Licensed under the [MIT License](LICENSE.txt).
