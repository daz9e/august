//! Fires due tasks (reminders, recurring jobs) by running them as turns in their chats.

use crate::db::{self, Task};
use crate::db::Db;
use async_trait::async_trait;
pub mod schedule;

use schedule::Schedule;
use std::sync::Arc;
use std::time::Duration;

const TICK: Duration = Duration::from_secs(20);

/// Whatever knows how to execute a due task (the gateway runs it as a chat turn).
#[async_trait]
pub trait TaskRunner: Send + Sync + 'static {
    async fn run_task(&self, task: Task) -> anyhow::Result<()>;
}

pub async fn run(db: Arc<Db>, runner: Arc<dyn TaskRunner>) {
    loop {
        if let Err(e) = tick(&db, &runner).await {
            eprintln!("scheduler: {e:#}");
        }
        tokio::time::sleep(TICK).await;
    }
}

async fn tick(db: &Db, runner: &Arc<dyn TaskRunner>) -> anyhow::Result<()> {
    let now = db::now();
    for task in db.due_tasks(now)? {
        // Move the schedule forward first, so a slow or failing run can't fire twice.
        db.finish_run(task.id, next_run(&task, now))?;
        let runner = runner.clone();
        tokio::spawn(async move {
            if let Err(e) = runner.run_task(task).await {
                eprintln!("scheduler: {e:#}");
            }
        });
    }
    Ok(())
}

/// `None` ends the task: one-shot, or a schedule that no longer parses.
fn next_run(task: &Task, now: i64) -> Option<i64> {
    Schedule::parse(&task.schedule).ok()?.next_after(now)
}
