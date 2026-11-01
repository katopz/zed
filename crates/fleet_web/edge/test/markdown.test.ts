import assert from "node:assert/strict";
import { test } from "node:test";
import { escapeHtml, renderMarkdown } from "../src/markdown.ts";

test("raw HTML is escaped, never passed through", () => {
  const output = renderMarkdown('hello <script>alert(1)</script> <img src=x onerror="alert(2)">');
  assert.ok(!output.includes("<script"), output);
  assert.ok(!output.includes("<img"), output);
  assert.ok(output.includes("&lt;script&gt;"), output);
});

test("block-level HTML is escaped", () => {
  const output = renderMarkdown("<div onclick=\"x()\">\n<script>alert(1)</script>\n</div>");
  assert.ok(!/<(script|div)/i.test(output), output);
});

test("unsafe link schemes are dropped, safe ones kept", () => {
  const unsafe = renderMarkdown("[click](javascript:alert(1)) [d](data:text/html,x) [v](vbscript:x)");
  assert.ok(!/href=/.test(unsafe), unsafe);
  const safe = renderMarkdown("[zed](https://zed.dev) and [local](/t/m3/abc)");
  assert.ok(safe.includes('href="https://zed.dev"'), safe);
  assert.ok(safe.includes('href="/t/m3/abc"'), safe);
});

test("code and attributes are escaped", () => {
  assert.ok(renderMarkdown("`<b>`").includes("&lt;b&gt;"));
  const titled = renderMarkdown('[x](https://a.b "t\\"><script>")');
  assert.ok(!titled.includes("<script"), titled);
  assert.equal(escapeHtml(`<"'&>`), "&lt;&quot;&#39;&amp;&gt;");
});

test("markdown still renders", () => {
  const output = renderMarkdown("# Title\n\n**bold** and `code`\n\n```rust\nfn main() {}\n```");
  assert.ok(output.includes("<h1>Title</h1>"), output);
  assert.ok(output.includes("<strong>bold</strong>"));
  assert.ok(output.includes("<pre><code"));
});
