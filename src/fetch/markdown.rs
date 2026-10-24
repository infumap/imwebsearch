// Copyright (C) The Infumap Authors
// SPDX-License-Identifier: MIT
// See LICENSE.txt for the full license text.

use htmd::element_handler::{HandlerResult, Handlers};
use htmd::{Element, HtmlToMarkdown};
use markup5ever_rcdom::{Node, NodeData};
use std::rc::Rc;

use crate::error::InfuResult;

pub(super) fn html_to_markdown(html: &str) -> InfuResult<String> {
  let converter = HtmlToMarkdown::builder()
    .skip_tags(vec!["script", "style", "nav", "footer", "aside", "noscript", "iframe", "svg", "form"])
    .add_handler(vec!["table"], compact_table)
    .build();
  let source = extract_main_html(html);
  let tree = converter.html_to_tree(&source).map_err(|error| format!("Could not parse rendered HTML: {error}"))?;
  extraction_budget(&tree)?;
  remove_page_chrome(&tree);
  let converted = converter.tree_to_markdown(&tree);
  if converted.len() > MAX_MARKDOWN_BYTES {
    return Err("Extracted Markdown exceeds its 32 MiB memory limit.".into());
  }
  // Retain indentation, code blocks, links, headings and tables. Only collapse
  // blank runs outside fenced code; do not drop labelled document sections.
  let mut lines = Vec::new();
  let mut fence = None;
  let mut blank = false;
  for line in converted.lines() {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
      let marker = &trimmed[..3];
      if fence == Some(marker) {
        fence = None;
      } else if fence.is_none() {
        fence = Some(marker);
      }
    }
    if fence.is_none() && line.trim().is_empty() && blank {
      continue;
    }
    blank = line.trim().is_empty();
    lines.push(line);
  }
  Ok(lines.join("\n").trim().to_owned())
}

/// The page's `<main>` region if it has one, otherwise the whole page. An
/// `<article>` is never chosen on its own: it can be a fragment of the content
/// (PiShop's first of 14 articles is the product description, which dropped
/// the product name and price), and `<main>` contains it anyway. Erring on more
/// text is preferred to risking the part that was wanted.
fn extract_main_html(html: &str) -> String {
  let document = scraper::Html::parse_document(html);
  for selector_text in ["main", "[role=main]"] {
    let Ok(selector) = scraper::Selector::parse(selector_text) else {
      continue;
    };
    if let Some(node) = document.select(&selector).next() {
      let inner = node.html();
      if inner.len() > 200 {
        return inner;
      }
    }
  }
  html.to_string()
}

const MAX_MARKDOWN_BYTES: usize = 32 * 1024 * 1024;

fn tag(node: &Node) -> &str {
  match &node.data {
    NodeData::Element { name, .. } => name.local.as_ref(),
    _ => "",
  }
}

/// Remove the site banner and menus: `<header>` and `role="banner"` or
/// `role="navigation"` elements, unless they sit inside an `<article>`,
/// `<main>` or `role="main"` region, where a header holds that content's own
/// title and byline. Pages without a main region otherwise convert from the
/// top: CanaKit's menu, built from divs inside `<header>` rather than `<nav>`,
/// came to 3,900 characters before the product name and price.
fn remove_page_chrome(root: &Rc<Node>) {
  let mut stack = vec![(root.clone(), false)];
  while let Some((node, in_content)) = stack.pop() {
    let mut children = node.children.borrow_mut();
    if !in_content {
      children.retain(|child| !is_page_chrome(child));
    }
    stack.extend(children.iter().map(|child| (child.clone(), in_content || is_content_region(child))));
  }
}

fn role(node: &Node) -> Option<String> {
  match &node.data {
    NodeData::Element { attrs, .. } => attrs
      .borrow()
      .iter()
      .find(|attribute| attribute.name.local.as_ref() == "role")
      .map(|attribute| attribute.value.trim().to_ascii_lowercase()),
    _ => None,
  }
}

fn is_content_region(node: &Node) -> bool {
  matches!(tag(node), "article" | "main") || role(node).as_deref() == Some("main")
}

fn is_page_chrome(node: &Node) -> bool {
  tag(node) == "header" || matches!(role(node).as_deref(), Some("banner" | "navigation"))
}

fn extraction_budget(root: &Rc<Node>) -> InfuResult<()> {
  let mut stack = vec![(root.clone(), 0_usize, 0_usize, 0_u32)];
  let mut nodes = 0;
  let mut estimated = 0_usize;
  while let Some((node, depth, prefixes, tables)) = stack.pop() {
    nodes += 1;
    if nodes > 100_000 || depth > 256 || tables > 4 {
      return Err("Rendered document exceeds its extraction complexity limit.".into());
    }
    let name = tag(&node);
    if matches!(name, "script" | "style" | "svg" | "noscript") {
      continue;
    }
    let prefixes = prefixes + usize::from(matches!(name, "blockquote" | "ul" | "ol"));
    let tables = tables + u32::from(name == "table");
    if let NodeData::Text { contents } = &node.data {
      let text = contents.borrow();
      estimated = estimated
        .saturating_add(text.len().saturating_mul(4 * 6_usize.saturating_pow(tables)))
        .saturating_add((text.bytes().filter(|byte| *byte == b'\n').count() + 1).saturating_mul(prefixes * 4));
    }
    if name == "table" {
      let rows = table_rows(&node);
      let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
      estimated = estimated.saturating_add(rows.len().saturating_mul(columns).saturating_mul(4));
    }
    if estimated > MAX_MARKDOWN_BYTES {
      return Err("Rendered document exceeds its Markdown expansion budget.".into());
    }
    stack.extend(node.children.borrow().iter().map(|child| (child.clone(), depth + 1, prefixes, tables)));
  }
  Ok(())
}

fn table_rows(table: &Rc<Node>) -> Vec<Vec<Rc<Node>>> {
  let mut rows = Vec::new();
  let mut pending = table.children.borrow().clone();
  pending.reverse();
  while let Some(node) = pending.pop() {
    match tag(&node) {
      "table" => {}
      "tr" => {
        rows.push(node.children.borrow().iter().filter(|cell| matches!(tag(cell), "th" | "td")).cloned().collect())
      }
      _ => {
        let mut children = node.children.borrow().clone();
        children.reverse();
        pending.extend(children);
      }
    }
  }
  rows
}

fn compact_table(handlers: &dyn Handlers, element: Element<'_>) -> Option<HandlerResult> {
  // The default formatter pads every row to the longest cell, which can turn
  // a small sparse table into gigabytes. Unpadded pipe tables preserve content
  // and also handle tables without explicit <th> headers.
  let rows = table_rows(element.node);
  let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
  if columns == 0 {
    return Some(handlers.walk_children(element.node));
  }
  let mut result = String::from("\n\n");
  for child in element.node.children.borrow().iter().filter(|node| tag(node) == "caption") {
    result.push_str(handlers.walk_children(child).content.trim());
    result.push_str("\n\n");
  }
  for (index, row) in rows.iter().enumerate() {
    result.push('|');
    for column in 0..columns {
      let cell = row.get(column).map(|node| handlers.walk_children(node).content).unwrap_or_default();
      result.push_str(&cell.trim().replace('|', "&#124;").replace('\r', "").replace('\n', "<br>"));
      result.push('|');
    }
    result.push('\n');
    if index == 0 {
      result.push('|');
      for _ in 0..columns {
        result.push_str("---|");
      }
      result.push('\n');
    }
  }
  result.push('\n');
  Some(result.into())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn drops_the_site_banner_but_keeps_content_headers() {
    let page = r#"<html><body>
      <header><div class="menu"><a href="/a">Raspberry Pi</a><a href="/b">Arduino</a></div></header>
      <div role="navigation"><a href="/c">Kits</a></div>
      <div class="product"><h1>Raspberry Pi 5 8GB</h1><span>$175.00</span></div>
      <section><article><header><h2>Review</h2><p>By A. Writer</p></header><p>Fast.</p></article></section>
    </body></html>"#;
    let markdown = html_to_markdown(page).unwrap();
    assert!(markdown.starts_with("# Raspberry Pi 5 8GB"), "{markdown}");
    assert!(markdown.contains("$175.00"), "{markdown}");
    assert!(!markdown.contains("Arduino") && !markdown.contains("Kits"), "{markdown}");
    assert!(markdown.contains("Review") && markdown.contains("By A. Writer"), "{markdown}");
  }
}
