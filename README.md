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

## `:old` image tag

Whenever Conti updates a container, it tags the image that was running before
the update as `<repo>:old`. This gives you a manual fallback even after
Conti's own backup container has been removed:

```sh
docker run <repo>:old
```

If a previous `<repo>:old` tag already exists, the image it pointed to is
removed once the tag has moved, so old images don't accumulate on disk.

## Failed update protection

After a rollback, Conti records the failed `(container, image digest)` pair in
`/var/lib/conti/failed.txt`. On the next run, if the registry still serves the
same digest, the update is skipped with a warning. Once a new upstream release
produces a different digest, the update is attempted normally.

To retry a blocked update manually, remove the relevant line from the file:

```sh
# inside the conti container
vi /var/lib/conti/failed.txt
```

When using the Docker deployment the file is stored in the named volume
`conti_data` and persists across container restarts.

## Docker deployment

The provided `Dockerfile` builds a minimal Alpine image. Inside the container
crond runs conti every night at 01:00 in the configured timezone.

```sh
docker compose up -d
```

The Docker socket is mounted so conti can reach the host daemon.

| Variable | Default | Description |
|---|---|---|
| `TZ` | `Europe/Berlin` | Timezone for the cron schedule. |
| `CRON_SCHEDULE` | `0 1 * * *` | Standard cron expression controlling when conti runs. |

Examples:

```sh
CRON_SCHEDULE="0 3 * * *"    # 03:00 every night
CRON_SCHEDULE="0 1 * * 0"    # 01:00 on Sundays only
CRON_SCHEDULE="0 */6 * * *"  # every 6 hours
```

## Manual run

To trigger conti once without waiting for the scheduled time:

```sh
docker run --rm \
  -e RUN_ONCE=true \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v conti_data:/var/lib/conti \
  umbert0/contirs
```

## Requirements

- Docker daemon accessible via the local Unix socket (`/var/run/docker.sock`).
- Rust 1.75 or later (only needed for builds outside Docker).
