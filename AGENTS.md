# Agent instructions — noetl/server

## Build placement (Mount Shasta Ranch policy, owner decision 2026-11-27)

Compile where the target is native, on our own machines, not in the cloud:

- **Linux / amd64** (release binaries, `linux/amd64` images, `.deb`): the **AMD box**,
  `ssh kadyapam@10.50.0.89` (32 cores, 60 GB RAM; checkouts in `~/projects/noetl/ai-meta/repos/*`).
- **Apple Silicon** (`aarch64-apple-darwin` binaries, `linux/arm64` images for the Mac kind
  cluster): the **Mac Studio**, always through `~/.cargo-queue` (never bypass it).
- **GitHub Actions**: lightweight checks only (fmt, clippy, tests on PRs). Do not add heavy,
  multi-arch or matrix build jobs, and do not dispatch release workflows just to see whether
  something builds.
- **Google Cloud Build / Cloud Run**: no builds or deploys without the owner's explicit OK
  (this includes `gcb-build`).
- No cross-compiling: from the Mac, build Linux artifacts on the AMD box (`--remote amd`).

How (from `noetl/ops`, i.e. `ai-meta/repos/ops`):

```bash
scripts/local-build/noetl-local-build.sh --repos server,worker,cli          # native release binaries -> ~/noetl-build/out/
scripts/local-build/noetl-local-build.sh --repos server --image             # + podman image for this host's arch
scripts/local-build/noetl-local-build.sh --repos server --image --kind-load noetl   # + load into the local kind cluster
scripts/local-build/noetl-local-build.sh --repos server,worker --remote amd # on the Mac: run the Linux build on the AMD box
```

Kind deploys keep using `automation/development/noetl.yaml` (it builds native images on each host).
Full rule: noetl/ai-meta `agents/rules/build-placement.md`.
