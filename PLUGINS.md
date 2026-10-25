# Search-provider plugins

Contract version 1. Unix only.

A plugin is one executable that adds any number of search providers. Each
provider joins the general search pool alongside the built-in providers and is
routed, rate limited, cooled down and reported on the status page in the same
way. The plugin only answers queries. All routing policy stays in imwebsearch.

The operator configures a single plugin by its executable path, arguments and
environment in the `[plugin]` section of `config.toml`. Plugins speak
[MCP](https://modelcontextprotocol.io) over stdio.

Whoever can modify the executable or the configuration controls code the
server runs, so imwebsearch refuses a plugin unless:

- the executable is a regular file owned by root or the server's user, and not
  writable by group or others (for example `chmod 755`);
- `config.toml` is not writable by group or others, and is owner-only
  (`chmod 600`) when `[plugin.env]` sets any variable.

Builds made with a umask of `002` are group-writable, so set permissions as part
of deployment.

A minimal working plugin is in [examples/dummy_plugin.rs](examples/dummy_plugin.rs).

## Processes

imwebsearch starts the plugin in two situations:

- **Discovery**, once at startup: `initialize`, `notifications/initialized`,
  `tools/list`. This must complete within 10 seconds. If discovery fails,
  imwebsearch runs without plugin providers until it is restarted.
- **Search**, once per search attempt: `initialize`, `notifications/initialized`,
  one `tools/call`. The whole exchange, including process startup, must complete
  within the attempt deadline: 20 seconds, or less when the overall search
  deadline is nearer.

A process is never reused. After the reply, imwebsearch closes stdin; a plugin
should exit on end of input. Any process still running one second later, or at
the deadline, is killed together with its process group. Keep startup fast:
configure a compiled executable rather than a package-runner command such as
`npx` or `uv run`.

The process environment:

- Arguments are those configured by the operator.
- The environment contains only variables the operator configured. `PATH`,
  `HOME` and other variables are not inherited.
- The working directory is `/`. Use absolute paths.
- The process runs in its own process group. It must not detach from it
  (`setsid`, double fork); detached processes escape cleanup.
- stdin and stdout carry the protocol. stderr goes to the imwebsearch log, may
  be truncated, and must not contain credentials.

Several plugin processes can run at the same time, for the same or different
providers. Anything the plugin persists itself must tolerate concurrent
processes. Rate and quota tracking are imwebsearch's job; a plugin does not need
to keep state between calls.

The plugin runs as the imwebsearch user with unrestricted network access. It is
not sandboxed and is not covered by [NETWORK_POLICY.md](NETWORK_POLICY.md).

## Framing

- JSON-RPC 2.0, UTF-8, one message per line, terminated by `\n`. Messages must
  not contain embedded newlines. Batches are not supported.
- Each line is at most 2 MiB.
- stdout must carry only protocol messages. Any other output on stdout fails the
  attempt as `unexpected_response`. Log to stderr.
- imwebsearch requests protocol version `2025-06-18`. The plugin may answer
  `2025-03-26`, `2025-06-18` or `2025-11-25`, and must advertise the `tools`
  capability.
- imwebsearch answers `ping` requests from the plugin, answers any other plugin
  request with error `-32601`, and ignores notifications from the plugin.

## Declaring providers

Each tool returned by `tools/list` that carries `_meta["imwebsearch/provider"]`
is one provider. Other tools are ignored, so a plugin can also be a general MCP
server. Return all tools in one response; `nextCursor` is not followed.

```json
{
  "name": "acme_news",
  "description": "Acme news search.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "query": { "type": "string" },
      "max_results": { "type": "integer", "minimum": 1, "maximum": 30 }
    },
    "required": ["query", "max_results"]
  },
  "_meta": {
    "imwebsearch/provider": {
      "version": 1,
      "tier": 2,
      "requests_per_minute": 10,
      "daily_quota": 200
    }
  }
}
```

The tool name is the provider name:

- It must match `^[a-z0-9_]{1,32}$` and must not equal a built-in provider name
  (`you`, `exa`, `exa_mcp`, `tavily`, `tavily_mcp`, `firecrawl`, `firecrawl_mcp`,
  `parallel`, `parallel_mcp`, `brave`, `brightdata`, `diffbot`, `serpapi`,
  `context`, `purili`, `hacker_news`, `wikipedia`). Use a distinctive prefix so
  that future built-in providers do not collide with it.
- **Keep names stable.** Cooldowns and quota ledgers are stored under the
  provider name and survive restarts. Renaming a provider discards them, so a
  provider cooling down for a day would be contacted again immediately.

At most 32 providers are registered. A tool with an invalid or colliding name,
an unsupported `version` or invalid metadata is skipped with a logged error; the
remaining providers still register.

`inputSchema` is required by MCP but not inspected. Do not declare an
`outputSchema`: error results carry a different `structuredContent` shape.

### Provider metadata

| Field | Required | Meaning |
| --- | --- | --- |
| `version` | yes | Contract version. Must be `1`. |
| `tier` | yes | Routing tier, a positive integer. A search uses the lowest tier with an available provider. Built-in providers use tiers 1–4; see [README.md](README.md). |
| `requests_per_minute` | yes | Maximum attempt starts in any rolling 60 seconds. 1 to 1,000. |
| `max_concurrent` | no | Maximum attempts in flight for this provider. 1 to 64. |
| `min_interval_ms` | no | Minimum spacing between attempt starts. 0 to 3,600,000. |
| `daily_quota` | no | The upstream's expected free allowance per rolling 24 hours. imwebsearch counts attempts against it persistently, shows the count on the status page, and uses it in place of `routing_requests_per_day`. It is an estimate, **not a local cap**: when the upstream refuses, report `quota_exhausted`. An attempt is counted once the plugin has initialized, just before `tools/call` is sent, whether or not it then succeeds. 1 to 10,000. |
| `routing_requests_per_day` | no | Soft daily target used to balance load between providers in the same tier. It never makes a provider unavailable. Used only without `daily_quota`. Defaults to 100. 1 to 1,000,000. |

Unknown fields are ignored. The operator can override any field except
`version` for each provider in the `[plugin.providers.<name>]` tables of
`config.toml`, and can disable individual providers there.

Plugin providers are unmetered: there is no monetary cost estimate, budget
or usage reporting in version 1.

## Searching

The call's arguments are:

```json
{ "query": "rust async cancellation", "max_results": 5 }
```

`query` is trimmed and non-empty. `max_results` is an integer from 1 to 30.

A successful result:

```json
{
  "content": [{ "type": "text", "text": "{\"results\":[...]}" }],
  "structuredContent": {
    "results": [
      { "title": "Example", "url": "https://example.com/", "snippet": "Matching text." }
    ]
  }
}
```

imwebsearch reads only `structuredContent`. MCP requires `content`; a text
block containing the same JSON is recommended.

- `url` must be absolute `http` or `https` with a host. Results with any other
  URL are dropped.
- An empty `title` is replaced by the URL. `snippet` may be empty.
- Results beyond `max_results` are ignored, and long fields may be truncated.
  Unknown fields are ignored.
- An empty `results` array is a successful search. imwebsearch tries another
  eligible provider for that query without penalising this one.

## Errors

Report upstream failures as a tool error, so imwebsearch can apply the right
cooldown:

```json
{
  "isError": true,
  "content": [{ "type": "text", "text": "Upstream returned HTTP 429." }],
  "structuredContent": { "kind": "rate_limited", "retry_after_secs": 3600, "code": "http_429" }
}
```

- `kind` selects the cooldown, as in the table below. A missing or unknown kind
  is treated as `unexpected_response`.
- `retry_after_secs` (optional, non-negative integer) replaces the default
  cooldown. Pass the upstream's retry hint whenever there is one.
- `code` (optional, short string) is shown on the status page.
- Error text is never passed to the model. It may be logged for the operator,
  and must not contain credentials.

| `kind` | Use when | Effect without `retry_after_secs` |
| --- | --- | --- |
| `rate_limited` | The upstream throttled the request (for example HTTP 429). | Cooldown of 24 hours. An ambiguous 429 may mean exhausted quota, so imwebsearch does not keep probing. |
| `quota_exhausted` | The upstream allowance is used up. | Cooldown of 24 hours. |
| `blocked` | The upstream refused access (for example a block page or HTTP 403). | Cooldown of 15 minutes. |
| `transient` | Network failure, upstream 5xx, upstream timeout. | Exponential backoff: 2 seconds, doubling per consecutive failure, at most 5 minutes. |
| `unexpected_response` | The upstream answered in a form the plugin could not interpret. | Exponential backoff, at least 15 seconds. |
| `invalid_credentials` | The configured credentials were rejected. | Provider disabled until imwebsearch restarts. `retry_after_secs` is ignored. |
| `invalid_request` | The upstream rejected this particular query. | No cooldown; the next query may be accepted. `retry_after_secs` is ignored. |

Cooldowns of 60 seconds or more persist across restarts.

Other failures:

- JSON-RPC errors `-32600` to `-32602` are treated as `invalid_request`. Return
  `-32602` for an unknown tool name or invalid arguments. Any other JSON-RPC
  error is treated as `unexpected_response`.
- Failure to start, exiting before replying, invalid framing and exceeding the
  deadline are treated as `transient`. They never disable a provider.

## Example exchange

Lines sent by imwebsearch are marked `>`, plugin replies `<`.

```
> {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"imwebsearch","version":"0.8.0"}}}
< {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"acme","version":"1.0.0"}}}
> {"jsonrpc":"2.0","method":"notifications/initialized"}
> {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"acme_news","arguments":{"query":"rust async cancellation","max_results":5}}}
< {"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"..."}],"structuredContent":{"results":[{"title":"Example","url":"https://example.com/","snippet":"Matching text."}]}}}
```

imwebsearch then closes stdin.

## Versioning

A change that existing plugins could misinterpret increments the contract
version. imwebsearch skips providers whose `version` it does not support and
logs why. New optional fields and new error kinds may be added within a version;
plugins must ignore fields they do not recognise.
