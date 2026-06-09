use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bollard::container::{
    Config, CreateContainerOptions, InspectContainerOptions, ListContainersOptions,
    NetworkingConfig, RemoveContainerOptions, RenameContainerOptions, StartContainerOptions,
    StopContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::models::{ContainerInspectResponse, HealthStatusEnum};
use bollard::Docker;
use futures_util::StreamExt;
use tracing::{error, info, warn};

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const LABEL_TIMEOUT: &str = "autoupdate.timeout";
const FAILED_UPDATES_FILE: &str = "/var/lib/conti/failed.txt";

const LABEL_PROJECT: &str = "com.docker.compose.project";
const LABEL_SERVICE: &str = "com.docker.compose.service";
const LABEL_DEPENDS_ON: &str = "com.docker.compose.depends_on";

struct ContainerNode {
    id: String,
    name: String,
    service: String,
    image: String,
    old_image_id: String,
    depends_on: Vec<String>,
    startup_timeout: Duration,
    inspect: ContainerInspectResponse,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

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
    let nodes = inspect_containers(&docker, &ids).await?;
    let groups = group_by_project(nodes);

    for (project, containers) in groups {
        match project {
            None => {
                for node in containers {
                    if let Err(e) = update_standalone(&docker, node, &failed).await {
                        error!("{:#}", e);
                    }
                }
            }
            Some(ref name) => {
                if let Err(e) = update_project_group(&docker, name, containers, &failed).await {
                    error!("Project {}: {:#}", name, e);
                }
            }
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

async fn inspect_containers(docker: &Docker, ids: &[String]) -> Result<Vec<ContainerNode>> {
    let mut nodes = Vec::new();

    for id in ids {
        let inspect = docker
            .inspect_container(id, None::<InspectContainerOptions>)
            .await
            .with_context(|| format!("Failed to inspect container {}", short_sha(id)))?;

        let labels = inspect.config.as_ref().and_then(|c| c.labels.as_ref());

        let image = inspect
            .config
            .as_ref()
            .and_then(|c| c.image.clone())
            .context("Container has no image configured")?;

        let name = inspect
            .name
            .as_deref()
            .map(|n| n.trim_start_matches('/').to_string())
            .context("Container has no name")?;

        let old_image_id = inspect
            .image
            .as_deref()
            .context("Container has no image ID")?
            .to_string();

        let service = labels
            .and_then(|l| l.get(LABEL_SERVICE))
            .cloned()
            .unwrap_or_else(|| name.clone());

        let depends_on = labels
            .and_then(|l| l.get(LABEL_DEPENDS_ON))
            .map(|v| parse_depends_on(v))
            .unwrap_or_default();

        let startup_timeout = startup_timeout_from_labels(labels);

        nodes.push(ContainerNode {
            id: id.clone(),
            name,
            service,
            image,
            old_image_id,
            depends_on,
            startup_timeout,
            inspect,
        });
    }

    Ok(nodes)
}

fn group_by_project(nodes: Vec<ContainerNode>) -> Vec<(Option<String>, Vec<ContainerNode>)> {
    let mut map: HashMap<Option<String>, Vec<ContainerNode>> = HashMap::new();

    for node in nodes {
        let project = node
            .inspect
            .config
            .as_ref()
            .and_then(|c| c.labels.as_ref())
            .and_then(|l| l.get(LABEL_PROJECT))
            .cloned();

        map.entry(project).or_default().push(node);
    }

    map.into_iter().collect()
}

// `com.docker.compose.depends_on` format: "service1:condition,service2:condition"
fn parse_depends_on(label: &str) -> Vec<String> {
    label
        .split(',')
        .filter_map(|entry| {
            let service = entry.split(':').next()?.trim();
            if service.is_empty() { None } else { Some(service.to_string()) }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Standalone update (containers without a compose project label)
// ---------------------------------------------------------------------------

async fn update_standalone(
    docker: &Docker,
    node: ContainerNode,
    failed: &HashSet<String>,
) -> Result<()> {
    info!(container = %node.name, image = %node.image, "Checking for update");

    pull_image(docker, &node.image)
        .await
        .with_context(|| format!("Failed to pull image {}", node.image))?;

    let new_image_id = docker
        .inspect_image(&node.image)
        .await
        .context("Failed to inspect image after pull")?
        .id
        .context("Image has no ID")?;

    if node.old_image_id == new_image_id {
        info!(container = %node.name, "Already up to date");
        return Ok(());
    }

    if failed.contains(&failed_key(&node.name, &new_image_id)) {
        warn!(
            container = %node.name,
            image = %short_sha(&new_image_id),
            "Skipping — previous attempt with this image failed"
        );
        return Ok(());
    }

    info!(
        container = %node.name,
        old = %short_sha(&node.old_image_id),
        new = %short_sha(&new_image_id),
        "New image available — updating"
    );

    docker
        .stop_container(&node.id, None::<StopContainerOptions>)
        .await
        .context("Failed to stop container")?;

    // Rename rather than remove so we can restart it if the new container fails.
    let backup_name = format!("{}_conti_backup", node.name);
    docker
        .rename_container(
            &node.id,
            RenameContainerOptions {
                name: backup_name.clone(),
            },
        )
        .await
        .context("Failed to rename old container")?;

    match recreate(docker, &node.inspect, &node.name, &node.image, node.startup_timeout).await {
        Ok(new_id) => {
            info!(container = %node.name, id = %short_sha(&new_id), "Update successful");
            if let Err(e) = docker.remove_container(&backup_name, None).await {
                warn!("Could not remove old container {}: {}", backup_name, e);
            }
        }
        Err(e) => {
            error!(container = %node.name, "Recreation failed: {:#}", e);
            warn!(container = %node.name, "Rolling back to previous container");
            if let Err(re) = rollback(docker, &node.id, &node.name).await {
                error!(container = %node.name, "Rollback also failed: {:#}", re);
            }
            mark_update_failed(&node.name, &new_image_id);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Project group update
// ---------------------------------------------------------------------------

async fn update_project_group(
    docker: &Docker,
    project: &str,
    nodes: Vec<ContainerNode>,
    failed: &HashSet<String>,
) -> Result<()> {
    info!(project = %project, containers = nodes.len(), "Processing compose project");

    // Pull all images and collect new digests.
    let mut new_ids: HashMap<String, String> = HashMap::new();
    for node in &nodes {
        pull_image(docker, &node.image)
            .await
            .with_context(|| format!("Failed to pull {} for {}", node.image, node.name))?;

        let id = docker
            .inspect_image(&node.image)
            .await
            .context("Failed to inspect image")?
            .id
            .context("Image has no ID")?;

        new_ids.insert(node.service.clone(), id);
    }

    // Services that have a genuinely new image and haven't previously failed.
    let needs_new_image: HashSet<String> = nodes
        .iter()
        .filter(|n| {
            let new_id = new_ids[&n.service].as_str();
            n.old_image_id != new_id && !failed.contains(&failed_key(&n.name, new_id))
        })
        .map(|n| n.service.clone())
        .collect();

    if needs_new_image.is_empty() {
        info!(project = %project, "All containers up to date");
        return Ok(());
    }

    // Extend the affected set to include every service that (transitively)
    // depends on a service that is being updated — they must be restarted too.
    let affected = propagate_affected(&nodes, &needs_new_image);

    let affected_nodes: Vec<&ContainerNode> =
        nodes.iter().filter(|n| affected.contains(&n.service)).collect();

    let sorted = topo_sort(&affected_nodes)
        .with_context(|| format!("Could not resolve update order for project {}", project))?;

    info!(
        project = %project,
        new_image = needs_new_image.len(),
        restart_only = sorted.len() - needs_new_image.len(),
        "Update plan ready"
    );

    // Stop all affected containers in reverse dependency order and park them
    // under a backup name so their original names are free for the new containers.
    let mut backups: Vec<(&ContainerNode, String)> = Vec::new();
    for node in sorted.iter().rev() {
        info!(container = %node.name, "Stopping");
        docker
            .stop_container(&node.id, None::<StopContainerOptions>)
            .await
            .with_context(|| format!("Failed to stop {}", node.name))?;

        let backup_name = format!("{}_conti_backup", node.name);
        docker
            .rename_container(
                &node.id,
                RenameContainerOptions {
                    name: backup_name.clone(),
                },
            )
            .await
            .with_context(|| format!("Failed to rename {}", node.name))?;

        backups.push((node, backup_name));
    }

    // Recreate containers in dependency order (dependencies before dependents).
    let mut started: Vec<(&ContainerNode, String)> = Vec::new();
    for node in &sorted {
        let image = &node.image;
        match recreate(docker, &node.inspect, &node.name, image, node.startup_timeout).await {
            Ok(new_id) => {
                info!(container = %node.name, id = %short_sha(&new_id), "Started");
                started.push((node, new_id));
            }
            Err(e) => {
                error!(container = %node.name, "Failed to recreate: {:#}", e);
                mark_update_failed(&node.name, &new_ids[&node.service]);
                group_rollback(docker, &started, &backups).await;
                bail!("Project update failed at '{}', rollback attempted", node.name);
            }
        }
    }

    // All containers are running — remove the backups.
    for (node, backup_name) in &backups {
        if let Err(e) = docker.remove_container(backup_name, None).await {
            warn!("Could not remove backup container {}: {}", backup_name, e);
        } else {
            info!(container = %node.name, "Update successful");
        }
    }

    info!(project = %project, "All containers updated successfully");
    Ok(())
}

// Stops and force-removes all successfully started new containers, then
// renames all backup containers back to their original names and starts them.
async fn group_rollback(
    docker: &Docker,
    started: &[(&ContainerNode, String)],
    backups: &[(&ContainerNode, String)],
) {
    for (node, new_id) in started.iter().rev() {
        if let Err(e) = docker.stop_container(new_id, None::<StopContainerOptions>).await {
            warn!("Rollback: could not stop {}: {}", node.name, e);
        }
        force_remove(docker, new_id).await;
    }

    for (node, _backup_name) in backups {
        if let Err(e) = docker
            .rename_container(
                &node.id,
                RenameContainerOptions {
                    name: node.name.clone(),
                },
            )
            .await
        {
            warn!("Rollback: could not rename {} back: {}", node.name, e);
            continue;
        }

        if let Err(e) = docker
            .start_container(&node.id, None::<StartContainerOptions<String>>)
            .await
        {
            warn!("Rollback: could not restart {}: {}", node.name, e);
        } else {
            info!(container = %node.name, "Rollback: container running again");
        }
    }
}

// Marks all transitive dependents of initially affected services as affected.
fn propagate_affected(nodes: &[ContainerNode], initially: &HashSet<String>) -> HashSet<String> {
    let mut affected = initially.clone();
    let mut changed = true;

    while changed {
        changed = false;
        for node in nodes {
            if !affected.contains(&node.service)
                && node.depends_on.iter().any(|dep| affected.contains(dep))
            {
                affected.insert(node.service.clone());
                changed = true;
            }
        }
    }

    affected
}

// Kahn's algorithm — returns nodes ordered so every dependency appears
// before the services that depend on it.
fn topo_sort<'a>(nodes: &[&'a ContainerNode]) -> Result<Vec<&'a ContainerNode>> {
    let index: HashMap<&str, usize> =
        nodes.iter().enumerate().map(|(i, n)| (n.service.as_str(), i)).collect();

    let mut in_degree = vec![0usize; nodes.len()];
    // adj[i] holds the indices of nodes that depend on node i.
    let mut adj: Vec<Vec<usize>> = vec![vec![]; nodes.len()];

    for (i, node) in nodes.iter().enumerate() {
        for dep in &node.depends_on {
            if let Some(&j) = index.get(dep.as_str()) {
                adj[j].push(i);
                in_degree[i] += 1;
            }
        }
    }

    let mut queue: VecDeque<usize> =
        (0..nodes.len()).filter(|&i| in_degree[i] == 0).collect();

    let mut result = Vec::new();
    while let Some(i) = queue.pop_front() {
        result.push(nodes[i]);
        for &j in &adj[i] {
            in_degree[j] -= 1;
            if in_degree[j] == 0 {
                queue.push_back(j);
            }
        }
    }

    if result.len() != nodes.len() {
        bail!("Circular dependency detected");
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Image pull
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Container recreation
// ---------------------------------------------------------------------------

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

    if let Err(e) = docker
        .start_container(&created.id, None::<StartContainerOptions<String>>)
        .await
    {
        force_remove(docker, &created.id).await;
        return Err(anyhow::Error::from(e).context("Failed to start container"));
    }

    if let Err(e) = wait_for_ready(docker, &created.id, info, startup_timeout).await {
        force_remove(docker, &created.id).await;
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

// ---------------------------------------------------------------------------
// Health / readiness check
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Rollback (standalone)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Failed update tracking
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn force_remove(docker: &Docker, id: &str) {
    let _ = docker
        .remove_container(
            id,
            Some(RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
}

fn short_sha(id: &str) -> &str {
    let s = id.strip_prefix("sha256:").unwrap_or(id);
    &s[..s.len().min(12)]
}
