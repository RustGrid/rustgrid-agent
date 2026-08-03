# Hosted production line-count exceptions

`src/hosted/execution/orchestration.rs` is temporarily above the 2,500-line target (2,717 physical lines, including its module-local regression test). It remains the sole lifecycle decision adapter and domain-event checkpoint boundary: node starts, budget reservations, graph reduction, and durable checkpoint ordering must stay atomic. Splitting only enough methods to meet the numeric limit would create a second lifecycle owner or forwarding-only module.

The next cohesive extraction is model-call admission and accounting (`record_cache_observability`, `reserve_graph_model_call`, `observe_model_cost`, and `observe_failed_model_cost`) into a dedicated execution-accounting module. That extraction should be a separate ticket with replay and reservation-reconciliation tests because those methods share mutable graph-budget state and provider-dispatch failure semantics.
