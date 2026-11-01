import type { ComponentChildren } from "preact";
import { renderToString } from "preact-render-to-string";
import { STYLES } from "./styles.ts";

export interface LayoutProps {
  title: string;
  login?: string | null;
  scriptSrc?: string;
  children: ComponentChildren;
}

function Layout({ title, login, scriptSrc, children }: LayoutProps) {
  return (
    <html lang="en">
      <head>
        <meta charset="utf-8" />
        <meta name="viewport" content="width=device-width, initial-scale=1" />
        <meta name="robots" content="noindex, nofollow" />
        <title>{title}</title>
        <style dangerouslySetInnerHTML={{ __html: STYLES }} />
        {scriptSrc && <script src={scriptSrc} defer />}
      </head>
      <body>
        <nav class="topbar">
          <a class="brand" href="/">
            Fleet
          </a>
          <span class="spacer" />
          {login && <span class="dim">@{login}</span>}
          {login && <a href="/logout">Sign out</a>}
        </nav>
        <div class="page">{children}</div>
      </body>
    </html>
  );
}

export function renderDocument(props: LayoutProps): string {
  return "<!doctype html>" + renderToString(<Layout {...props} />);
}

/** JSON for an inert `<script type="application/json">`; `<` is escaped so data cannot close the tag. */
export function inertJson(value: unknown): string {
  return JSON.stringify(value).replace(/</g, "\\u003c");
}
