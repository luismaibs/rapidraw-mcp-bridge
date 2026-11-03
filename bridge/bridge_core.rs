//! Scheduling between the MCP pipe and the editor window.
//!
//! Calls that only read state or stop work (`status`, `list_folders`, `cancel_export`, `job_status`,
//! `cancel_job`) are answered at once, even while a long job runs. Everything else is a *job* that takes a
//! turn: one at a time, in arrival order, because the editor is one window with one selected photo and two
//! edits must never overlap.
//!
//! A caller waits for its job for a while. If the job is still going, the caller gets the job's id instead of
//! a timeout, and the job keeps running on its own: `job_status` follows it (progress, partial results, the
//! final result) and `cancel_job` stops it. A job is not lost when the agent that asked disappears.
//!
//! A turn is only handed back when the window has answered (or was reloaded), so an overdue job is never run
//! over. A caller that left before its job got a turn is skipped, since nobody can ever collect that result.
//!
//! The window is behind a trait so all of this can be tested without the editor.
use super::{bridge_log, wire};
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    io::{Error, Result},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{Mutex as AsyncMutex, oneshot},
    time::timeout,
};

/// Where requests go. The app implements it with a Tauri event to the main window.
pub trait Window: Send + Sync + 'static {
    /// Hand one request to the editor window. Err when there is no window to receive it.
    fn deliver(&self, request: Value) -> Result<()>;
}

/// Runs on a finished job's result before it is stored (the app checks the disk there).
pub type Finalize = Box<dyn FnOnce(&mut Value) + Send + 'static>;

#[derive(Clone, Copy)]
pub struct Limits {
    /// How long `status` and the other immediate calls may wait for the window.
    pub immediate: Duration,
    /// How long to wait for the window to register itself after the editor starts.
    pub startup: Duration,
    /// How long a caller waits for its job before it is handed the job's id.
    pub detach: Duration,
    /// How long the window may stay silent (no progress, no answer) before a job counts as overdue.
    pub budget: fn(&str) -> Duration,
    /// How long a running job may ignore a cancel request before it is reported as cancelled anyway.
    pub cancel_grace: Duration,
}

impl Limits {
    pub fn production() -> Self {
        // RAPIDRAW_MCP_DETACH_S shortens the wait so a job hand-off can be tried without a very long job.
        let detach = std::env::var("RAPIDRAW_MCP_DETACH_S")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|s| (1..=600).contains(s))
            .unwrap_or(45);
        Self {
            immediate: Duration::from_secs(8),
            startup: Duration::from_secs(90),
            detach: Duration::from_secs(detach),
            budget: |op| Duration::from_secs(wire::op_timeout(op)),
            cancel_grace: Duration::from_secs(90),
        }
    }
}

const KEEP_FINISHED: usize = 12;
/// Longest a `job_status` call may wait for its job to finish.
const MAX_WAIT_S: u64 = 40;
const KEEP_FOR: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Queued,
    Running,
    Finished,
}

struct Job {
    op: String,
    call: String,
    created: Instant,
    finished: Option<Instant>,
    phase: Phase,
    /// Id of the request in the window while the job runs.
    request: Option<String>,
    /// Last sign of life from the window (a progress report or the start).
    activity: Instant,
    overdue: bool,
    cancel: bool,
    cancel_at: Option<Instant>,
    /// The cancel went unanswered past the grace period: reported as cancelled while the window catches up.
    forced: bool,
    /// The caller was handed the job id, so the job no longer depends on it staying connected.
    detached: bool,
    progress: Option<Value>,
    partial: Option<Value>,
    result: Option<Value>,
    finalize: Option<Finalize>,
}

pub struct Core<W: Window> {
    window: W,
    limits: Limits,
    ready: AtomicBool,
    session: Mutex<Option<String>>,
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    queue: AsyncMutex<()>,
    jobs: Mutex<HashMap<String, Job>>,
    waiting: AtomicUsize,
}

fn code(name: &str, text: impl std::fmt::Display) -> Error {
    Error::other(format!("[{name}] {text}"))
}

fn reloaded() -> Value {
    json!({"error": "[ui_reloaded] The editor window was reloaded while this was running. Check the state and retry."})
}

fn state_of(job: &Job) -> &'static str {
    match job.phase {
        Phase::Queued => "queued",
        Phase::Running if job.forced => "cancelled",
        Phase::Running if job.cancel => "cancelling",
        Phase::Running => "running",
        Phase::Finished => match job.result.as_ref() {
            Some(v) if v["cancelled"] == true => "cancelled",
            Some(v) if v["error"].is_string() => "failed",
            _ => "done",
        },
    }
}

/// Drop guard: a caller that goes away (its future is dropped) gives up its place in the queue.
struct Attached<W: Window> {
    core: Arc<Core<W>>,
    id: String,
    armed: bool,
}

impl<W: Window> Drop for Attached<W> {
    fn drop(&mut self) {
        if self.armed {
            self.core.abandon(&self.id);
        }
    }
}

impl<W: Window> Core<W> {
    pub fn new(window: W, limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            window,
            limits,
            ready: AtomicBool::new(false),
            session: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            queue: AsyncMutex::new(()),
            jobs: Mutex::new(HashMap::new()),
            waiting: AtomicUsize::new(0),
        })
    }

    /// The editor window announces itself. A new session id means the page was reloaded: whatever it was
    /// asked before is gone, so those jobs end now instead of waiting for a timeout.
    pub fn register(&self, session: &str) {
        let changed = {
            let mut current = self.session.lock().unwrap();
            let changed = current.as_deref().is_some_and(|old| old != session);
            *current = Some(session.to_string());
            changed
        };
        self.ready.store(true, Ordering::SeqCst);
        if changed {
            let dropped: Vec<_> = self.pending.lock().unwrap().drain().collect();
            bridge_log::editor(&format!("editor window reloaded; {} request(s) in flight were lost", dropped.len()));
            for (_, tx) in dropped {
                let _ = tx.send(reloaded());
            }
        }
    }

    /// The window's answer to a request.
    pub fn reply(&self, id: &str, result: Value) {
        let waiting = self.pending.lock().unwrap().remove(id);
        match waiting {
            Some(tx) => {
                let _ = tx.send(result);
            }
            None => bridge_log::editor(&format!("reply {id} ignored: nobody is waiting for it any more")),
        }
    }

    /// The window reports how far a request has got. It counts as a sign of life, so a job that keeps
    /// reporting is never called overdue however long it takes.
    pub fn progress(&self, request: &str, progress: Value, partial: Option<Value>) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.values_mut().find(|j| j.request.as_deref() == Some(request)) {
            job.activity = Instant::now();
            job.progress = Some(progress);
            if partial.is_some() {
                job.partial = partial;
            }
        }
    }

    /// What the bridge is doing, for `status`.
    pub fn snapshot(&self) -> Value {
        let jobs = self.jobs.lock().unwrap();
        let running = jobs.iter().find(|(_, j)| j.phase == Phase::Running).map(|(id, j)| {
            json!({
                "job": id,
                "op": j.op,
                "call": j.call,
                "for_s": j.created.elapsed().as_secs(),
                "overdue": j.overdue,
            })
        });
        json!({
            "protocol": wire::PROTOCOL,
            "running": running,
            "queued": self.waiting.load(Ordering::SeqCst),
        })
    }

    fn view(&self, id: &str, job: &Job) -> Value {
        let mut out = json!({
            "job": id,
            "op": job.op,
            "state": state_of(job),
            "elapsed_s": job.created.elapsed().as_secs(),
        });
        if let Some(p) = &job.progress {
            out["progress"] = p.clone();
        }
        match job.phase {
            Phase::Finished => match &job.result {
                Some(Value::Object(map)) => {
                    for (k, v) in map {
                        out[k] = v.clone();
                    }
                }
                Some(other) => out["result"] = other.clone(),
                None => {}
            },
            Phase::Queued => {
                out["message"] = json!("Queued behind other work. It runs on its own once the editor is free; follow it with job_status and do not repeat the call.");
            }
            Phase::Running => {
                if let Some(p) = &job.partial {
                    out["partial"] = p.clone();
                }
                if job.overdue {
                    out["quiet_for_s"] = json!(job.activity.elapsed().as_secs());
                    out["warning"] = json!("No sign of progress for a long time; the editor window may be frozen (a GPU out-of-memory error is the usual cause). If it stays like this, call restart_editor.");
                }
                out["message"] = json!("Still running. The work continues even if you stop waiting: follow it with job_status, stop it with cancel_job, and do not repeat the call.");
                if job.forced {
                    out["forced"] = json!(true);
                    out["message"] = json!("Cancelled, but the editor has not confirmed it: it may still be finishing the step it was on, and the queue stays closed until it answers. If it is frozen, restart_editor recovers it.");
                }
            }
        }
        out
    }

    pub fn job_status(&self, id: &str) -> Value {
        let jobs = self.jobs.lock().unwrap();
        match jobs.get(id) {
            Some(job) => self.view(id, job),
            None => json!({"error": format!("[unknown_job] No job {id:?}. Finished jobs are kept for an hour, and only the last {KEEP_FINISHED}.")}),
        }
    }

    /// `job_status`, optionally holding the answer until the job finishes (or `wait` passes), so an agent can
    /// wait on a long job without a sleep of its own.
    async fn wait_for_job(&self, id: &str, wait: Duration) -> Value {
        let until = Instant::now() + wait;
        loop {
            let view = self.job_status(id);
            let live = matches!(view["state"].as_str(), Some("queued" | "running" | "cancelling"));
            if !live || Instant::now() >= until {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub fn cancel_job(&self, id: &str) -> Value {
        let mut jobs = self.jobs.lock().unwrap();
        let Some(job) = jobs.get_mut(id) else {
            return json!({"error": format!("[unknown_job] No job {id:?}.")});
        };
        match job.phase {
            Phase::Finished => {
                let mut v = self.view(id, jobs.get(id).unwrap());
                v["note"] = json!("It had already finished; nothing to cancel.");
                v
            }
            Phase::Queued => {
                job.phase = Phase::Finished;
                job.finished = Some(Instant::now());
                job.result = Some(json!({"cancelled": true, "message": "Cancelled before it started."}));
                bridge_log::editor(&format!("{} {} cancelled while queued", job.call, job.op));
                self.view(id, jobs.get(id).unwrap())
            }
            Phase::Running => {
                job.cancel = true;
                job.cancel_at.get_or_insert_with(Instant::now);
                bridge_log::editor(&format!("{} {} cancel requested", job.call, job.op));
                let mut v = self.view(id, jobs.get(id).unwrap());
                v["note"] = json!("The editor was asked to stop. It finishes the step it is on (one photo, one segmentation); job_status then shows what was done.");
                v
            }
        }
    }

    fn abandon(&self, id: &str) {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(id)
            && job.phase == Phase::Queued
            && !job.detached
        {
            job.cancel = true;
        }
    }

    /// Forget old finished jobs.
    fn purge(&self) {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.retain(|_, j| j.finished.is_none_or(|t| t.elapsed() < KEEP_FOR));
        let mut finished: Vec<(String, Instant)> = jobs
            .iter()
            .filter_map(|(id, j)| j.finished.map(|t| (id.clone(), t)))
            .collect();
        if finished.len() > KEEP_FINISHED {
            finished.sort_by_key(|(_, t)| *t);
            for (id, _) in finished.iter().take(finished.len() - KEEP_FINISHED) {
                jobs.remove(id);
            }
        }
    }

    pub async fn run(self: &Arc<Self>, op: &str, call: &str, args: Map<String, Value>, finalize: Option<Finalize>) -> Result<Value> {
        let job_arg = args.get("job").and_then(Value::as_str).unwrap_or("").to_string();
        match op {
            "job_status" => {
                let wait = args.get("wait_s").and_then(Value::as_u64).unwrap_or(0).min(MAX_WAIT_S);
                return Ok(self.wait_for_job(&job_arg, Duration::from_secs(wait)).await);
            }
            "cancel_job" => return Ok(self.cancel_job(&job_arg)),
            _ => {}
        }
        self.wait_ready(call, op).await?;
        if wire::is_immediate(op) {
            let mut reply = self.exchange(op, call, args, self.limits.immediate).await?;
            if op == "status" && reply.is_object() {
                reply["bridge"] = self.snapshot();
            }
            return Ok(reply);
        }

        let job_id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        self.jobs.lock().unwrap().insert(
            job_id.clone(),
            Job {
                op: op.to_string(),
                call: call.to_string(),
                created: Instant::now(),
                finished: None,
                phase: Phase::Queued,
                request: None,
                activity: Instant::now(),
                overdue: false,
                cancel: false,
                cancel_at: None,
                forced: false,
                detached: false,
                progress: None,
                partial: None,
                result: None,
                finalize,
            },
        );
        self.purge();
        let mut attached = Attached { core: Arc::clone(self), id: job_id.clone(), armed: true };
        let me = Arc::clone(self);
        let id_for_task = job_id.clone();
        tokio::spawn(async move { me.take_turn(id_for_task, args).await });

        let give_up = Instant::now() + self.limits.detach;
        loop {
            enum Next {
                Done(Value),
                Silent,
                Wait,
            }
            let next = {
                let jobs = self.jobs.lock().unwrap();
                match jobs.get(&job_id) {
                    None => Next::Done(json!({"error": "[internal] the bridge lost this job"})),
                    Some(job) if job.phase == Phase::Finished => Next::Done(job.result.clone().unwrap_or(Value::Null)),
                    Some(job) if job.overdue => Next::Silent,
                    Some(_) => Next::Wait,
                }
            };
            match next {
                Next::Done(value) => {
                    attached.armed = false;
                    return Ok(value);
                }
                Next::Silent => {
                    attached.armed = false;
                    if let Some(job) = self.jobs.lock().unwrap().get_mut(&job_id) {
                        job.detached = true;
                    }
                    return Err(code(
                        "ui_not_responding",
                        format!("The editor did not answer {op} in time. Its work may still be running (job {job_id}): follow it with job_status, do not repeat the call."),
                    ));
                }
                Next::Wait => {}
            }
            if Instant::now() >= give_up {
                attached.armed = false;
                let mut jobs = self.jobs.lock().unwrap();
                return match jobs.get_mut(&job_id) {
                    Some(job) => {
                        job.detached = true;
                        Ok(self.view(&job_id, jobs.get(&job_id).unwrap()))
                    }
                    None => Err(code("internal", "the bridge lost this job")),
                };
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }

    async fn wait_ready(&self, call: &str, op: &str) -> Result<()> {
        if self.ready.load(Ordering::SeqCst) {
            return Ok(());
        }
        bridge_log::editor(&format!("{call} {op} waiting for the editor window to register"));
        let started = Instant::now();
        while !self.ready.load(Ordering::SeqCst) {
            if started.elapsed() > self.limits.startup {
                return Err(code(
                    "ui_not_ready",
                    format!("The editor window did not register with the bridge in {} s; it may still be starting or failed to load.", self.limits.startup.as_secs()),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }

    fn send(&self, op: &str, call: &str, args: Map<String, Value>) -> Result<(String, oneshot::Receiver<Value>)> {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        if let Err(e) = self.window.deliver(json!({"id": id, "op": op, "call": call, "args": args})) {
            self.pending.lock().unwrap().remove(&id);
            return Err(code("ui_not_responding", format!("cannot reach the editor window: {e}")));
        }
        Ok((id, rx))
    }

    async fn exchange(&self, op: &str, call: &str, args: Map<String, Value>, limit: Duration) -> Result<Value> {
        let (id, rx) = self.send(op, call, args)?;
        match timeout(limit, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Ok(reloaded()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bridge_log::editor(&format!("{call} {op} no answer from the window in {} s", limit.as_secs()));
                Err(code(
                    "ui_not_responding",
                    format!("The editor window did not answer in {} s. It may be frozen or still loading; if it stays like this, restart_editor recovers it.", limit.as_secs()),
                ))
            }
        }
    }

    fn finish(&self, id: &str, mut value: Value) {
        let finalize = self.jobs.lock().unwrap().get_mut(id).and_then(|j| j.finalize.take());
        if value.get("error").is_none()
            && let Some(finalize) = finalize
        {
            finalize(&mut value);
        }
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(id) {
            job.phase = Phase::Finished;
            job.finished = Some(Instant::now());
            job.request = None;
            bridge_log::editor(&format!(
                "{} {} finished after {} s: {}",
                job.call,
                job.op,
                job.created.elapsed().as_secs(),
                match value.get("error").and_then(Value::as_str) {
                    Some(e) => format!("error: {e}"),
                    None if value["cancelled"] == true => "cancelled".to_string(),
                    None => "ok".to_string(),
                }
            ));
            job.result = Some(value);
        }
    }

    async fn take_turn(self: Arc<Self>, job_id: String, args: Map<String, Value>) {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let turn = self.queue.lock().await;
        self.waiting.fetch_sub(1, Ordering::SeqCst);

        let claimed = {
            let mut jobs = self.jobs.lock().unwrap();
            match jobs.get_mut(&job_id) {
                None => None,
                Some(job) if job.phase == Phase::Finished => None, // cancelled while it waited
                Some(job) if job.cancel => {
                    job.phase = Phase::Finished;
                    job.finished = Some(Instant::now());
                    job.result = Some(json!({"cancelled": true, "message": "Cancelled before it started."}));
                    bridge_log::editor(&format!("{} {} skipped: the caller stopped waiting before its turn", job.call, job.op));
                    None
                }
                Some(job) => {
                    job.phase = Phase::Running;
                    job.activity = Instant::now();
                    Some((job.op.clone(), job.call.clone(), (self.limits.budget)(&job.op)))
                }
            }
        };
        let Some((op, call, budget)) = claimed else {
            return;
        };

        match self.send(&op, &call, args) {
            Err(e) => self.finish(&job_id, json!({"error": e.to_string()})),
            Ok((request, mut reply)) => {
                if let Some(job) = self.jobs.lock().unwrap().get_mut(&job_id) {
                    job.request = Some(request.clone());
                }
                let mut stop_sent = false;
                let value = loop {
                    match timeout(Duration::from_millis(100), &mut reply).await {
                        Ok(Ok(v)) => break v,
                        Ok(Err(_)) => break reloaded(),
                        Err(_) => {
                            let (cancel, overdue_now) = {
                                let mut jobs = self.jobs.lock().unwrap();
                                match jobs.get_mut(&job_id) {
                                    Some(job) => {
                                        let newly = !job.overdue && job.activity.elapsed() > budget;
                                        if newly {
                                            job.overdue = true;
                                        }
                                        if !job.forced && job.cancel_at.is_some_and(|at| at.elapsed() > self.limits.cancel_grace) {
                                            job.forced = true;
                                            bridge_log::editor(&format!(
                                                "{} {} cancel not honoured in {} s: reported as cancelled while the editor catches up",
                                                job.call,
                                                job.op,
                                                self.limits.cancel_grace.as_secs()
                                            ));
                                        }
                                        (job.cancel, newly)
                                    }
                                    None => (false, false),
                                }
                            };
                            if overdue_now {
                                bridge_log::editor(&format!(
                                    "{call} {op} TIMEOUT: no sign of life for {} s; the turn stays closed until the window answers or reloads",
                                    budget.as_secs()
                                ));
                            }
                            if cancel && !stop_sent {
                                stop_sent = true;
                                let mut stop = Map::new();
                                stop.insert("request".into(), json!(request));
                                let _ = self.send("cancel", &call, stop);
                            }
                        }
                    }
                };
                self.finish(&job_id, value);
            }
        }
        drop(turn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tokio::sync::mpsc;

    struct Fake(mpsc::UnboundedSender<Value>);
    impl Window for Fake {
        fn deliver(&self, request: Value) -> Result<()> {
            self.0.send(request).map_err(|_| Error::other("no window"))
        }
    }

    /// How long the fake editor takes per operation, in ms. `None` never answers.
    fn delay(op: &str) -> Option<u64> {
        match op {
            "status" | "list_folders" | "cancel_export" | "cancel" => Some(5),
            "slow" | "overrun" | "chatty" | "stoppable" => Some(400),
            "quick" => Some(60),
            "hang" => None,
            _ => Some(30),
        }
    }

    fn tiny(op: &str) -> Duration {
        match op {
            "overrun" | "chatty" => Duration::from_millis(150),
            _ => Duration::from_secs(5),
        }
    }

    struct Rig {
        core: Arc<Core<Fake>>,
        started: Arc<Mutex<Vec<(String, Instant)>>>,
        finished: Arc<Mutex<Vec<(String, Instant)>>>,
        peak: Arc<AtomicUsize>,
    }

    /// A core wired to a fake editor that answers after `delay`, reports progress when it is `chatty`,
    /// stops when told to if it is `stoppable`, and records what it saw.
    fn rig(limits: Limits) -> Rig {
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let core = Core::new(Fake(tx), limits);
        core.register("window-1");
        let started = Arc::new(Mutex::new(Vec::new()));
        let finished = Arc::new(Mutex::new(Vec::new()));
        let peak = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let stops: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let (c, s, f, p) = (core.clone(), started.clone(), finished.clone(), peak.clone());
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let (c, s, f, p, active, stops) = (c.clone(), s.clone(), f.clone(), p.clone(), active.clone(), stops.clone());
                tokio::spawn(async move {
                    let op = request["op"].as_str().unwrap().to_string();
                    let id = request["id"].as_str().unwrap().to_string();
                    if op == "cancel" {
                        stops.lock().unwrap().insert(request["args"]["request"].as_str().unwrap().to_string());
                        c.reply(&id, json!({"ok": true}));
                        return;
                    }
                    let queued = !wire::is_immediate(&op);
                    if queued {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        p.fetch_max(now, Ordering::SeqCst);
                    }
                    s.lock().unwrap().push((op.clone(), Instant::now()));
                    let Some(ms) = delay(&op) else { return };
                    let mut cancelled = false;
                    if op == "chatty" || op == "stoppable" {
                        // Works for a long time, reporting progress, and stops early if asked.
                        for step in 0..(ms / 40) {
                            if op == "stoppable" && stops.lock().unwrap().contains(&id) {
                                cancelled = true;
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(40)).await;
                            c.progress(&id, json!({"done": step + 1}), Some(json!([step])));
                        }
                    } else {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }
                    f.lock().unwrap().push((op.clone(), Instant::now()));
                    if queued {
                        active.fetch_sub(1, Ordering::SeqCst);
                    }
                    c.reply(&id, if cancelled { json!({"cancelled": true, "done": 1}) } else { json!({"ok": true, "op": op}) });
                });
            }
        });
        Rig { core, started, finished, peak }
    }

    fn limits() -> Limits {
        Limits {
            immediate: Duration::from_millis(300),
            startup: Duration::from_millis(150),
            detach: Duration::from_millis(1500),
            budget: tiny,
            cancel_grace: Duration::from_millis(300),
        }
    }

    fn no_args() -> Map<String, Value> {
        Map::new()
    }

    fn job_arg(id: &str) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("job".into(), json!(id));
        m
    }

    #[tokio::test]
    async fn status_answers_while_a_long_job_runs() {
        let rig = rig(limits());
        let core = rig.core.clone();
        let long = tokio::spawn(async move { core.run("slow", "c1", no_args(), None).await });
        tokio::time::sleep(Duration::from_millis(60)).await;
        let began = Instant::now();
        let status = rig.core.run("status", "c2", no_args(), None).await.unwrap();
        assert!(began.elapsed() < Duration::from_millis(150), "status waited {:?}", began.elapsed());
        assert_eq!(status["bridge"]["running"]["op"], "slow");
        assert!(long.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn queued_calls_never_overlap_and_keep_their_order() {
        let rig = rig(limits());
        let mut jobs = Vec::new();
        for n in 0..5 {
            let core = rig.core.clone();
            jobs.push(tokio::spawn(async move { core.run("quick", &format!("c{n}"), no_args(), None).await }));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        for job in jobs {
            assert!(job.await.unwrap().is_ok());
        }
        assert_eq!(rig.peak.load(Ordering::SeqCst), 1, "two operations reached the editor at once");
        assert_eq!(rig.finished.lock().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn work_that_overran_keeps_the_turn_until_the_window_answers() {
        let rig = rig(limits());
        // "overrun" is allowed 150 ms of silence but the editor needs 400 ms.
        let first = rig.core.run("overrun", "a", no_args(), None).await;
        let message = first.unwrap_err().to_string();
        assert!(message.contains("[ui_not_responding]"), "{message}");
        // The caller was told, but the editor is still busy: the next call must wait for it.
        let second = rig.core.run("quick", "b", no_args(), None).await;
        assert!(second.is_ok());
        let finished = rig.finished.lock().unwrap().clone();
        let started = rig.started.lock().unwrap().clone();
        let end_a = finished.iter().find(|(n, _)| n == "overrun").unwrap().1;
        let start_b = started.iter().find(|(n, _)| n == "quick").unwrap().1;
        assert!(start_b >= end_a, "the next call started while the first was still running");
    }

    #[tokio::test]
    async fn a_caller_that_left_is_skipped() {
        let rig = rig(limits());
        let core = rig.core.clone();
        let blocker = tokio::spawn(async move { core.run("slow", "a", no_args(), None).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        // The second caller gives up (its future is dropped) while the long job still holds the editor.
        let core = rig.core.clone();
        let waiter = tokio::time::timeout(Duration::from_millis(80), core.run("quick", "b", no_args(), None)).await;
        assert!(waiter.is_err(), "the second call should still be waiting behind the long job");
        blocker.await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let seen: Vec<String> = rig.started.lock().unwrap().iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(seen, vec!["slow".to_string()], "an abandoned call reached the editor: {seen:?}");
    }

    #[tokio::test]
    async fn a_reloaded_window_ends_what_was_in_flight() {
        let rig = rig(limits());
        let core = rig.core.clone();
        let stuck = tokio::spawn(async move { core.run("hang", "a", no_args(), None).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        rig.core.register("window-2");
        let result = stuck.await.unwrap().unwrap();
        assert!(result["error"].as_str().unwrap().contains("[ui_reloaded]"), "{result}");
        // And the editor is usable again.
        assert!(rig.core.run("quick", "b", no_args(), None).await.is_ok());
    }

    #[tokio::test]
    async fn no_window_gives_a_clear_error() {
        let (tx, _rx) = mpsc::unbounded_channel::<Value>();
        let core = Core::new(Fake(tx), limits());
        let message = core.run("quick", "a", no_args(), None).await.unwrap_err().to_string();
        assert!(message.contains("[ui_not_ready]"), "{message}");
    }

    #[tokio::test]
    async fn a_silent_window_fails_immediate_calls_quickly() {
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let core = Core::new(Fake(tx), limits());
        core.register("window-1");
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let began = Instant::now();
        let message = core.run("status", "a", no_args(), None).await.unwrap_err().to_string();
        assert!(message.contains("[ui_not_responding]"), "{message}");
        assert!(began.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_window_that_cannot_be_reached_is_reported() {
        let (tx, rx) = mpsc::unbounded_channel::<Value>();
        let core = Core::new(Fake(tx), limits());
        core.register("window-1");
        drop(rx);
        let result = core.run("quick", "a", no_args(), None).await.unwrap();
        assert!(result["error"].as_str().unwrap().contains("[ui_not_responding]"), "{result}");
    }

    #[tokio::test]
    async fn a_long_job_hands_back_its_id_and_can_be_followed() {
        let mut l = limits();
        l.detach = Duration::from_millis(100);
        let rig = rig(l);
        let envelope = rig.core.run("slow", "a", no_args(), None).await.unwrap();
        assert_eq!(envelope["state"], "running", "{envelope}");
        let id = envelope["job"].as_str().unwrap().to_string();
        assert_eq!(rig.core.job_status(&id)["state"], "running");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let done = rig.core.job_status(&id);
        assert_eq!(done["state"], "done", "{done}");
        assert_eq!(done["op"], "slow");
        assert_eq!(done["ok"], true);
    }

    #[tokio::test]
    async fn a_detached_job_survives_its_caller_disappearing() {
        let mut l = limits();
        l.detach = Duration::from_millis(60);
        let rig = rig(l);
        let core = rig.core.clone();
        let blocker = tokio::spawn(async move { core.run("slow", "a", no_args(), None).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Queued behind the blocker, it gets its id after the detach time, then the caller walks away.
        let envelope = rig.core.run("quick", "b", no_args(), None).await.unwrap();
        assert_eq!(envelope["state"], "queued", "{envelope}");
        let id = envelope["job"].as_str().unwrap().to_string();
        blocker.await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(rig.core.job_status(&id)["state"], "done");
        let seen: Vec<String> = rig.started.lock().unwrap().iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(seen, vec!["slow".to_string(), "quick".to_string()]);
    }

    #[tokio::test]
    async fn a_job_that_keeps_reporting_is_never_overdue() {
        // "chatty" takes 400 ms with only 150 ms of silence allowed, but reports every 40 ms.
        let rig = rig(limits());
        let result = rig.core.run("chatty", "a", no_args(), None).await.unwrap();
        assert_eq!(result["ok"], true, "{result}");
    }

    #[tokio::test]
    async fn progress_and_partial_results_are_visible_while_it_runs() {
        let mut l = limits();
        l.detach = Duration::from_millis(100);
        let rig = rig(l);
        let envelope = rig.core.run("chatty", "a", no_args(), None).await.unwrap();
        let id = envelope["job"].as_str().unwrap().to_string();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let view = rig.core.job_status(&id);
        assert!(view["progress"]["done"].as_u64().unwrap() >= 1, "{view}");
        assert!(view["partial"].is_array(), "{view}");
    }

    #[tokio::test]
    async fn a_queued_job_can_be_cancelled_and_never_reaches_the_editor() {
        let mut l = limits();
        l.detach = Duration::from_millis(60);
        let rig = rig(l);
        let core = rig.core.clone();
        let blocker = tokio::spawn(async move { core.run("slow", "a", no_args(), None).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let envelope = rig.core.run("quick", "b", no_args(), None).await.unwrap();
        let id = envelope["job"].as_str().unwrap().to_string();
        let cancelled = rig.core.run("cancel_job", "c", job_arg(&id), None).await.unwrap();
        assert_eq!(cancelled["state"], "cancelled", "{cancelled}");
        blocker.await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        let seen: Vec<String> = rig.started.lock().unwrap().iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(seen, vec!["slow".to_string()], "a cancelled job reached the editor: {seen:?}");
    }

    #[tokio::test]
    async fn a_running_job_is_told_to_stop() {
        let mut l = limits();
        l.detach = Duration::from_millis(60);
        let rig = rig(l);
        // "stoppable" would work for 400 ms; cancelling must end it well before that.
        let envelope = rig.core.run("stoppable", "a", no_args(), None).await.unwrap();
        let id = envelope["job"].as_str().unwrap().to_string();
        let asked = Instant::now();
        let cancelling = rig.core.run("cancel_job", "b", job_arg(&id), None).await.unwrap();
        assert_eq!(cancelling["state"], "cancelling", "{cancelling}");
        loop {
            let view = rig.core.job_status(&id);
            if view["state"] == "cancelled" {
                break;
            }
            assert!(asked.elapsed() < Duration::from_millis(350), "the job did not stop: {view}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn a_cancel_the_window_never_honours_is_reported_as_cancelled() {
        let mut l = limits();
        l.detach = Duration::from_millis(60);
        let rig = rig(l);
        // "hang" never answers and never looks at the stop request.
        let envelope = rig.core.run("hang", "a", no_args(), None).await.unwrap();
        let id = envelope["job"].as_str().unwrap().to_string();
        let cancelling = rig.core.run("cancel_job", "b", job_arg(&id), None).await.unwrap();
        assert_eq!(cancelling["state"], "cancelling", "{cancelling}");
        tokio::time::sleep(Duration::from_millis(700)).await;
        let view = rig.core.job_status(&id);
        assert_eq!(view["state"], "cancelled", "{view}");
        assert_eq!(view["forced"], true, "{view}");
        // The turn is still held by the silent window: it shows up as busy, not as free.
        assert_eq!(rig.core.snapshot()["running"]["job"], id.as_str());
    }

    #[tokio::test]
    async fn the_result_is_checked_before_it_is_stored() {
        let rig = rig(limits());
        let finalize: Finalize = Box::new(|v| v["checked_on_disk"] = json!(true));
        let result = rig.core.run("quick", "a", no_args(), Some(finalize)).await.unwrap();
        assert_eq!(result["checked_on_disk"], true, "{result}");
    }

    #[tokio::test]
    async fn job_status_can_wait_for_the_job_to_finish() {
        let mut l = limits();
        l.detach = Duration::from_millis(60);
        let rig = rig(l);
        let envelope = rig.core.run("slow", "a", no_args(), None).await.unwrap();
        let id = envelope["job"].as_str().unwrap().to_string();
        let mut args = job_arg(&id);
        args.insert("wait_s".into(), json!(5));
        let began = Instant::now();
        let view = rig.core.run("job_status", "b", args, None).await.unwrap();
        assert_eq!(view["state"], "done", "{view}");
        assert!(began.elapsed() < Duration::from_secs(2), "it should return when the job ends, not when the wait ends");
    }

    #[tokio::test]
    async fn job_status_gives_up_waiting_and_reports_the_live_state() {
        let mut l = limits();
        l.detach = Duration::from_millis(60);
        let rig = rig(l);
        let envelope = rig.core.run("hang", "a", no_args(), None).await.unwrap();
        let id = envelope["job"].as_str().unwrap().to_string();
        let mut args = job_arg(&id);
        args.insert("wait_s".into(), json!(1));
        let began = Instant::now();
        let view = rig.core.run("job_status", "b", args, None).await.unwrap();
        assert_eq!(view["state"], "running", "{view}");
        assert!(began.elapsed() >= Duration::from_millis(900));
    }

    #[tokio::test]
    async fn an_unknown_job_is_an_error() {
        let rig = rig(limits());
        let view = rig.core.run("job_status", "a", job_arg("nope"), None).await.unwrap();
        assert!(view["error"].as_str().unwrap().contains("[unknown_job]"), "{view}");
    }
}
