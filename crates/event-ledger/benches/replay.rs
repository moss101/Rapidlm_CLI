//! P10-030: append + read-back throughput for the durable event ledger.
use criterion::{Criterion, black_box, criterion_group, criterion_main};

use event_ledger::event::{ActorKind, ActorRef, EventKind};
use event_ledger::ledger::{AppendOptions, EventLedger};
use protocol::{EventId, ProjectId, RedactionClass, SessionId, TraceId};

fn bench_append_and_replay(c: &mut Criterion) {
    let dir = std::env::temp_dir().join(format!("rapidlm-bench-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("bench.sqlite");
    let cancel = event_ledger::ledger::CancellationToken::new();
    let ledger = EventLedger::open(&db).unwrap();
    let session = SessionId::new();
    ledger
        .create_session(session, ProjectId::new(), &cancel)
        .unwrap();
    let actor = ActorRef::new(ActorKind::Human, &EventId::new().to_string()).unwrap();

    c.bench_function("append_1000_events", |b| {
        b.iter(|| {
            for i in 0..1000 {
                ledger
                    .append(
                        session,
                        actor.clone(),
                        EventKind::ModelStreamDelta,
                        serde_json::json!({ "chunk": black_box(format!("d{i}")) }),
                        &AppendOptions {
                            redaction: RedactionClass::Project,
                            trace_id: TraceId::new(),
                            expected_seq: None,
                        },
                        &cancel,
                    )
                    .unwrap();
            }
        })
    });
    c.bench_function("read_back_1000_events", |b| {
        b.iter(|| {
            for seq in 1..=1000u64 {
                black_box(ledger.get(session, seq, &cancel).unwrap());
            }
        })
    });
    drop(ledger);
    let _ = std::fs::remove_dir_all(&dir);
}

criterion_group!(benches, bench_append_and_replay);
criterion_main!(benches);
