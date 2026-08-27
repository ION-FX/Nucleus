use crate::state::{save_registry, AppState, NetPrev, ServerRuntime};
use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use nucleus_core::{PowerAction, ServerStatus};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn container_name(id: &str) -> String {
    format!("nucleus-{id}")
}

pub(crate) async fn image_exists(docker: &bollard::Docker, image: &str) -> bool {
    docker.inspect_image(image).await.is_ok()
}

pub async fn pull_image(docker: &bollard::Docker, image: &str) -> Result<()> {
    let opts = bollard::image::CreateImageOptions {
        from_image: image.to_string(),
        ..Default::default()
    };
    let mut stream = docker.create_image(Some(opts), None, None);
    while let Some(item) = stream.next().await {
        item.map_err(|e| anyhow!("pulling {image}: {e}"))?;
    }
    Ok(())
}

/// Pterodactyl-style built-in environment for a server container: egg
/// variables plus SERVER_MEMORY / SERVER_IP / SERVER_PORT derived from the
/// spec (games and install scripts expect these to always be present).
pub(crate) fn container_env(spec: &nucleus_core::CreateServerRequest) -> Vec<String> {
    let mut env: Vec<String> = spec
        .env
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    if !spec.env.contains_key("SERVER_MEMORY") {
        env.push(format!("SERVER_MEMORY={}", spec.limits.mem_mb));
    }
    if !spec.env.contains_key("SERVER_IP") {
        env.push("SERVER_IP=0.0.0.0".to_string());
    }
    if !spec.env.contains_key("SERVER_PORT") {
        if let Some(p) = spec.ports.first() {
            env.push(format!("SERVER_PORT={}", p.container));
        }
    }
    env
}

/// Startups created before built-in substitution existed may contain an
/// empty `-XmxM` artifact; repair it from the server's memory limit.
fn sanitize_startup(startup: &str, mem_mb: u64) -> String {
    startup.replace("-XmxM", &format!("-Xmx{mem_mb}M"))
}

async fn build_host_config(
    state: &AppState,
    rt: &ServerRuntime,
) -> Result<(
    bollard::secret::HostConfig,
    std::collections::HashMap<String, std::collections::HashMap<(), ()>>,
)> {
    let dir = rt.server_dir(&state.cfg);
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;

    let binds = vec![
        format!(
            "{}:/data",
            dir.canonicalize().unwrap_or(dir.clone()).display()
        ),
        // Pterodactyl-style aliases so stock egg scripts/startups work.
        format!(
            "{}:/home/container",
            dir.canonicalize().unwrap_or(dir.clone()).display()
        ),
    ];

    let mut port_bindings = std::collections::HashMap::new();
    let mut exposed = std::collections::HashMap::new();
    for p in &rt.spec.ports {
        let key = format!("{}/{}", p.container, p.proto);
        port_bindings.insert(
            key.clone(),
            Some(vec![bollard::models::PortBinding {
                host_ip: None,
                host_port: Some(p.host.to_string()),
            }]),
        );
        exposed.insert(key, std::collections::HashMap::<(), ()>::new());
    }

    let nano_cpus = (rt.spec.limits.cpu_cores * 1_000_000_000f64) as i64;
    let mut host_config = bollard::secret::HostConfig {
        binds: Some(binds),
        port_bindings: Some(port_bindings),
        memory: Some((rt.spec.limits.mem_mb.max(128) * 1024 * 1024) as i64),
        nano_cpus: Some(nano_cpus),
        ..Default::default()
    };
    if rt.spec.limits.pids_limit > 0 {
        host_config.pids_limit = Some(rt.spec.limits.pids_limit as i64);
    }
    if rt.spec.limits.disk_mb > 0 {
        let mut storage_opt = std::collections::HashMap::new();
        storage_opt.insert(
            "size".to_string(),
            format!("{}G", rt.spec.limits.disk_mb / 1024),
        );
        host_config.storage_opt = Some(storage_opt);
    }
    Ok((host_config, exposed))
}

pub async fn create_server(
    state: Arc<AppState>,
    mut req: nucleus_core::CreateServerRequest,
) -> Result<ServerStatus> {
    if req.id.len() < 3
        || !req
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(anyhow!("invalid server id"));
    }
    if state.servers.contains_key(&req.id) {
        return Err(anyhow!("server {} already exists", req.id));
    }

    let install_script = req.install_script.clone();
    let installer_image = req.installer_image.clone();

    let dir = state.cfg.servers_dir().join(&req.id);
    tokio::fs::create_dir_all(&dir).await?;

    // Standard env vars startup templates can rely on.
    let first_port = req.ports.first().map(|p| p.container).unwrap_or(25565);
    req.env
        .entry("SERVER_MEMORY".to_string())
        .or_insert_with(|| req.limits.mem_mb.to_string());
    req.env
        .entry("SERVER_PORT".to_string())
        .or_insert(first_port.to_string());

    let rt = Arc::new(ServerRuntime::new(req));
    if rt.spec.accept_eula {
        tokio::fs::write(dir.join("eula.txt"), "eula=true\n")
            .await
            .ok();
    }

    let name = container_name(&rt.spec.id);
    // Remove stale container from a previous life of this id.
    let _ = state
        .docker
        .remove_container(
            &name,
            Some(bollard::container::RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;

    if !image_exists(&state.docker, &rt.spec.image).await {
        tracing::info!("pulling image {}", rt.spec.image);
        pull_image(&state.docker, &rt.spec.image).await?;
    }

    let (host_config, exposed) = build_host_config(&state, &rt).await?;

    let config = bollard::container::Config {
        image: Some(rt.spec.image.clone()),
        entrypoint: Some(vec!["/bin/bash".into(), "-c".into()]),
        cmd: Some(vec![sanitize_startup(&rt.spec.startup, rt.spec.limits.mem_mb)]),
        env: Some(container_env(&rt.spec)),
        tty: Some(false),
        open_stdin: Some(true),
        attach_stdin: Some(true),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        working_dir: Some("/data".into()),
        user: Some("0:0".into()),
        exposed_ports: Some(exposed),
        host_config: Some(host_config),
        labels: Some(
            [("nucleus.server.id", rt.spec.id.as_str())]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ),
        ..Default::default()
    };

    let create_opts = bollard::container::CreateContainerOptions {
        name: name.clone(),
        platform: None,
    };
    state
        .docker
        .create_container(Some(create_opts), config)
        .await
        .with_context(|| format!("creating container {name}"))?;

    let status = rt.status();
    state.servers.insert(rt.spec.id.clone(), rt.clone());
    save_registry(&state.cfg, &state.servers);

    // Egg install script: run as a sidecar right after registration.
    if let Some(script) = install_script {
        crate::installer::store_script(
            &state.cfg,
            &rt.spec.id,
            crate::installer::StoredScript { script: script.clone(), image: installer_image.clone() },
        );
        let st2 = state.clone();
        let rt2 = rt.clone();
        tokio::spawn(async move {
            let _ = crate::installer::start_script_install(st2, rt2, script, installer_image);
        });
    }

    Ok(status)
}

pub async fn remove_server(state: Arc<AppState>, id: &str, purge_data: bool) -> Result<()> {
    let rt = state.get(id)?;
    if rt.running.load(Ordering::Relaxed) {
        power(state.clone(), id, PowerAction::Kill, None).await?;
    }
    let _ = state
        .docker
        .remove_container(
            &container_name(id),
            Some(bollard::container::RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await;
    state.servers.remove(id);
    save_registry(&state.cfg, &state.servers);
    if purge_data {
        let dir = rt.server_dir(&state.cfg);
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
    Ok(())
}

pub async fn status(state: Arc<AppState>, id: &str) -> Result<ServerStatus> {
    let rt = state.get(id)?;
    let mut st = rt.status();
    // Reconcile with actual container state (covers daemon restarts).
    if let Ok(inspect) = state
        .docker
        .inspect_container(&container_name(id), None)
        .await
    {
        if let Some(s) = inspect.state {
            st.running = s.running.unwrap_or(false);
            rt.running.store(st.running, Ordering::Relaxed);
            if let Some(code) = s.exit_code {
                if !st.running {
                    *rt.exit_code.lock().unwrap() = Some(code);
                    st.exit_code = Some(code);
                }
            }
        }
    }
    Ok(st)
}

pub async fn list_servers(state: Arc<AppState>) -> Vec<ServerStatus> {
    let mut out = Vec::new();
    for e in state.servers.iter() {
        out.push(e.value().status());
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Send a line to the container's process stdin (game console command).
pub async fn send_command(state: Arc<AppState>, id: &str, line: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let rt = state.get(id)?;
    // Take the writer out so no std MutexGuard lives across the await.
    let mut taken = rt.stdin.lock().unwrap().take();
    let Some(ref mut writer) = taken else {
        return Err(anyhow!("server not attached; is it running?"));
    };
    writer
        .write_all(format!("{line}\n").as_bytes())
        .await
        .context("writing to container stdin")?;
    writer.flush().await.context("flushing container stdin")?;
    *rt.stdin.lock().unwrap() = taken;
    Ok(())
}

async fn attach_and_feed(state: Arc<AppState>, rt: Arc<ServerRuntime>) {
    let id = rt.spec.id.clone();
    let name = container_name(&id);
    let options = bollard::container::AttachContainerOptions {
        detach_keys: Some("".to_string()),
        stream: Some(true),
        stdin: Some(true),
        stdout: Some(true),
        stderr: Some(true),
        logs: Some(true),
    };
    match state.docker.attach_container(&name, Some(options)).await {
        Ok(res) => {
            *rt.stdin.lock().unwrap() = Some(res.input);

            let mut output = res.output;
            while let Some(chunk) = output.next().await {
                match chunk {
                    Ok(out) => {
                        let text = match out {
                            bollard::container::LogOutput::StdErr { message }
                            | bollard::container::LogOutput::StdOut { message }
                            | bollard::container::LogOutput::Console { message }
                            | bollard::container::LogOutput::StdIn { message } => {
                                String::from_utf8_lossy(&message).to_string()
                            }
                        };
                        for line in text.lines() {
                            if !line.trim().is_empty() {
                                rt.push_log(line);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(%e, "attach stream ended");
                        break;
                    }
                }
            }
            *rt.stdin.lock().unwrap() = None;
        }
        Err(e) => {
            tracing::warn!(%e, server = %id, "attach failed; falling back to logs");
            fallback_log_pump(state, rt).await;
        }
    }
}

async fn fallback_log_pump(state: Arc<AppState>, rt: Arc<ServerRuntime>) {
    let name = container_name(&rt.spec.id);
    let opts = bollard::container::LogsOptions {
        follow: true,
        stdout: true,
        stderr: true,
        since: 0,
        until: 0,
        timestamps: false,
        tail: "1000".to_string(),
    };
    let mut stream = state.docker.logs(&name, Some(opts));
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(out) => {
                let text = match out {
                    bollard::container::LogOutput::StdErr { message }
                    | bollard::container::LogOutput::StdOut { message }
                    | bollard::container::LogOutput::Console { message }
                    | bollard::container::LogOutput::StdIn { message } => {
                        String::from_utf8_lossy(&message).to_string()
                    }
                };
                for line in text.lines() {
                    if !line.trim().is_empty() {
                        rt.push_log(line);
                    }
                }
            }
            Err(_) => break,
        }
    }
}

async fn watch_exit(state: Arc<AppState>, rt: Arc<ServerRuntime>) {
    let name = container_name(&rt.spec.id);
    let mut stream = state.docker.wait_container(
        &name,
        None::<bollard::container::WaitContainerOptions<String>>,
    );
    if let Some(item) = stream.next().await {
        let code = item.map(|s| s.status_code).unwrap_or(-1);
        rt.running.store(false, Ordering::Relaxed);
        *rt.exit_code.lock().unwrap() = Some(code);
        *rt.stdin.lock().unwrap() = None;
        rt.push_log(&format!("[nucleus] container exited with code {code}"));
        let _ = rt.log_tx.send(crate::state::decode_exit_event(code));
        let _ = state.exit_tx.send((rt.spec.id.clone(), code));
    }
}

async fn graceful_stop(state: Arc<AppState>, rt: Arc<ServerRuntime>) {
    use tokio::io::AsyncWriteExt;
    if let Some(cmd) = rt.spec.stop_command.clone() {
        let mut taken = { rt.stdin.lock().unwrap().take() };
        let mut sent = false;
        if let Some(writer) = taken.as_mut() {
            sent = writer
                .write_all(format!("{cmd}\n").as_bytes())
                .await
                .is_ok()
                && writer.flush().await.is_ok();
            *rt.stdin.lock().unwrap() = taken;
        }
        if sent {
            for _ in 0..30 {
                if !rt.running.load(Ordering::Relaxed) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
        }
    }
    let _ = state
        .docker
        .stop_container(
            &container_name(&rt.spec.id),
            Some(bollard::container::StopContainerOptions { t: 20 }),
        )
        .await;
}

pub async fn power(
    state: Arc<AppState>,
    id: &str,
    action: PowerAction,
    _command: Option<String>,
) -> Result<()> {
    let rt = state.get(id)?;
    let name = container_name(id);
    match action {
        PowerAction::Start => {
            if rt.running.load(Ordering::Relaxed) {
                return Err(anyhow!("already running"));
            }
            *rt.exit_code.lock().unwrap() = None;

            let (host_config, exposed) = build_host_config(&state, &rt).await?;

            // Recreate + start, trying /bin/bash first and falling back to
            // /bin/sh for minimal images (busybox etc.) that ship without bash.
            let mut started = false;
            let mut last_err = None;
            for shell in ["/bin/bash", "/bin/sh"] {
                let cfg = bollard::container::Config {
                    image: Some(rt.spec.image.clone()),
                    entrypoint: Some(vec![shell.to_string(), "-c".into()]),
                    cmd: Some(vec![sanitize_startup(
                        &rt.spec.startup,
                        rt.spec.limits.mem_mb,
                    )]),
                    env: Some(container_env(&rt.spec)),
                    tty: Some(false),
                    open_stdin: Some(true),
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    working_dir: Some("/data".into()),
                    user: Some("0:0".into()),
                    exposed_ports: Some(exposed.clone()),
                    host_config: Some(host_config.clone()),
                    labels: Some(
                        [("nucleus.server.id", rt.spec.id.as_str())]
                            .iter()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect(),
                    ),
                    ..Default::default()
                };
                let _ = state
                    .docker
                    .remove_container(
                        &name,
                        Some(bollard::container::RemoveContainerOptions {
                            force: true,
                            ..Default::default()
                        }),
                    )
                    .await;
                let create_opts = bollard::container::CreateContainerOptions {
                    name: name.clone(),
                    platform: None,
                };
                state.docker.create_container(Some(create_opts), cfg).await?;

                match state
                    .docker
                    .start_container(
                        &name,
                        None::<bollard::container::StartContainerOptions<String>>,
                    )
                    .await
                {
                    Ok(()) => {
                        started = true;
                        break;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        let lower = msg.to_lowercase();
                        // Missing interpreter -> try the next shell; anything
                        // else (port conflicts, image issues) is fatal.
                        if lower.contains("no such file") || lower.contains("not found") {
                            last_err = Some(msg);
                            continue;
                        }
                        return Err(anyhow!("Docker responded with {e}"));
                    }
                }
            }
            if !started {
                return Err(anyhow!(
                    "could not start container (no usable shell?): {}",
                    last_err.unwrap_or_default()
                ));
            }
            rt.running.store(true, Ordering::Relaxed);
            *rt.container_id.lock().unwrap() = Some(name.clone());

            let st1 = state.clone();
            let r1 = rt.clone();
            tokio::spawn(async move { attach_and_feed(st1, r1).await });
            let st2 = state.clone();
            let r2 = rt.clone();
            tokio::spawn(async move { watch_exit(st2, r2).await });

            rt.push_log("[nucleus] container started");
            Ok(())
        }
        PowerAction::Stop => {
            graceful_stop(state, rt).await;
            Ok(())
        }
        PowerAction::Restart => {
            if rt.running.load(Ordering::Relaxed) {
                graceful_stop(state.clone(), rt.clone()).await;
            }
            Box::pin(power(state, id, PowerAction::Start, None)).await
        }
        PowerAction::Kill => {
            let _ = state
                .docker
                .kill_container(
                    &name,
                    None::<bollard::container::KillContainerOptions<String>>,
                )
                .await;
            Ok(())
        }
    }
}

// ── live resource stats ──────────────────────────────────────────────────

#[derive(serde::Serialize, Clone)]
pub struct ServerStats {
    pub running: bool,
    pub cpu_percent: f64,
    pub mem_used_mb: f64,
    pub mem_limit_mb: f64,
    pub mem_percent: f64,
    pub net_rx_kbps: f64,
    pub net_tx_kbps: f64,
}

fn zero_stats(running: bool) -> ServerStats {
    ServerStats {
        running,
        cpu_percent: 0.0,
        mem_used_mb: 0.0,
        mem_limit_mb: 0.0,
        mem_percent: 0.0,
        net_rx_kbps: 0.0,
        net_tx_kbps: 0.0,
    }
}

/// One docker stats sample with delta computation against the previous poll.
pub async fn stats(state: Arc<AppState>, id: &str) -> Result<ServerStats> {
    let rt = state.get(id)?;
    let name = container_name(id);
    let st = status(state.clone(), id).await?;
    if !st.running {
        state.stats_prev.remove(id);
        return Ok(zero_stats(false));
    }

    let opts = bollard::container::StatsOptions { one_shot: false, stream: false };
    let mut stream = state.docker.stats(&name, Some(opts));
    let Some(sample) = stream.next().await else {
        return Ok(zero_stats(true));
    };
    let s: bollard::container::Stats = sample.map_err(|e| anyhow!("stats: {e}"))?;

    let cur_cpu = s.cpu_stats.cpu_usage.total_usage;
    let cur_sys = s.cpu_stats.system_cpu_usage.unwrap_or(0);
    let online = s.cpu_stats.online_cpus.unwrap_or(1) as f64;
    let mem_usage = s.memory_stats.usage.unwrap_or(0);
    let mem_limit = s.memory_stats.limit.unwrap_or(1);
    let cache = match &s.memory_stats.stats {
        Some(bollard::container::MemoryStatsStats::V1(v)) => v.cache,
        Some(bollard::container::MemoryStatsStats::V2(v)) => v.inactive_file,
        None => 0,
    };
    let (rx, tx) = s
        .networks
        .as_ref()
        .map(|n| {
            n.values().fold((0u64, 0u64), |(a, b), v| {
                (a + v.rx_bytes, b + v.tx_bytes)
            })
        })
        .unwrap_or((0, 0));

    // CPU% needs a previous sample; the stats stream provides precpu.
    let prev_cpu = s.precpu_stats.cpu_usage.total_usage;
    let prev_sys = s.precpu_stats.system_cpu_usage.unwrap_or(0);

    let cpu_pct = if prev_sys > 0 && cur_sys > prev_sys && cur_cpu >= prev_cpu {
        ((cur_cpu - prev_cpu) as f64 / (cur_sys - prev_sys) as f64) * online * 100.0
    } else {
        match state.stats_prev.get(id).map(|r| *r.value()) {
            Some((pc, ps)) if ps > 0 && cur_sys > ps => {
                ((cur_cpu.saturating_sub(pc)) as f64 / (cur_sys - ps) as f64) * online * 100.0
            }
            _ => 0.0,
        }
    };
    state.stats_prev.insert(id.to_string(), (cur_cpu, cur_sys));

    let net_delta = |now: u64, then: Option<(u64, std::time::Instant)>| -> f64 {
        match then {
            Some((prev_bytes, t)) => {
                let dt = t.elapsed().as_secs_f64();
                if dt > 0.05 {
                    (now.saturating_sub(prev_bytes)) as f64 / dt / 1024.0
                } else {
                    0.0
                }
            }
            None => 0.0,
        }
    };
    let prev_net = state.net_prev.get(id).map(|e| *e.value());
    let rx_rate = net_delta(rx, prev_net.map(|n| (n.rx, n.at)));
    let tx_rate = net_delta(tx, prev_net.map(|n| (n.tx, n.at)));
    state
        .net_prev
        .insert(id.to_string(), NetPrev { rx, tx, at: std::time::Instant::now() });

    let mem_used_mb = (mem_usage.saturating_sub(cache)) as f64 / 1024.0 / 1024.0;
    Ok(ServerStats {
        running: true,
        cpu_percent: cpu_pct.clamp(0.0, 100.0 * online),
        mem_used_mb,
        mem_limit_mb: mem_limit as f64 / 1024.0 / 1024.0,
        mem_percent: if mem_limit > 0 {
            (mem_used_mb / (mem_limit as f64 / 1024.0 / 1024.0)) * 100.0
        } else {
            0.0
        },
        net_rx_kbps: rx_rate.max(0.0),
        net_tx_kbps: tx_rate.max(0.0),
    })
}
