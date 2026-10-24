// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

const fields = {serverUrl: document.getElementById('serverUrl'), token: document.getElementById('token'),
  enabled: document.getElementById('enabled')};
const statusLine = document.getElementById('status');

async function load() {
  const values = await chrome.storage.local.get({serverUrl: 'ws://127.0.0.1:8791/browser-relay', token: '', enabled: false});
  fields.serverUrl.value = values.serverUrl;
  fields.token.value = values.token;
  fields.enabled.checked = values.enabled;
  showStatus();
}

async function showStatus() {
  const {status} = await chrome.storage.session.get({status: null});
  statusLine.textContent = '';
  if (!status) return;
  const state = document.createElement('strong');
  state.textContent = status.state;
  statusLine.append('Status: ', state, status.detail ? ` — ${status.detail}` : '');
}

document.getElementById('save').addEventListener('click', async () => {
  await chrome.storage.local.set({serverUrl: fields.serverUrl.value.trim(), token: fields.token.value.trim(),
    enabled: fields.enabled.checked});
});

chrome.storage.onChanged.addListener((_, area) => {
  if (area === 'session') showStatus();
});

load();
