//! End-to-end queue routing: producer pushes via [`phantom_loop::LoopEffect::EnqueueTo`],
//! consumer pops via [`phantom_loop::LoopMessageQueueSource`].
//!
//! Verifies the producer→consumer contract that makes cross-loop messaging
//! coherent: the field-map on the producer side and the payload-key
//! extraction on the consumer side line up under realistic JSON shapes.

use std::sync::Arc;

use phantom_loop::{
    EffectContext, FieldMap, LoopContext, LoopEffect, LoopMessageQueueSource, LoopPullResult,
    LoopQueueRegistry, LoopSource, run_effects,
};
use serde_json::json;

#[test]
fn enqueue_to_then_queue_source_round_trips_with_field_map() {
    let registry = Arc::new(LoopQueueRegistry::new());

    // Producer side: imagine the PR-finder loop just produced this result.
    let producer_result = json!({
        "result": {
            "pr_url": "https://github.com/jdmiranda/phantom/pull/1234",
            "pr_number": 1234
        }
    });

    let effects = vec![LoopEffect::EnqueueTo {
        queue: "review-queue".to_string(),
        fields: vec![
            FieldMap {
                from: "result.pr_url".to_string(),
                to: "target_pr".to_string(),
            },
            FieldMap {
                from: "result.pr_number".to_string(),
                to: "pr_number".to_string(),
            },
        ],
    }];

    let ctx = EffectContext {
        result: &producer_result,
        from_loop: "pr-finder",
        queues: &registry,
    };
    let outcome = run_effects(&effects, &ctx).expect("effects run cleanly");
    assert!(!outcome.stop_requested);

    // Consumer side: drain `review-queue` via the real queue source.
    let mut source = LoopMessageQueueSource::new(&registry, "review-queue");
    let consumer_ctx = LoopContext {
        loop_id: "reviewer".to_string(),
    };
    match source.next(&consumer_ctx) {
        LoopPullResult::Available(input) => {
            // Payload must carry the field-mapped values.
            assert_eq!(
                input.payload["target_pr"],
                "https://github.com/jdmiranda/phantom/pull/1234"
            );
            assert_eq!(input.payload["pr_number"], 1234);

            // Correlation id must carry the producing loop's identifier so
            // the consumer can trace causality back to PR-finder.
            assert!(
                input.correlation_id.as_str().starts_with("pr-finder:msg:"),
                "correlation must inherit `from_loop`, got `{}`",
                input.correlation_id
            );

            // Key falls back to `<queue-name>:msg:<pop_count>` because the
            // mapped payload has no `key` field.
            assert_eq!(input.key, "review-queue:msg:1");
        }
        other => panic!("expected Available, got {other:?}"),
    }

    // Second pop on an empty queue → Empty (not Done; queue sources are
    // open-ended by design).
    assert!(matches!(source.next(&consumer_ctx), LoopPullResult::Empty));
}

#[test]
fn distinct_queues_route_independently() {
    let reg = Arc::new(LoopQueueRegistry::new());

    // Two independent producer effects, two independent target queues.
    let r1 = json!({"pr_number": 1});
    let r2 = json!({"pr_number": 2});

    let to_queue_a = vec![LoopEffect::EnqueueTo {
        queue: "queue-a".to_string(),
        fields: vec![FieldMap {
            from: "pr_number".to_string(),
            to: "n".to_string(),
        }],
    }];
    let to_queue_b = vec![LoopEffect::EnqueueTo {
        queue: "queue-b".to_string(),
        fields: vec![FieldMap {
            from: "pr_number".to_string(),
            to: "n".to_string(),
        }],
    }];

    run_effects(
        &to_queue_a,
        &EffectContext {
            result: &r1,
            from_loop: "p1",
            queues: &reg,
        },
    )
    .unwrap();
    run_effects(
        &to_queue_b,
        &EffectContext {
            result: &r2,
            from_loop: "p2",
            queues: &reg,
        },
    )
    .unwrap();

    let mut src_a = LoopMessageQueueSource::new(&reg, "queue-a");
    let mut src_b = LoopMessageQueueSource::new(&reg, "queue-b");
    let ctx = LoopContext {
        loop_id: "c".to_string(),
    };

    match src_a.next(&ctx) {
        LoopPullResult::Available(input) => assert_eq!(input.payload["n"], 1),
        other => panic!("queue-a expected Available, got {other:?}"),
    }
    match src_b.next(&ctx) {
        LoopPullResult::Available(input) => assert_eq!(input.payload["n"], 2),
        other => panic!("queue-b expected Available, got {other:?}"),
    }

    // Both queues are now empty — make sure they don't bleed into each
    // other.
    assert!(matches!(src_a.next(&ctx), LoopPullResult::Empty));
    assert!(matches!(src_b.next(&ctx), LoopPullResult::Empty));
}
