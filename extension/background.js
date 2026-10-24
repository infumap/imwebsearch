// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

// Holds a WebSocket to the imwebsearch server and renders the pages it asks
// for in active tabs of the user's current window, so the user can watch them
// and complete verification challenges.

importScripts('capture.js');

const PROTOCOL_VERSION = 1;
const DEFAULT_SERVER_URL = 'ws://127.0.0.1:8791/browser-relay';
// WebSocket traffic keeps the service worker alive; the server drops silent connections.
const PING_MS = 20000;
// A refused attempt costs little, and the gap after a server restart is what the user notices.
// It also stays under the service worker's 30-second idle limit, so retries do not stop for the alarm.
const MAX_RETRY_MS = 4000;
// Non-interactive challenges usually clear within a few seconds in a real browser.
const AUTO_SOLVE_MS = 8000;
const RESOURCE_TYPES = ['main_frame', 'sub_frame', 'stylesheet', 'script', 'image', 'font', 'object',
  'xmlhttprequest', 'ping', 'csp_report', 'media', 'websocket', 'webtransport', 'webbundle', 'other'];
// Defense in depth: the server checks the requested and final URLs, but
// redirects and subresources load here without its address checks.
const PRIVATE_ADDRESS = '^[a-z]+://([^/@]*@)?(127\\.|10\\.|0\\.|169\\.254\\.|192\\.168\\.|172\\.(1[6-9]|2[0-9]|3[01])\\.|100\\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\\.|\\[)';
const PRIVATE_DOMAINS = ['localhost', 'local', 'internal', 'lan', 'home.arpa'];

let socket = null;
// Set while connect() reads the settings, before it assigns `socket`.
let connecting = false;
// Bumped by a settings change, so a connect() still reading the old ones reads them again.
let settingsEpoch = 0;
let retryTimer = null;
let retryDelay = 2000;
let nextRuleId = 1;
const jobs = new Map();

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

async function settings() {
  return chrome.storage.local.get({serverUrl: DEFAULT_SERVER_URL, token: '', enabled: false});
}

async function setStatus(state, detail = '') {
  await chrome.storage.session.set({status: {state, detail, at: Date.now()}});
  updateBadge(state);
}

function updateBadge(state) {
  const waiting = [...jobs.values()].some(job => job.notified);
  const [text, color] = waiting ? ['?', '#d97706']
    : state === 'connected' || state === 'disabled' ? ['', '#000000'] : ['!', '#dc2626'];
  chrome.action.setBadgeText({text});
  chrome.action.setBadgeBackgroundColor({color});
}

function send(message) {
  if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(message));
}

// The settings change's own connect() is skipped while another is reading
// them, so a change meanwhile makes this one read them again.
async function connectionSettings() {
  for (;;) {
    const epoch = settingsEpoch;
    const values = await settings();
    const {authFailed} = await chrome.storage.session.get({authFailed: false});
    if (epoch === settingsEpoch) return {...values, authFailed};
  }
}

async function connect() {
  // The startup connect() and the event that woke the worker, such as an
  // alarm, both arrive here before either has a socket.
  if (connecting || socket && socket.readyState <= WebSocket.OPEN) return;
  connecting = true;
  clearTimeout(retryTimer);
  let config;
  try {
    config = await connectionSettings();
  } finally {
    // Nothing below awaits before assigning `socket`.
    connecting = false;
  }
  const {serverUrl, token, enabled, authFailed} = config;
  if (!enabled || !token) return setStatus('disabled');
  if (authFailed) return;
  let ws;
  try {
    ws = new WebSocket(serverUrl);
  } catch (error) {
    await setStatus('error', String(error.message || error));
    return scheduleRetry();
  }
  socket = ws;
  let pingTimer = null;
  // Attach the handlers before awaiting anything: a refused local connection
  // can close before a storage write completes.
  ws.onopen = () => {
    ws.send(JSON.stringify({type: 'hello', token, version: PROTOCOL_VERSION, agent: navigator.userAgent}));
    pingTimer = setInterval(() => ws.send(JSON.stringify({type: 'ping'})), PING_MS);
  };
  ws.onmessage = event => {
    let message;
    try { message = JSON.parse(event.data); } catch (_) { return; }
    handle(message);
  };
  ws.onclose = async event => {
    clearInterval(pingTimer);
    // A socket the settings replaced leaves the jobs, status and retries to its successor.
    if (socket !== ws) return;
    socket = null;
    for (const job of jobs.values()) job.cancelled = true;
    if (event.code === 4001 || event.code === 4002) {
      // Retrying cannot help until the settings or extension change.
      await chrome.storage.session.set({authFailed: true});
      return setStatus('error', event.reason || 'Refused by the server.');
    }
    await setStatus('disconnected', event.reason || `Connection closed (${event.code}).`);
    scheduleRetry();
  };
  await setStatus('connecting', serverUrl);
}

function scheduleRetry() {
  clearTimeout(retryTimer);
  retryTimer = setTimeout(connect, retryDelay);
  retryDelay = Math.min(retryDelay * 2, MAX_RETRY_MS);
}

function handle(message) {
  switch (message.type) {
    case 'ready':
      retryDelay = 2000;
      setStatus('connected');
      break;
    case 'fetch':
      runJob(message);
      break;
    case 'cancel': {
      const job = jobs.get(message.id);
      if (job) job.cancelled = true;
      break;
    }
  }
}

async function runJob(request) {
  const job = {id: request.id, tabId: null, previousTabId: null, ruleIds: [], cancelled: false, challenged: false, notified: false,
    mainFrames: new Set(), status: null, contentType: null, navError: null, download: false};
  jobs.set(job.id, job);
  let result;
  try {
    result = await render(request, job);
  } catch (error) {
    result = {error: String(error.message || error)};
  }
  jobs.delete(job.id);
  if (job.ruleIds.length) {
    await chrome.declarativeNetRequest.updateSessionRules({removeRuleIds: job.ruleIds}).catch(() => {});
  }
  // A pending challenge stays open so the user can still complete it.
  if (job.tabId !== null && !result.verification_pending) await closeTab(job);
  chrome.notifications.clear(`job-${job.id}`);
  updateBadge(socket?.readyState === WebSocket.OPEN ? 'connected' : 'disconnected');
  send({type: 'result', id: job.id, ...result});
}

async function openTab(job) {
  let windowId = null;
  try {
    windowId = (await chrome.windows.getLastFocused({windowTypes: ['normal']})).id;
  } catch (_) {}
  if (windowId === null) {
    const created = await chrome.windows.create({url: 'about:blank', focused: true, type: 'normal'});
    job.tabId = created.tabs[0].id;
    return created.tabs[0];
  }
  const [previous] = await chrome.tabs.query({active: true, windowId});
  job.previousTabId = previous?.id ?? null;
  const tab = await chrome.tabs.create({windowId, url: 'about:blank', active: true, openerTabId: previous?.id});
  job.tabId = tab.id;
  await chrome.windows.update(windowId, {focused: true}).catch(() => {});
  return tab;
}

// Close a finished job's tab, returning to the tab it was opened from if the
// user is still looking at the job's tab.
async function closeTab(job) {
  const tab = await chrome.tabs.get(job.tabId).catch(() => null);
  if (!tab) return;
  if (tab.active && job.previousTabId !== null) await chrome.tabs.update(job.previousTabId, {active: true}).catch(() => {});
  await chrome.tabs.remove(tab.id).catch(() => {});
}

async function blockPrivateAddresses(job) {
  const ids = [nextRuleId++, nextRuleId++];
  const condition = {tabIds: [job.tabId], resourceTypes: RESOURCE_TYPES};
  await chrome.declarativeNetRequest.updateSessionRules({addRules: [
    {id: ids[0], priority: 1, action: {type: 'block'}, condition: {...condition, regexFilter: PRIVATE_ADDRESS}},
    {id: ids[1], priority: 1, action: {type: 'block'}, condition: {...condition, requestDomains: PRIVATE_DOMAINS}},
  ]});
  job.ruleIds = ids;
}

async function inject(tabId, func, args = []) {
  try {
    const [frame] = await chrome.scripting.executeScript({target: {tabId}, func, args});
    return frame?.result ?? null;
  } catch (_) {
    // The tab is navigating, showing an error page or a document scripts cannot reach.
    return null;
  }
}

function readText() {
  const type = document.contentType;
  const body = /xml/.test(type) ? new XMLSerializer().serializeToString(document) : document.body?.innerText || '';
  return {url: location.href, body};
}

async function render(request, job) {
  const url = new URL(request.url);
  if (url.protocol !== 'http:' && url.protocol !== 'https:') throw new Error('Unsupported URL scheme.');
  const deadline = Date.now() + request.timeout_ms;
  const tab = await openTab(job);
  await blockPrivateAddresses(job);
  await chrome.tabs.update(tab.id, {url: url.href});
  let started = null;
  let changed = 0;
  let signature = '';
  let challengeSince = null;
  for (;;) {
    await sleep(100);
    if (Date.now() >= deadline || job.cancelled) {
      if (job.challenged) return {verification_pending: true};
      throw new Error(job.cancelled ? 'Cancelled.' : 'The page did not finish loading before the deadline.');
    }
    if (job.download) throw new Error('The URL started a file download rather than opening a document.');
    if (job.navError) throw new Error(`Navigation failed: ${job.navError}.`);
    // The server reads PDFs with its own headless browser.
    if (/application\/pdf/i.test(job.contentType || '')) return {pdf: true};
    const probe = await inject(tab.id, imwebsearchProbe);
    if (!probe?.ready) {
      started = null;
      signature = '';
      continue;
    }
    const now = Date.now();
    started ??= now;
    if (probe.signature !== signature) {
      signature = probe.signature;
      changed = now;
    }
    const settled = now - started >= 750 && now - changed >= 400;
    if (!(settled && probe.meaningful || now - started >= 3000)) continue;
    const type = await inject(tab.id, () => document.contentType);
    if (type && !/html/i.test(type)) {
      const text = await inject(tab.id, readText);
      if (!text) continue;
      if (job.status >= 400) return {status: job.status};
      return {url: text.url, content_type: type, body: text.body};
    }
    const snapshot = await inject(tab.id, imwebsearchCapture, [{limit: request.limit, dump: request.dump}]);
    if (!snapshot) continue;
    if (snapshot.oversized) throw new Error('Rendered document exceeds its byte or complexity limit.');
    if (snapshot.blocked?.kind === 'challenge') {
      job.challenged = true;
      challengeSince ??= now;
      if (!job.notified && now - challengeSince >= AUTO_SOLVE_MS) await askUser(job, url, tab.windowId);
      // Settle again: completing the challenge navigates to the document.
      started = null;
      await sleep(900);
      continue;
    }
    job.challenged = false;
    if (job.status >= 400) return {status: job.status};
    if (snapshot.blocked) return {error: snapshot.blocked.message};
    return {url: snapshot.url, content_type: type || 'text/html', body: snapshot.html, raw: snapshot.raw};
  }
}

async function askUser(job, url, windowId) {
  job.notified = true;
  send({type: 'waiting', id: job.id});
  updateBadge('connected');
  // The user may have moved away from the job's tab while it loaded.
  await chrome.tabs.update(job.tabId, {active: true}).catch(() => {});
  await chrome.windows.update(windowId, {focused: true}).catch(() => {});
  chrome.notifications.create(`job-${job.id}`, {
    type: 'basic',
    iconUrl: 'icon.png',
    title: 'Verification needed',
    message: `${url.host} wants you to confirm you are human. Click to open the page.`,
    requireInteraction: true,
  });
}

function jobForTab(tabId) {
  for (const job of jobs.values()) if (job.tabId === tabId) return job;
  return null;
}

chrome.webRequest.onBeforeRequest.addListener(details => {
  jobForTab(details.tabId)?.mainFrames.add(details.url);
}, {urls: ['<all_urls>'], types: ['main_frame']});

chrome.webRequest.onHeadersReceived.addListener(details => {
  const job = jobForTab(details.tabId);
  if (!job) return;
  job.status = details.statusCode;
  job.contentType = details.responseHeaders?.find(header => header.name.toLowerCase() === 'content-type')?.value || null;
}, {urls: ['<all_urls>'], types: ['main_frame']}, ['responseHeaders']);

chrome.webNavigation.onErrorOccurred.addListener(details => {
  const job = jobForTab(details.tabId);
  // Downloads and replaced navigations abort; neither is a failure to report here.
  if (job && details.frameId === 0 && details.error !== 'net::ERR_ABORTED') job.navError = details.error;
});

chrome.downloads.onCreated.addListener(item => {
  for (const job of jobs.values()) {
    if (job.mainFrames.has(item.url) || job.mainFrames.has(item.finalUrl)) {
      job.download = true;
      chrome.downloads.cancel(item.id).then(() => chrome.downloads.erase({id: item.id})).catch(() => {});
    }
  }
});

chrome.notifications.onClicked.addListener(async id => {
  const job = jobs.get(Number(id.replace('job-', '')));
  if (!job?.tabId) return;
  const tab = await chrome.tabs.get(job.tabId).catch(() => null);
  if (!tab) return;
  await chrome.windows.update(tab.windowId, {focused: true});
  await chrome.tabs.update(tab.id, {active: true});
  chrome.notifications.clear(id);
});

chrome.storage.onChanged.addListener(async (changes, area) => {
  if (area !== 'local') return;
  await chrome.storage.session.set({authFailed: false});
  settingsEpoch++;
  retryDelay = 2000;
  if (socket) {
    socket.close();
    socket = null;
    for (const job of jobs.values()) job.cancelled = true;
  }
  connect();
});

chrome.alarms.create('reconnect', {periodInMinutes: 1});
chrome.alarms.onAlarm.addListener(alarm => {
  if (alarm.name === 'reconnect') connect();
});

connect();
