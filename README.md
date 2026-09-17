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

- **Store raw, reproject on demand.** Observations are kept forever in their original frame and
  units. Transforms to any astronomical frame (ICRF, GCRF, body-fixed) happen at query time via a
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

## Roadmap

soloc starts with a single first-party schema ("Entity") to showcase the pattern; the plan is to grow the
schema library, the tooling, and the integrations around it.

- [x] Core `spacetimestamp` schema, identity, and derived topology
- [x] Append-only ledger with Arrow-IPC persistence and object-store backends (S3 / GCS / Azure)
- [x] Arrow Flight server (proof-of-concept)
- [ ] Debugging / visualizer tool for inspecting frames, chains, and poses
- [ ] Ledger performance benchmarks and storage-organization optimizations
- [ ] Ecosystem integrations — ROS2 [tf2](https://wiki.ros.org/tf2), GPS → spacetimestamp,
  [ArcGIS](https://www.esri.com/) / GeoArrow

## Community

- **Contributing**: see [`CONTRIBUTING.md`](CONTRIBUTING.md)
- **Security**: report privately via [`SECURITY.md`](SECURITY.md)
- **Questions & ideas**: [Discussions](https://github.com/PhenomenaRL/soloc/discussions)
- **Bugs & features**: [Issues](https://github.com/PhenomenaRL/soloc/issues)

## License

Licensed under the [Apache License, Version 2.0](LICENSE). See [`NOTICE`](NOTICE) for attribution.
