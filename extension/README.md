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

The server binds `127.0.0.1` by default. For a browser on another machine, bind
the server to a private interface such as a WireGuard address and use that URL.
The token and pages travel over unencrypted `ws://`, so do not expose the port publicly.

## Behaviour

- While a browser is connected, every page that needs a browser is rendered
  there, and there is no headless fallback if it fails. URLs ending in a text or PDF
  suffix are still downloaded directly first, as without the relay. A document that
  turns out to be a PDF is handed back to the headless browser, which reads PDFs.
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

- The relay itself returns only HTML and text documents. PDFs go to the headless
  browser, which a challenge can still block, and downloads are refused.
- Only challenge pages that `capture.js` recognizes wait for the user. Other bot
  walls arrive as HTTP errors or refusal pages and fail immediately.
- Requests from relay tabs to literal private addresses and to `localhost`,
  `.local`, `.internal`, `.lan` and `.home.arpa` names are blocked. DNS names that
  resolve to private addresses are not blocked; the server only checks the
  requested and final URLs. See [network policy](../NETWORK_POLICY.md).
- Firefox is not supported (it does not run MV3 service workers).
