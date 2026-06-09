# conti

A single-shot Docker container auto-updater written in Rust.

Conti connects to the local Docker daemon, finds all running containers labeled
`autoupdate=true`, and recreates them whenever a newer image digest is available
for the same tag. All container properties are preserved across the update.

## How it works

1. Pull the image currently used by the container.
2. Compare the pulled digest against the running container's image ID.
3. If they differ, stop the container and recreate it with the new image.
4. Verify the new container starts and stays healthy.
5. If it does not, roll back by restarting the previous container.

## Usage

```sh
cargo build --release
./target/release/conti
```

Log verbosity is controlled via the `RUST_LOG` environment variable (default: `info`):

```sh
RUST_LOG=debug ./target/release/conti
```

## Container labels

| Label | Required | Description |
|---|---|---|
| `autoupdate=true` | yes | Opt the container in to automatic updates. |
| `autoupdate.timeout=<seconds>` | no | How long to wait before deciding the new container is healthy. Default: `60`. |

Example `docker run` flags:

```sh
docker run -d \
  --label autoupdate=true \
  --label autoupdate.timeout=30 \
  nginx:latest
```

## Startup verification

**Without a Docker `HEALTHCHECK`:** Conti waits the full timeout, then checks
whether the container is still running.

**With a Docker `HEALTHCHECK`:** Conti polls every 2 seconds and considers the
update successful as soon as Docker reports the container as `healthy`. It rolls
back immediately if the status becomes `unhealthy`, or if the timeout expires
while the container is still `starting`.

## Rollback

When the new container fails the startup check, Conti:

1. Removes the failed container.
2. Renames the stopped backup container back to its original name.
3. Restarts it.

## Docker deployment

The provided `Dockerfile` builds a minimal Alpine image. Inside the container
crond runs conti every night at 01:00 in the configured timezone.

```sh
docker compose up -d
```

The Docker socket is mounted so conti can reach the host daemon. Set the `TZ`
environment variable in `compose.yml` to match your local timezone (default:
`Europe/Berlin`).

## Requirements

- Docker daemon accessible via the local Unix socket (`/var/run/docker.sock`).
- Rust 1.75 or later (only needed for builds outside Docker).
