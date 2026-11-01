import type { ThreadMeta } from "../types.ts";
import { basename, relativeTime, toMillis } from "./format.ts";

function groupByRig(threads: ThreadMeta[]): [string, ThreadMeta[]][] {
  const sorted = [...threads].sort((left, right) => (toMillis(right.updated_at) ?? 0) - (toMillis(left.updated_at) ?? 0));
  const groups = new Map<string, ThreadMeta[]>();
  for (const thread of sorted) {
    const group = groups.get(thread.rig);
    if (group) group.push(thread);
    else groups.set(thread.rig, [thread]);
  }
  return [...groups.entries()];
}

function ThreadRow({ thread, now }: { thread: ThreadMeta; now: number }) {
  const folder = thread.folders.length > 0 ? basename(thread.folders[0]) : "";
  const search = [thread.title, thread.rig, thread.model ?? "", folder].join(" ").toLowerCase();
  return (
    <li class="thread-row" data-search={search}>
      <a href={`/t/${encodeURIComponent(thread.rig)}/${encodeURIComponent(thread.id)}`}>
        <span class={thread.live ? "dot live" : "dot"} title={thread.live ? "live" : "idle"} />
        <span class="thread-title">{thread.title || "Untitled"}</span>
        <span class="thread-meta">
          {folder && <span class="tag">{folder}</span>}
          {thread.model && <span class="dim mono">{thread.model}</span>}
          <span class="dim">{thread.messages} msg</span>
          <span class="dim">{relativeTime(thread.updated_at, now)}</span>
        </span>
      </a>
    </li>
  );
}

export function ThreadsSection({ threads, error, now }: { threads: ThreadMeta[] | null; error: string | null; now: number }) {
  return (
    <section id="threads">
      <div class="section-head">
        <h2>Threads</h2>
        <input id="thread-filter" type="search" placeholder="Filter threads" autocomplete="off" aria-label="Filter threads" />
      </div>
      {error && <div class="banner">Origin offline: {error}.</div>}
      {threads && threads.length === 0 && <p class="dim">No threads published yet.</p>}
      {threads &&
        groupByRig(threads).map(([rig, group]) => (
          <div class="thread-group" key={rig}>
            <h3>
              {rig} <span class="dim">({group.length})</span>
            </h3>
            <ul class="thread-list">
              {group.map((thread) => (
                <ThreadRow key={`${thread.rig}/${thread.id}`} thread={thread} now={now} />
              ))}
            </ul>
          </div>
        ))}
    </section>
  );
}
