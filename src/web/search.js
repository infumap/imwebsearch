// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

const form = document.querySelector(".search-form");
const content = document.querySelector(".search-content");
const status = document.querySelector(".search-status");
const button = form.querySelector("button[type=submit]");
let searching = false;

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (searching) return;

  searching = true;
  button.disabled = true;
  button.textContent = "Searching…";
  content.setAttribute("aria-busy", "true");
  status.textContent = "Searching…";
  // A new search from a GET results page must not leave the old query in the URL.
  history.replaceState(null, "", "/");

  try {
    const response = await fetch(form.action, {
      method: "POST",
      headers: { Accept: "text/html" },
      body: new URLSearchParams(new FormData(form)),
    });
    const page = new DOMParser().parseFromString(await response.text(), "text/html");
    const results = page.querySelector(".search-content");
    if (!results) throw new Error("Missing search results");

    content.replaceChildren(...results.childNodes);
    document.title = page.title;
    document.body.className = page.body.className;
    status.textContent = response.ok ? "Search complete." : "Search failed.";
  } catch {
    const notice = document.createElement("p");
    notice.className = "notice";
    notice.setAttribute("role", "alert");
    notice.textContent = "Search is currently unavailable. Please try again shortly.";
    content.replaceChildren(notice);
    status.textContent = "Search failed.";
  } finally {
    searching = false;
    button.disabled = false;
    button.textContent = "Search";
    content.removeAttribute("aria-busy");
  }
});
