# /// script
# requires-python = ">=3.11"
# dependencies = ["zstandard>=0.22"]
# ///
"""
Verdict ping-pong GOAT scorer (`.issues/016` part 1).

Scores post-hoc fix rate for the `request_verdict` ping-pong benchmark by
reading persisted thread records from Zed's threads database
(`paths::data_dir()/threads/threads.db`). Verdict negotiation data rides
inside each thread record itself: every `request_verdict` tool call's
structured output (`reviewer`, `round`, `max_rounds`, `session_id`) is
persisted alongside the thread's messages, so no log scraping is needed.

Usage:
  uv run script/verdict_scorer.py                    # auto-detect threads.db
  uv run script/verdict_scorer.py --db PATH --since 2026-08-01
  uv run script/verdict_scorer.py --out .benchmarks/verdict_goat.json

Exit code 0 always; the verdict (promote / don't promote) is a human call
per the issue's GOAT gates.
"""

from __future__ import annotations

import argparse
import json
import re
import sqlite3
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

import zstandard

TOOL_NAME = "request_verdict"

# Correction keywords from the benchmark design (word-boundary matched).
CORRECTION_PATTERNS = [
    r"you missed",
    r"missed",
    r"actually",
    r"\bfix\b",
    r"\bfixed\b",
    r"wrong",
    r"didn'?t",
    r"does ?n'?t work",
    r"not working",
    r"still fail",
    r"still broken",
    r"regression",
    r"revert",
    r"not quite",
    r"try again",
    r"one more",
    r"almost",
]
CORRECTION_RE = re.compile("|".join(CORRECTION_PATTERNS), re.IGNORECASE)

SUMMARY_HEADING_RE = re.compile(r"^#{1,3}\s*summary\b", re.IGNORECASE | re.MULTILINE)


def default_metadata_db_candidates() -> list[Path]:
    """agent_ui's ThreadMetadataStore DB (sidebar_threads table)."""
    home = Path.home()
    if sys.platform == "win32":
        root = home / "AppData" / "Local" / "Zed" / "db"
    elif sys.platform == "darwin":
        root = home / "Library" / "Application Support" / "Zed" / "db"
    else:
        root = home / ".local" / "share" / "zed" / "db"
    return [root / "0-dev" / "db.sqlite", root / "0-global" / "db.sqlite"]


def load_continuation_edges(db_path: Path | None) -> dict[str, str]:
    """child session id -> continued-from session id."""
    path = db_path
    if path is None:
        path = next((p for p in default_metadata_db_candidates() if p.exists()), None)
        if path is None:
            return {}
    try:
        connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        edges = {
            child: parent
            for child, parent in connection.execute(
                "SELECT session_id, continued_from_session_id FROM sidebar_threads "
                "WHERE session_id IS NOT NULL AND continued_from_session_id IS NOT NULL"
            )
        }
        connection.close()
        return edges
    except sqlite3.Error:
        return {}


def chain_root(session_id: str, edges: dict[str, str]) -> str:
    """Follow continued-from links to the chain's originating thread."""
    seen: set[str] = set()
    current = session_id
    while current in edges and current not in seen:
        seen.add(current)
        current = edges[current]
    return current


def default_db_candidates() -> list[Path]:
    home = Path.home()
    if sys.platform == "win32":
        local = home / "AppData" / "Local"
        return [
            local / "Zed" / "threads" / "threads.db",
            local / "Zed-dev" / "threads" / "threads.db",
        ]
    if sys.platform == "darwin":
        return [
            home / "Library" / "Application Support" / "Zed" / "threads" / "threads.db",
            home / "Library" / "Application Support" / "Zed-dev" / "threads" / "threads.db",
        ]
    return [
        home / ".local" / "share" / "zed" / "threads" / "threads.db",
        home / ".config" / "zed" / "threads" / "threads.db",
    ]


def decompress_data(data: bytes, data_type: str | None) -> bytes:
    if data_type and data_type.lower() == "zstd":
        # Frames written by the `zstd` crate don't embed a content size, so use
        # streaming decompression rather than `ZstdDecompressor.decompress`.
        return zstandard.ZstdDecompressor().decompressobj().decompress(data)
    # Unknown type: try zstd first (current writer), fall back to raw JSON.
    try:
        return zstandard.ZstdDecompressor().decompressobj().decompress(data)
    except zstandard.ZstdError:
        return data


def content_text(content: list) -> str:
    """Concatenate the plain-text items of a message content array.

    `Text` variants serialize as {"Text": "..."} (serde newtype variants);
    bare strings are tolerated for forward compatibility. Structured variants
    (Mention/Image/Thinking/ToolUse) are skipped.
    """
    parts = []
    for item in content:
        if isinstance(item, str):
            parts.append(item)
        elif isinstance(item, dict) and isinstance(item.get("Text"), str):
            parts.append(item["Text"])
    return "\n".join(parts)


def agent_message_text(msg: dict) -> str:
    return content_text(msg.get("content", []))


def user_message_text(msg: dict) -> str:
    return content_text(msg.get("content", []))


@dataclass
class VerdictCall:
    round: int | None
    max_rounds: int | None
    reviewer: str
    is_error: bool
    error: str | None
    session_id: str | None


@dataclass
class ThreadScore:
    thread_id: str
    title: str
    updated_at: str
    verdict_calls: list[VerdictCall] = field(default_factory=list)
    has_summary: bool = False
    post_summary_user_messages: int = 0
    corrections: list[str] = field(default_factory=list)
    input_tokens: int = 0
    output_tokens: int = 0
    first_user_message: str = ""

    @property
    def verdict_on(self) -> bool:
        return any(not call.is_error for call in self.verdict_calls)

    @property
    def rounds_used(self) -> int:
        return max((call.round or 0 for call in self.verdict_calls), default=0)

    @property
    def aborted(self) -> bool:
        # An error after at least one successful round = negotiation abort.
        successes = sum(1 for call in self.verdict_calls if not call.is_error)
        errors = sum(1 for call in self.verdict_calls if call.is_error)
        return successes > 0 and errors > 0

    @property
    def reviewers(self) -> set[str]:
        return {call.reviewer for call in self.verdict_calls if call.reviewer}


def parse_verdict_calls(messages: list) -> list[VerdictCall]:
    calls: list[VerdictCall] = []
    for message in messages:
        # Unit variants (`"Resume"`, `"Compaction"`-adjacent) are bare strings.
        if not isinstance(message, dict):
            continue
        agent = message.get("Agent")
        if not isinstance(agent, dict):
            continue
        results = agent.get("tool_results")
        if not isinstance(results, dict):
            continue
        for result in results.values():
            if not isinstance(result, dict) or result.get("tool_name") != TOOL_NAME:
                continue
            output = result.get("output")
            calls.append(parse_tool_output(output, bool(result.get("is_error"))))
    return calls


def parse_tool_output(output, is_error_flag: bool) -> VerdictCall:
    # The structured output is the serde form of RequestVerdictToolOutput
    # (untagged): success = flat object with round/max_rounds/reviewer,
    # error = flat object with error/reviewer.
    if not isinstance(output, dict):
        output = {}
    round_ = output.get("round")
    max_rounds = output.get("max_rounds")
    return VerdictCall(
        round=round_ if isinstance(round_, int) else None,
        max_rounds=max_rounds if isinstance(max_rounds, int) else None,
        reviewer=str(output.get("reviewer") or ""),
        is_error=is_error_flag or "error" in output,
        error=output.get("error"),
        session_id=output.get("session_id"),
    )


def score_thread(thread_id: str, thread: dict) -> ThreadScore:
    messages = thread.get("messages", [])
    score = ThreadScore(
        thread_id=thread_id,
        title=str(thread.get("title") or ""),
        updated_at=str(thread.get("updated_at") or ""),
        verdict_calls=parse_verdict_calls(messages),
    )

    usage = thread.get("cumulative_token_usage") or {}
    score.input_tokens = int(usage.get("input_tokens", 0) or 0)
    score.output_tokens = int(usage.get("output_tokens", 0) or 0)

    # Locate the FINAL assistant summary: the last agent message containing a
    # `## Summary` heading. Corrections are user messages strictly after it.
    final_summary_ix = None
    for ix, message in enumerate(messages):
        if not isinstance(message, dict):
            continue
        agent = message.get("Agent")
        if isinstance(agent, dict) and SUMMARY_HEADING_RE.search(agent_message_text(agent)):
            final_summary_ix = ix
    if final_summary_ix is not None:
        score.has_summary = True
        for message in messages[final_summary_ix + 1 :]:
            if not isinstance(message, dict):
                continue
            user = message.get("User")
            if not isinstance(user, dict):
                continue
            text = user_message_text(user)
            if not text.strip():
                continue
            score.post_summary_user_messages += 1
            if CORRECTION_RE.search(text):
                # Keep a short snippet for manual review of flagged threads.
                snippet = " ".join(text.split())[:120]
                score.corrections.append(snippet)

    # First user message: a continuation whose opening message is corrective
    # is a post-hoc fix in a follow-up thread (same-session continuations are
    # rare - see the dry-run finding in .issues/016).
    for message in messages:
        if not isinstance(message, dict):
            continue
        user = message.get("User")
        if not isinstance(user, dict):
            continue
        text = user_message_text(user)
        if text.strip():
            score.first_user_message = " ".join(text.split())[:120]
            break
    return score


def cohort_stats(chains: list[dict]) -> dict:
    with_summary = [c for c in chains if c["has_summary"]]
    corrected = [c for c in with_summary if c["corrections"]]
    verdict_on = [c for c in chains if c["verdict_on"]]
    rounds = [c["rounds_used"] for c in verdict_on]
    return {
        "chains": len(chains),
        "chains_with_summary": len(with_summary),
        "chains_with_corrections": len(corrected),
        "post_hoc_fix_rate": (len(corrected) / len(with_summary)) if with_summary else None,
        "rounds_used_distribution": {
            str(n): rounds.count(n) for n in sorted(set(rounds))
        },
        "negotiation_aborts": sum(1 for c in chains if c["aborted"]),
        "reviewers": sorted({r for c in chains for r in c["reviewers"]}),
        "avg_chain_input_tokens": (
            sum(c["input_tokens"] for c in chains) / len(chains) if chains else None
        ),
        "avg_chain_output_tokens": (
            sum(c["output_tokens"] for c in chains) / len(chains) if chains else None
        ),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--db",
        type=Path,
        help="Path to threads.db (default: auto-detect Zed data dirs)",
    )
    parser.add_argument(
        "--since",
        type=str,
        help="Only score threads updated on/after this ISO date (YYYY-MM-DD)",
    )
    parser.add_argument(
        "--metadata-db",
        type=Path,
        help="Path to agent_ui's db.sqlite (sidebar_threads) for continuation "
        "linking (default: auto-detect)",
    )
    parser.add_argument("--out", type=Path, help="Write JSON summary to this path")
    args = parser.parse_args()

    db_path = args.db
    if db_path is None:
        db_path = next((p for p in default_db_candidates() if p.exists()), None)
        if db_path is None:
            print("error: no threads.db found; pass --db PATH", file=sys.stderr)
            print(
                "candidates checked: "
                + ", ".join(str(p) for p in default_db_candidates()),
                file=sys.stderr,
            )
            return 2

    since = None
    if args.since:
        since = datetime.fromisoformat(args.since).replace(tzinfo=timezone.utc)

    connection = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    rows = connection.execute(
        "SELECT id, updated_at, data_type, data FROM threads"
    ).fetchall()
    connection.close()

    scores: list[ThreadScore] = []
    parse_failures = 0
    for thread_id, updated_at, data_type, data in rows:
        if since is not None:
            try:
                thread_time = datetime.fromisoformat(
                    str(updated_at).replace("Z", "+00:00")
                )
                if thread_time < since:
                    continue
            except ValueError:
                pass
        try:
            thread = json.loads(decompress_data(bytes(data), data_type))
        except Exception:
            parse_failures += 1
            continue
        scores.append(score_thread(str(thread_id), thread))

    verdict_on = [s for s in scores if s.verdict_on]
    verdict_off = [s for s in scores if not s.verdict_on]

    # Group threads into continuation chains: corrections in follow-up threads
    # count against the originating thread, and a continuation whose FIRST
    # user message is corrective is a post-hoc fix signal of its own.
    edges = load_continuation_edges(args.metadata_db)
    chains: dict[str, list[ThreadScore]] = {}
    for score in scores:
        root = chain_root(score.thread_id, edges)
        chains.setdefault(root, []).append(score)

    def chain_record(root: str, members: list[ThreadScore]) -> dict:
        # Descendants other than the root are continuations by construction.
        continuations = [m for m in members if m.thread_id != root]
        corrective_continuations = [
            m
            for m in continuations
            if m.first_user_message and CORRECTION_RE.search(m.first_user_message)
        ]
        corrections = [c for m in members for c in m.corrections]
        corrections += [m.first_user_message for m in corrective_continuations]
        return {
            "root": root,
            "members": [m.thread_id for m in members],
            "verdict_on": any(m.verdict_on for m in members),
            "has_summary": any(m.has_summary for m in members),
            "corrections": corrections,
            "rounds_used": max((m.rounds_used for m in members), default=0),
            "aborted": any(m.aborted for m in members),
            "reviewers": sorted({r for m in members for r in m.reviewers}),
            "input_tokens": sum(m.input_tokens for m in members),
            "output_tokens": sum(m.output_tokens for m in members),
        }

    chain_records = [chain_record(root, members) for root, members in chains.items()]
    on_chains = [c for c in chain_records if c["verdict_on"]]
    off_chains = [c for c in chain_records if not c["verdict_on"]]
    report = {
        "db": str(db_path),
        "metadata_db_linked": bool(edges),
        "scored_threads": len(scores),
        "chain_count": len(chain_records),
        "parse_failures": parse_failures,
        "verdict_on": cohort_stats(on_chains),
        "verdict_off": cohort_stats(off_chains),
        "chains": [
            {
                "root": c["root"],
                "members": c["members"],
                "rounds_used": c["rounds_used"],
                "aborted": c["aborted"],
                "reviewers": c["reviewers"],
                "has_summary": c["has_summary"],
                "corrections": c["corrections"],
            }
            for c in chain_records
        ],
    }

    # Human-readable table.
    print(f"threads.db: {db_path}")
    print(
        f"scored: {report['scored_threads']} threads in {len(chain_records)} chains "
        f"({parse_failures} parse failures, continuation links: {len(edges)})\n"
    )
    for label, cohort in (("verdict ON", report["verdict_on"]), ("verdict OFF", report["verdict_off"])):
        rate = cohort["post_hoc_fix_rate"]
        rate_str = f"{rate:.1%}" if rate is not None else "n/a"
        print(f"{label}:")
        print(f"  chains={cohort['chains']} with_summary={cohort['chains_with_summary']}")
        print(f"  post-hoc fix rate: {rate_str} ({cohort['chains_with_corrections']} corrected)")
        print(f"  rounds: {cohort['rounds_used_distribution']}")
        print(f"  aborts: {cohort['negotiation_aborts']}  reviewers: {cohort['reviewers']}")
        avg_in = cohort["avg_chain_input_tokens"]
        avg_out = cohort["avg_chain_output_tokens"]
        print(
            "  avg chain tokens: "
            + (f"in={avg_in:.0f} out={avg_out:.0f}" if avg_in is not None else "n/a")
        )
        print()

    on_rate = report["verdict_on"]["post_hoc_fix_rate"]
    off_rate = report["verdict_off"]["post_hoc_fix_rate"]
    if on_rate is not None and off_rate not in (None, 0):
        relative = (off_rate - on_rate) / off_rate
        print(f"relative fix-rate reduction: {relative:.1%} (GOAT gate: >= ~30%)")
    else:
        print("relative fix-rate reduction: n/a (need both cohorts with summaries)")

    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(report, indent=2), encoding="utf-8")
        print(f"\nwrote {args.out}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
