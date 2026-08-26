//! Per-server scheduled tasks: persistence, the fire loop, and execution.

use crate::state::AppState;
use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub name: String,
    pub cron: String,
    /// command | power | backup
    pub action: String,
    /// command text for `command`, start/stop/restart/kill for `power`
    #[serde(default)]
    pub payload: Option<String>,
    pub enabled: bool,
    #[serde(default)]
    pub last_fired: Option<String>,
    #[serde(default)]
    pub last_result: Option<String>,
}

pub struct Scheduler {
    pub app: Arc<AppState>,
}

impl Scheduler {
    fn path(&self) -> std::path::PathBuf {
        self.app.cfg.data_dir.join("schedules.json")
    }

    fn load(&self) -> std::collections::BTreeMap<String, Vec<Schedule>> {
        std::fs::read_to_string(self.path())
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    fn save(&self, all: &std::collections::BTreeMap<String, Vec<Schedule>>) {
        if let Ok(json) = serde_json::to_string_pretty(all) {
            let _ = std::fs::write(self.path(), json);
        }
    }

    pub fn list(&self, server_id: &str) -> Vec<Schedule> {
        self.load().get(server_id).cloned().unwrap_or_default()
    }

    pub fn add(&self, server_id: &str, mut task: Schedule) -> Result<Schedule> {
        crate::cron::Cron::parse(&task.cron)?;
        task.id = new_id();
        let mut all = self.load();
        all.entry(server_id.to_string()).or_default().push(task.clone());
        self.save(&all);
        Ok(task)
    }

    pub fn update(
        &self,
        server_id: &str,
        task_id: &str,
        mut patch: SchedulePatch,
    ) -> Result<Schedule> {
        if let Some(c) = &patch.cron {
            crate::cron::Cron::parse(c)?;
        }
        let mut all = self.load();
        let tasks = all
            .get_mut(server_id)
            .ok_or_else(|| anyhow::anyhow!("no schedules for server"))?;
        let t = tasks
            .iter_mut()
            .find(|t| t.id == task_id)
            .ok_or_else(|| anyhow::anyhow!("unknown schedule {task_id}"))?;
        if let Some(v) = patch.name.take() {
            t.name = v;
        }
        if let Some(v) = patch.cron.take() {
            t.cron = v;
        }
        if let Some(v) = patch.action.take() {
            t.action = v;
        }
        if let Some(v) = patch.payload {
            t.payload = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(v) = patch.enabled {
            t.enabled = v;
        }
        let updated = t.clone();
        self.save(&all);
        Ok(updated)
    }

    pub fn delete(&self, server_id: &str, task_id: &str) -> Result<()> {
        let mut all = self.load();
        if let Some(tasks) = all.get_mut(server_id) {
            tasks.retain(|t| t.id != task_id);
            self.save(&all);
        }
        Ok(())
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct SchedulePatch {
    pub name: Option<String>,
    pub cron: Option<String>,
    pub action: Option<String>,
    pub payload: Option<String>,
    pub enabled: Option<bool>,
}

fn new_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..10).map(|_| rng.sample(rand::distributions::Alphanumeric) as char).collect()
}

/// Spawn the 30-second fire loop.
pub fn spawn(app: Arc<AppState>) {
    tokio::spawn(async move {
        let sched = Scheduler { app: app.clone() };
        loop {
            tick(&sched).await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

async fn tick(sched: &Scheduler) {
    let now = Local::now();
    let minute_key = now.format("%Y%m%d%H%M").to_string();
    let all = sched.load();

    for (server_id, tasks) in all {
        for mut task in tasks {
            if !task.enabled {
                continue;
            }
            let Ok(cron) = crate::cron::Cron::parse(&task.cron) else { continue };
            if !cron.matches(&now) || task.last_fired.as_deref() == Some(minute_key.as_str()) {
                continue;
            }
            task.last_fired = Some(minute_key.clone());
            persist_task(sched, &server_id, &task);

            let s2 = Arc::new(Scheduler { app: sched.app.clone() });
            let sid = server_id.clone();
            tokio::spawn(async move {
                execute(&s2, &sid, &mut task).await;
            });
        }
    }
}

fn persist_task(sched: &Scheduler, server_id: &str, updated: &Schedule) {
    let mut all = sched.load();
    if let Some(tasks) = all.get_mut(server_id) {
        if let Some(t) = tasks.iter_mut().find(|t| t.id == updated.id) {
            *t = updated.clone();
        }
        sched.save(&all);
    }
}

async fn execute(sched: &Scheduler, server_id: &str, task: &mut Schedule) {
    let state = &sched.app;
    let result: Result<()> = match task.action.as_str() {
        "command" => match task.payload.clone() {
            Some(cmd) => crate::docker::send_command(state.clone(), server_id, &cmd).await,
            None => Err(anyhow::anyhow!("command payload missing")),
        },
        "power" => {
            let Some(action) = task.payload.clone() else {
                return finish(sched, server_id, task, Err(anyhow::anyhow!("power payload missing")));
            };
            let action = match action.as_str() {
                "start" => nucleus_core::PowerAction::Start,
                "stop" => nucleus_core::PowerAction::Stop,
                "restart" => nucleus_core::PowerAction::Restart,
                "kill" => nucleus_core::PowerAction::Kill,
                other => return finish(sched, server_id, task, Err(anyhow::anyhow!("bad power action '{other}'"))),
            };
            crate::docker::power(state.clone(), server_id, action, None).await
        }
        "backup" => crate::backups::create_backup(state.clone(), server_id.to_string())
            .await
            .map(|_| ()),
        other => Err(anyhow::anyhow!("unknown action '{other}'")),
    };

    finish(sched, server_id, task, result);
}

fn finish(sched: &Scheduler, server_id: &str, task: &mut Schedule, result: Result<()>) {
    let rt = match sched.app.servers.get(server_id) {
        Some(r) => r.clone(),
        None => return,
    };
    match &result {
        Ok(()) => {
            task.last_result = Some(format!("ok at {}", Local::now().format("%H:%M:%S")));
            rt.push_log(&format!("[schedule] '{}' completed", task.name));
        }
        Err(e) => {
            task.last_result = Some(format!(
                "failed at {}: {e:#}",
                Local::now().format("%H:%M:%S")
            ));
            rt.push_log(&format!("[schedule] '{}' failed: {e:#}", task.name));
        }
    }
    persist_task(sched, server_id, task);
}

/// For API responses: attach a computed next-run timestamp when possible.
pub fn with_next_run(mut v: serde_json::Value, task: &Schedule) -> serde_json::Value {
    if let Ok(cron) = crate::cron::Cron::parse(&task.cron) {
        if let Some(next) = cron.next_after(&Local::now()) {
            let ts: DateTime<Local> = next;
            v["next_run"] = serde_json::json!(ts.format("%Y-%m-%d %H:%M %Z").to_string());
        }
    }
    if !task.enabled {
        v["next_run"] = serde_json::json!(null);
    }
    v
}
