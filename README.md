# NoETL Server (`noetl-control-plane`)

Control-plane executable for distributed NoETL orchestration.

## Distribution Channels

- **Crates.io**: `noetl-control-plane`
- **Container image**: recommended primary runtime channel (GHCR/GCR)
- **Cloud Build**: recommended for image builds and GKE deploy pipelines

## Release Checklist

1. Bump `version` in `Cargo.toml`.
2. Build and verify:
   - `cargo build --release`
3. Publish crate:
   - `cargo publish`
4. Build and push container image (`server`):
   - via Cloud Build or GitHub Actions.
5. Deploy image to target cluster and validate health endpoints.

## Notes

- This binary is typically deployed in Kubernetes; container distribution is the operational channel.
- Keep crate and container tags aligned to the same semantic version (`vX.Y.Z`).
