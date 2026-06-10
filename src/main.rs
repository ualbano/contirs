use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bollard::container::{
    Config, CreateContainerOptions, InspectContainerOptions, ListContainersOptions,
    NetworkingConfig, RemoveContainerOptions, RenameContainerOptions, StartContainerOptions,
    StopContainerOptions,
};
use bollard::image::{CreateImageOptions, TagImageOptions};
use bollard::models::{ContainerInspectResponse, HealthStatusEnum};
use bollard::Docker;
use futures_util::StreamExt;
use tracing::{error, info, warn};

mod notify;
use notify::NotificationEvent;

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const LABEL_TIMEOUT: &str = "autoupdate.timeout";
const FAILED_UPDATES_FILE: &str = "/var/lib/conti/failed.txt";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let smtp_config = notify::SmtpConfig::from_env().context("Invalid SMTP configuration")?;

    let docker =
        Docker::connect_with_local_defaults().context("Failed to connect to Docker daemon")?;

    info!("Connected to Docker daemon");

    let ids = find_autoupdate_containers(&docker).await?;

    if ids.is_empty() {
        info!("No running containers with label autoupdate=true found");
        return Ok(());
    }

    info!("Found {} autoupdate container(s)", ids.len());

    let failed = load_failed_updates();
    let mut events = Vec::new();

    for id in &ids {
        match update_container(&docker, id, &failed).await {
            Ok(Some(event)) => events.push(event),
            Ok(None) => {}
            Err(e) => {
                error!("Container {}: {:#}", short_sha(id), e);
                events.push(NotificationEvent::Failed {
                    container: short_sha(id).to_string(),
                    error: format!("{:#}", e),
                });
            }
        }
    }

    if let Some(config) = &smtp_config {
        if let Err(e) = notify::send_summary(config, &events).await {
            error!("Failed to send notification email: {:#}", e);
        }
    }

    Ok(())
}

async fn find_autoupdate_containers(docker: &Docker) -> Result<Vec<String>> {
    let mut filters = HashMap::new();
    filters.insert("label", vec!["autoupdate=true"]);
    filters.insert("status", vec!["running"]);

    let list = docker
        .list_containers(Some(ListContainersOptions {
            filters,
            ..Default::default()
        }))
        .await
        .context("Failed to list containers")?;

    Ok(list.into_iter().filter_map(|c| c.id).collect())
}

async fn update_container(
    docker: &Docker,
    id: &str,
    failed: &HashSet<String>,
) -> Result<Option<NotificationEvent>> {
    let info = docker
        .inspect_container(id, None::<InspectContainerOptions>)
        .await
        .context("Failed to inspect container")?;

    let image = info
        .config
        .as_ref()
        .and_then(|c| c.image.clone())
        .context("Container has no image configured")?;

    let name = info
        .name
        .as_deref()
        .map(|n| n.trim_start_matches('/').to_string())
        .context("Container has no name")?;

    let old_image_id = info
        .image
        .as_deref()
        .context("Container has no current image ID")?
        .to_string();

    let startup_timeout = startup_timeout_from_labels(
        info.config.as_ref().and_then(|c| c.labels.as_ref()),
    );

    info!(container = %name, image = %image, "Checking for update");

    pull_image(docker, &image)
        .await
        .with_context(|| format!("Failed to pull image {}", image))?;

    let new_image_id = docker
        .inspect_image(&image)
        .await
        .context("Failed to inspect image after pull")?
        .id
        .context("Image has no ID")?;

    if old_image_id == new_image_id {
        info!(container = %name, "Already up to date");
        return Ok(None);
    }

    if failed.contains(&failed_key(&name, &new_image_id)) {
        warn!(
            container = %name,
            image = %short_sha(&new_image_id),
            "Skipping update — a previous attempt with this image failed"
        );
        return Ok(None);
    }

    info!(
        container = %name,
        old = %short_sha(&old_image_id),
        new = %short_sha(&new_image_id),
        "New image available — updating"
    );

    if let Err(e) = tag_old_image(docker, &image, &old_image_id).await {
        warn!(container = %name, "Could not tag previous image as :old: {:#}", e);
    }

    docker
        .stop_container(id, None::<StopContainerOptions>)
        .await
        .context("Failed to stop container")?;

    // Rename rather than remove so we can restart it if the new container fails.
    let backup_name = format!("{}_conti_backup", name);
    docker
        .rename_container(
            id,
            RenameContainerOptions {
                name: backup_name.clone(),
            },
        )
        .await
        .context("Failed to rename old container")?;

    let event = match recreate(docker, &info, &name, &image, startup_timeout).await {
        Ok(new_id) => {
            info!(container = %name, id = %short_sha(&new_id), "Update successful");
            if let Err(e) = docker.remove_container(&backup_name, None).await {
                warn!("Could not remove old container {}: {}", backup_name, e);
            }
            NotificationEvent::Updated {
                container: name,
                old_image: short_sha(&old_image_id).to_string(),
                new_image: short_sha(&new_image_id).to_string(),
            }
        }
        Err(e) => {
            error!(container = %name, "Recreation failed: {:#}", e);
            warn!(container = %name, "Rolling back to previous container");
            if let Err(re) = rollback(docker, id, &name).await {
                error!(container = %name, "Rollback also failed: {:#}", re);
            }
            mark_update_failed(&name, &new_image_id);
            NotificationEvent::Failed {
                container: name,
                error: format!("{:#}", e),
            }
        }
    };

    Ok(Some(event))
}

async fn pull_image(docker: &Docker, image: &str) -> Result<()> {
    let (from_image, tag) = parse_image_ref(image);

    let mut stream = docker.create_image(
        Some(CreateImageOptions {
            from_image: from_image.to_string(),
            tag: tag.to_string(),
            ..Default::default()
        }),
        None,
        None,
    );

    while let Some(result) = stream.next().await {
        result.context("Error during image pull")?;
    }

    Ok(())
}

fn parse_image_ref(image: &str) -> (&str, &str) {
    if let Some(pos) = image.rfind(':') {
        let tag = &image[pos + 1..];
        // A colon that is part of a registry port is always followed by a slash.
        if !tag.contains('/') {
            return (&image[..pos], tag);
        }
    }
    (image, "latest")
}

// Tags the image currently in use as `<repo>:old` so it remains available for
// manual rollback, and removes whatever previously held that tag to avoid
// accumulating dangling images.
async fn tag_old_image(docker: &Docker, image: &str, old_image_id: &str) -> Result<()> {
    let (repo, _) = parse_image_ref(image);
    let old_tag = format!("{}:old", repo);

    let previous_old_id = docker.inspect_image(&old_tag).await.ok().and_then(|i| i.id);

    docker
        .tag_image(
            old_image_id,
            Some(TagImageOptions {
                repo: repo.to_string(),
                tag: "old".to_string(),
            }),
        )
        .await
        .with_context(|| format!("Failed to tag {} as {}", short_sha(old_image_id), old_tag))?;

    if let Some(prev_id) = previous_old_id {
        if prev_id != old_image_id {
            if let Err(e) = docker.remove_image(&prev_id, None, None).await {
                warn!(
                    "Could not remove previous {} image {}: {}",
                    old_tag,
                    short_sha(&prev_id),
                    e
                );
            }
        }
    }

    Ok(())
}

async fn recreate(
    docker: &Docker,
    info: &ContainerInspectResponse,
    name: &str,
    image: &str,
    startup_timeout: Duration,
) -> Result<String> {
    let config = build_config(info, image)?;

    let created = docker
        .create_container(
            Some(CreateContainerOptions {
                name,
                platform: None,
            }),
            config,
        )
        .await
        .context("Failed to create container")?;

    docker
        .start_container(&created.id, None::<StartContainerOptions<String>>)
        .await
        .context("Failed to start container")?;

    if let Err(e) = wait_for_ready(docker, &created.id, info, startup_timeout).await {
        // Force-remove so the name is freed even if the container is still running.
        let _ = docker
            .remove_container(
                &created.id,
                Some(RemoveContainerOptions { force: true, ..Default::default() }),
            )
            .await;
        return Err(e);
    }

    Ok(created.id)
}

fn build_config(info: &ContainerInspectResponse, image: &str) -> Result<Config<String>> {
    let cfg = info.config.as_ref().context("Missing container config")?;

    // Networks are stored separately from HostConfig and must be re-attached explicitly.
    let networking_config = info.network_settings.as_ref().and_then(|ns| {
        ns.networks.clone().map(|nets| NetworkingConfig {
            endpoints_config: nets,
        })
    });

    Ok(Config {
        image: Some(image.to_string()),
        cmd: cfg.cmd.clone(),
        entrypoint: cfg.entrypoint.clone(),
        env: cfg.env.clone(),
        labels: cfg.labels.clone(),
        working_dir: cfg.working_dir.clone(),
        user: cfg.user.clone(),
        hostname: cfg.hostname.clone(),
        domainname: cfg.domainname.clone(),
        tty: cfg.tty,
        open_stdin: cfg.open_stdin,
        stop_signal: cfg.stop_signal.clone(),
        stop_timeout: cfg.stop_timeout,
        exposed_ports: cfg.exposed_ports.clone(),
        volumes: cfg.volumes.clone(),
        host_config: info.host_config.clone(),
        networking_config,
        ..Default::default()
    })
}

fn startup_timeout_from_labels(labels: Option<&HashMap<String, String>>) -> Duration {
    labels
        .and_then(|l| l.get(LABEL_TIMEOUT))
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_STARTUP_TIMEOUT)
}

async fn wait_for_ready(
    docker: &Docker,
    id: &str,
    info: &ContainerInspectResponse,
    timeout: Duration,
) -> Result<()> {
    if !container_has_healthcheck(info) {
        tokio::time::sleep(timeout).await;
        return check_still_running(docker, id).await;
    }

    info!("Container has a healthcheck — polling for healthy status");

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;

        let state = docker
            .inspect_container(id, None::<InspectContainerOptions>)
            .await
            .context("Failed to inspect container during health poll")?;

        if !state.state.as_ref().and_then(|s| s.running).unwrap_or(false) {
            let code = state.state.as_ref().and_then(|s| s.exit_code).unwrap_or(0);
            bail!("Container exited (exit code {})", code);
        }

        match state
            .state
            .as_ref()
            .and_then(|s| s.health.as_ref())
            .and_then(|h| h.status.as_ref())
        {
            Some(HealthStatusEnum::HEALTHY) => return Ok(()),
            Some(HealthStatusEnum::UNHEALTHY) => bail!("Container is unhealthy"),
            _ => {}
        }

        if tokio::time::Instant::now() >= deadline {
            bail!("Container did not become healthy within {}s", timeout.as_secs());
        }
    }
}

async fn check_still_running(docker: &Docker, id: &str) -> Result<()> {
    let state = docker
        .inspect_container(id, None::<InspectContainerOptions>)
        .await
        .context("Failed to inspect container")?;

    if state.state.as_ref().and_then(|s| s.running).unwrap_or(false) {
        return Ok(());
    }

    let code = state.state.as_ref().and_then(|s| s.exit_code).unwrap_or(0);
    bail!("Container exited (exit code {})", code);
}

fn container_has_healthcheck(info: &ContainerInspectResponse) -> bool {
    info.config
        .as_ref()
        .and_then(|c| c.healthcheck.as_ref())
        .and_then(|h| h.test.as_ref())
        .and_then(|test| test.first())
        .map(|cmd| cmd != "NONE" && !cmd.is_empty())
        .unwrap_or(false)
}

fn failed_key(container: &str, image_id: &str) -> String {
    format!("{} {}", container, image_id)
}

fn load_failed_updates() -> HashSet<String> {
    match std::fs::read_to_string(FAILED_UPDATES_FILE) {
        Ok(content) => content.lines().map(str::to_string).collect(),
        // A missing file is not an error — it simply means no failures have been recorded yet.
        Err(_) => HashSet::new(),
    }
}

fn mark_update_failed(container: &str, image_id: &str) {
    let entry = format!("{}\n", failed_key(container, image_id));
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(FAILED_UPDATES_FILE)
        .and_then(|mut f| f.write_all(entry.as_bytes()));

    if let Err(e) = result {
        warn!("Could not write to {}: {}", FAILED_UPDATES_FILE, e);
    }
}

async fn rollback(docker: &Docker, id: &str, original_name: &str) -> Result<()> {
    docker
        .rename_container(
            id,
            RenameContainerOptions {
                name: original_name.to_string(),
            },
        )
        .await
        .context("Failed to rename container back during rollback")?;

    docker
        .start_container(id, None::<StartContainerOptions<String>>)
        .await
        .context("Failed to restart old container during rollback")?;

    info!(container = %original_name, "Rollback successful — previous container is running again");
    Ok(())
}

fn short_sha(id: &str) -> &str {
    let s = id.strip_prefix("sha256:").unwrap_or(id);
    &s[..s.len().min(12)]
}
