// Thread data is untrusted: raw HTML in markdown is escaped, never passed
// through, and link targets are limited to safe schemes.
import { Marked, type Tokens } from "marked";

const HTML_ESCAPES: Record<string, string> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
  '"': "&quot;",
  "'": "&#39;",
};

export function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (char) => HTML_ESCAPES[char]);
}

function safeHref(href: string): string | null {
  const trimmed = href.trim();
  if (/^(https?:|mailto:)/i.test(trimmed)) return trimmed;
  // Relative and fragment links carry no scheme, so they cannot be `javascript:`.
  if (/^[#/]/.test(trimmed) && !trimmed.startsWith("//")) return trimmed;
  return null;
}

const markdown = new Marked({
  gfm: true,
  breaks: true,
  async: false,
  renderer: {
    html({ text }: Tokens.HTML | Tokens.Tag): string {
      return escapeHtml(text);
    },
    link({ href, title, tokens }: Tokens.Link): string {
      const label = this.parser.parseInline(tokens);
      const target = safeHref(href);
      if (target === null) return label;
      const titleAttribute = title ? ` title="${escapeHtml(title)}"` : "";
      return `<a href="${escapeHtml(target)}"${titleAttribute} rel="noopener noreferrer nofollow">${label}</a>`;
    },
    // Remote images are blocked by CSP anyway; show the alt text as a link instead.
    image({ href, text }: Tokens.Image): string {
      const target = safeHref(href);
      const label = `[image: ${escapeHtml(text || "untitled")}]`;
      if (target === null) return label;
      return `<a href="${escapeHtml(target)}" rel="noopener noreferrer nofollow">${label}</a>`;
    },
  },
});

export function renderMarkdown(source: string): string {
  return markdown.parse(source, { async: false }) as string;
}
