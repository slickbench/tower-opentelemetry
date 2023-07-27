# tower-opentelemetry
An OpenTelemetry layer for Tower-based services such as [Axum](https://github.com/tokio-rs/axum).

Currently only supports HTTP.

## Usage
```rust
// set the global text propagation method (without this, nothing happens)
opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
// add the layer to your axum router (or other Tower-based service)
router.layer(tower_opentelemetry::Layer::new());
```

## Troubleshooting
Ensure your entire tracing stack is using the same version of opentelemetry crates.
