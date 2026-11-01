import { renderMarkdown } from "../markdown.ts";
import type { Block, Entry, ThreadView } from "../types.ts";
import { absoluteTime, basename, compactNumber, relativeTime } from "./format.ts";

// `renderMarkdown` escapes all raw HTML, so its output is the only string
// that may go through dangerouslySetInnerHTML.
function Markdown({ source }: { source: string }) {
  return <div class="markdown" dangerouslySetInnerHTML={{ __html: renderMarkdown(source) }} />;
}

function BlockView({ block }: { block: Block }) {
  switch (block.kind) {
    case "text":
      return <Markdown source={block.text} />;
    case "thinking":
      return (
        <details class="block thinking">
          <summary>Thinking</summary>
          <Markdown source={block.text} />
        </details>
      );
    case "tool_use":
      return (
        <details class="block tool">
          <summary>
            <span class="mono">{block.name ?? "tool"}</span>
            {block.status && <span class={`tag tool-${block.status.toLowerCase().replace(/[^a-z]/g, "")}`}>{block.status}</span>}
          </summary>
          <pre>{block.text}</pre>
        </details>
      );
    case "tool_result":
      return (
        <details class="block tool-result">
          <summary>
            Result{block.name && <span class="mono"> {block.name}</span>}
          </summary>
          <pre>{block.text}</pre>
        </details>
      );
    case "image":
      return <div class="block dim">[image{block.text ? `: ${block.text}` : ""}]</div>;
    case "mention":
      return <span class="mention mono">@{block.name ?? block.text}</span>;
    default:
      return <pre class="block">{(block as Block).text}</pre>;
  }
}

function EntryView({ entry }: { entry: Entry }) {
  if (entry.role === "resume") {
    return (
      <div class="resume">
        <span>resumed</span>
      </div>
    );
  }
  return (
    <div class={entry.role === "user" ? "entry user" : "entry agent"}>
      {entry.blocks.map((block, index) => (
        <BlockView key={index} block={block} />
      ))}
    </div>
  );
}

export function ThreadPage({ thread, now }: { thread: ThreadView; now: number }) {
  const tokens = thread.tokens;
  return (
    <>
      <header class="thread-header">
        <a class="back" href="/">
          ← Fleet
        </a>
        <h1>{thread.title || "Untitled"}</h1>
        <div class="thread-facts">
          <span class={thread.live ? "dot live" : "dot"} />
          <span class="tag">{thread.rig}</span>
          {thread.folders.map((folder) => (
            <span class="tag" title={folder} key={folder}>
              {basename(folder)}
            </span>
          ))}
          {thread.model && <span class="mono dim">{thread.model}</span>}
          {tokens && (
            <span class="dim" title={`cache read ${tokens.cache_read}, cache write ${tokens.cache_write}`}>
              {compactNumber(tokens.input)} in · {compactNumber(tokens.output)} out
            </span>
          )}
          <span class="dim" title={absoluteTime(thread.updated_at)}>
            updated {relativeTime(thread.updated_at, now)}
          </span>
        </div>
      </header>
      <main class="conversation">
        {thread.entries.length === 0 && <p class="dim">This thread has no messages.</p>}
        {thread.entries.map((entry, index) => (
          <EntryView key={index} entry={entry} />
        ))}
      </main>
    </>
  );
}
