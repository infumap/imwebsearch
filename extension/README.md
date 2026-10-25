# Browser relay extension

A Chrome/Chromium extension (MV3, Chrome 116 or newer) that registers a desktop
browser with imwebsearch. While it is connected, imwebsearch renders every page
in this browser instead of its own headless browser. Bot protection such as
Cloudflare often challenges headless browsers; here the user can complete any
challenge that does not clear on its own. When no browser is connected,
fetches use the headless browser as before.

## Setup

1. Choose a secret of at least 32 characters, for example `openssl rand -hex 32`, and put
   it in the server's `config.toml`:

   ```toml
   [browser_relay]
   token = "<secret>"
   ```

   Restart the server. It logs the relay endpoint, `ws://<host>:<port>/browser-relay`.
2. Use a **dedicated browser profile** that is not signed in to anything.
   Relayed pages load with that profile's cookies, and what they return reaches
   the model. Clearance cookies such as `cf_clearance` build up in the profile, so
   fewer challenges appear over time.
3. Open `chrome://extensions`, enable Developer mode, choose **Load unpacked** and
   select this directory.
4. Open the extension's popup. Enter the server URL and token, tick **Connect**
   and save. The status changes to `connected`, and the server's status page shows
   `Browser relay: Connected`.

Upgrade the server and extension together: relay protocol version 2 requires
extension version 0.2.0 or newer. After updating, reload the extension at
`chrome://extensions` and accept its new `debugger` permission. The extension
attaches only to temporary tabs fetching likely or detected PDFs, so it can
capture response bytes before Chrome's viewer or download handler consumes them.
Ordinary page fetches do not attach the debugger. Chrome may show a debugging
notification during PDF capture. Opening DevTools in an attached tab interrupts
the fetch.

The server binds `127.0.0.1` by default. For a browser on another machine, bind
the server to a private interface such as a WireGuard address and use that URL.
The token and pages travel over unencrypted `ws://`, so do not expose the port publicly.

## Behaviour

- While a browser is connected, every page that needs a browser is rendered
  there, and there is no headless fallback if it fails. URLs ending in a text
  suffix (`.txt`, `.json`, `.md`, `.markdown`, `.csv`, `.xml`) are still downloaded
  directly first, as without the relay. PDFs use the connected browser's cookies
  and completed verification challenges. Their original response bytes are sent
  to the server in bounded chunks, then processed by its native PDF extraction
  service and cache. PDF extraction requires `fetch.gpu_tools_url`.
- URLs whose path ends in `.pdf` (case-insensitive, including encoded filenames,
  with any query or fragment) enable PDF capture before navigating. If such a URL
  returns HTML, including a verification page, it renders normally with the
  debugger attached and does not require a second request. Other URLs start
  without the debugger. If response headers identify a PDF or octet-stream
  document, the extension stops that navigation, attaches the debugger, and
  retries the effective URL in the same tab with the same cookies and deadline.
  This extra request can break single-use URLs and can waste part or all of the
  first PDF download; the header observer cannot pause that response.
- Each relayed page opens as a new active tab in the browser's last focused
  window, and that window is brought to the front. The relay expects you to watch
  it. The extension waits for the text to settle, then captures it with the same
  script the headless browser uses (`capture.js`). It then closes the tab and
  returns you to the tab you were on.
- If a challenge persists for eight seconds, its tab and window are brought to
  the front again and a notification appears.
- While a browser is connected, the larger of `browser_relay.timeout_secs` (default
  55) and `fetch.timeout_secs` bounds each fetch. Keep it below the MCP client's
  timeout (Infumap defaults to 60).
  If time runs out during a challenge, the tab stays open and the model is told to
  retry after the user completes it.
- One browser is registered at a time; a new connection replaces the old one.
  The server runs at most two relay fetches at once.

## Limits

- Intercepted PDF responses, including PDFs served as `application/octet-stream`,
  are streamed without saving a download. An initially unrecognized PDF may
  reach Chrome's viewer or download handler before the retry; downloads observed
  by the extension are cancelled on a best-effort basis. Other file downloads
  are refused. Captured PDFs must fit the 128 MiB PDF size limit and the overall
  fetch deadline; no OCR is performed.
- Only challenge pages that `capture.js` recognizes wait for the user. Other bot
  walls arrive as HTTP errors or refusal pages and fail immediately.
- Requests from relay tabs to literal private addresses and to `localhost`,
  `.local`, `.internal`, `.lan` and `.home.arpa` names are blocked. DNS names that
  resolve to private addresses are not blocked; the server only checks the
  requested and final URLs. See [network policy](../NETWORK_POLICY.md).
- Firefox is not supported (it does not run MV3 service workers).
