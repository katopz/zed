//! Plan 037 perf gate: a reattach replays the whole journal of a long turn.
//! Streaming agents journal one update per token, so 10k chunks is a
//! realistic single-turn worst case.

use agentd::journal::{FileJournal, JournalStore as _, coalesce_chunks};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use serde_json::{Value, json};
use std::hint::black_box;

const CHUNKS: usize = 10_000;
const SESSION_ID: &str = "bench-session";

fn token_updates() -> Vec<Value> {
    (0..CHUNKS)
        .map(|index| match index % 500 {
            499 => json!({"sessionUpdate": "tool_call", "toolCallId": index.to_string()}),
            _ => json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "tok "},
            }),
        })
        .collect()
}

fn journal_replay(criterion: &mut Criterion) {
    let updates = token_updates();

    criterion.bench_function("append_10k_chunks", |bencher| {
        bencher.iter_batched(
            || {
                let dir = tempfile::tempdir().expect("tempdir");
                let journal = FileJournal::new(dir.path().to_path_buf()).expect("journal");
                (dir, journal)
            },
            |(dir, mut journal)| {
                for update in &updates {
                    journal.append_update(SESSION_ID, update).expect("append");
                }
                drop(dir);
            },
            BatchSize::PerIteration,
        );
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let mut journal = FileJournal::new(dir.path().to_path_buf()).expect("journal");
    for update in &updates {
        journal.append_update(SESSION_ID, update).expect("append");
    }

    criterion.bench_function("read_10k_chunks", |bencher| {
        bencher.iter(|| black_box(journal.updates(SESSION_ID).expect("read")));
    });

    criterion.bench_function("coalesce_10k_chunks", |bencher| {
        bencher.iter_batched(
            || updates.clone(),
            |updates| black_box(coalesce_chunks(updates)),
            BatchSize::LargeInput,
        );
    });

    criterion.bench_function("replay_10k_chunks", |bencher| {
        bencher.iter(|| {
            let updates = journal.updates(SESSION_ID).expect("read");
            black_box(coalesce_chunks(updates))
        });
    });
}

criterion_group!(benches, journal_replay);
criterion_main!(benches);
