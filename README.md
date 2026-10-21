<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/soloc-lockup-dark.svg">
    <img alt="soloc" src="assets/soloc-lockup.svg" width="480">
  </picture>
</p>

<p align="center">
  <strong>The ecosystem for unifying localization anywhere in the solar-system.</strong>
</p>

<p align="center">
  <a href="https://github.com/PhenomenaRL/soloc/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/PhenomenaRL/soloc/ci.yml?branch=main&label=CI"></a>
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-blue.svg"></a>
  <img alt="MSRV" src="https://img.shields.io/badge/rust-1.88%2B-orange.svg">
</p>

## Why soloc

The main question that soloc answers is: how do you say **where something is, and when**, in a way that
survives crossing between reference frames, unit systems, and time scales? soloc gives you one
answer that works **anywhere in the solar system**. Soloc, in other words, is the GPS of the solar system.

The core data is *spacetimestamp*: a single row that records a pose (position + orientation) in an
explicit reference frame, at an explicit instant, in native units. Tag any data stream with one and
it becomes spatiotemporally addressable, comparable, transformable, and mergeable with everyone
else's, from a robot deep underwater, to a deep-space probe or construction vehicles on the lunar surface.

- **Store raw, reproject on demand.** Retained observations keep their original frame and
  units; an optional memory limit evicts older history. Transforms to astronomical frames
  (ICRF, GCRF, body-fixed) happen at query time via a
  physics engine backed by [NAIF SPICE](https://naif.jpl.nasa.gov/naif/toolkit.html) ephemerides.
- **Arrow-native throughout.** Every observation is an [Apache Arrow](https://arrow.apache.org/)
  `RecordBatch`. Zero-copy interop with Python (`pyarrow`), Julia, and the rest of the Arrow
  ecosystem comes for free.
- **Federated by design.** Each operator can run their own server; federation is an exchange of Arrow
  Flight streams, or even simple RecordBatches.


<p align="center">
  <img width="320" height="300" alt="{0B34A4BA-40C3-4DDD-88CA-F1C66439E499}" src="https://github.com/user-attachments/assets/44fb7dfd-97bd-4c91-94ad-f4b52c2f56f3" />  
  <img width="320" height="300" alt="{102AE882-DE63-4156-A45F-106B33AF558D}" src="https://github.com/user-attachments/assets/774c4298-4f35-44db-af38-e010ba6cb90f" />
  <img width="320" height="300" alt="{3A5CEB01-C6CE-49CC-9BEA-ABC0CC50A1C8}" src="https://github.com/user-attachments/assets/64f53218-62dc-4964-a432-e4634ca13e8e" />
</p>

## Crates

| Crate | What it is |
|---|---|
| [`spacetimestamp`](crates/spacetimestamp/) | **The core**: the Arrow schema, identity system, derived topology, physics transforms, and spatiotemporal filters |
| [`soloc-ledger`](crates/soloc-ledger/) | Append-only ledger: persistence, pose cache, and federation |
| [`soloc-server`](crates/soloc-server/) | Arrow Flight gRPC server that hosts a ledger over the wire |

## Getting started

**Prerequisites:** Rust ≥ 1.88.

```bash
cargo build --workspace
cargo test --workspace
```

To run the server, see [`crates/soloc-server`](crates/soloc-server/).

The [GPS/GPX workflow](crates/soloc-ledger/README.md#gpsgpx-workflow) covers a single
fix or GPX 1.1 tracks/routes through conversion, bounded ingestion, query, transform
and IPC persistence. Run the small Halifax example without kernels:

```bash
cargo run --locked -p soloc-ledger --features gpx --example import_gpx -- /tmp/halifax.arrow
```

It uses explicit identities, ellipsoidal heights, time and orientation policies with
the ordinary-tracking WGS84→ITRF93 approximation. The
[core API](crates/spacetimestamp/README.md#gps-coordinate-and-time-conversion) documents
precision limits and failure behavior; [Flight ingestion](crates/soloc-server/README.md#batched-gpx-import)
uses the existing server. For Earth-relative playback, the
[exporter](crates/soloc-ledger/README.md#earth-relative-gpx-playback-export) adds
kernel-derived frame poses and segment provenance. The
[visualizer](visualizer/README.md#gpx-playback-with-a-positioned-earth) renders the
WGS84 surface and preserves observation gaps. See the
[acceptance checks](crates/soloc-ledger/README.md#gpsgpx-acceptance-checks) to reproduce
the complete validation workflow.

## Roadmap

soloc starts with a single first-party schema ("Entity") to showcase the pattern; the plan is to grow the
schema library, the tooling, and the integrations around it.

- [x] Core `spacetimestamp` schema, identity, and derived topology
- [x] Append-only ledger with Arrow-IPC persistence and object-store backends (S3 / GCS / Azure)
- [x] Bounded ledger memory: a rolling-window memory limit that keeps each entity's latest state
- [x] Arrow Flight server (proof-of-concept)
- [x] Single GPS observation conversion and optional GPX 1.1 parsing/mapping
- [x] Batched GPX ingestion and partial-completion reports (local ledger / Flight)
- [x] Earth-relative GPX export and playback with explicit segment gaps
- [ ] Debugging / visualizer tool for inspecting frames, chains, and poses
- [ ] Ledger performance benchmarks and storage-organization optimizations
- [ ] Ecosystem integrations — ROS2 [tf2](https://wiki.ros.org/tf2),
  [ArcGIS](https://www.esri.com/) / GeoArrow

## Community

- **Contributing**: see [`CONTRIBUTING.md`](CONTRIBUTING.md)
- **Security**: report privately via [`SECURITY.md`](SECURITY.md)
- **Questions & ideas**: [Discussions](https://github.com/PhenomenaRL/soloc/discussions)
- **Bugs & features**: [Issues](https://github.com/PhenomenaRL/soloc/issues)

## License

Licensed under the [Apache License, Version 2.0](LICENSE). See [`NOTICE`](NOTICE) for attribution.
