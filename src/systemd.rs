use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use nix::sys::socket::UnixAddr;
use rustbus::connection::Timeout;
use rustbus::connection::ll_conn::force_finish_on_error;
use rustbus::wire::ObjectPath;
use rustbus::{DuplexConn, Marshal, MessageBuilder, MessageType, RpcConn, Signature, Unmarshal};

use crate::jobs::{Failure, Job, Snapshot};

// PID 1's own socket: there is no bus daemon in the initrd, and none yet early in the real root.
const SOCKET: &str = "/run/systemd/private";
const PATH: &str = "/org/freedesktop/systemd1";
const MANAGER: &str = "org.freedesktop.systemd1.Manager";
const TIMEOUT: Timeout = Timeout::Duration(Duration::from_secs(2));

#[derive(Unmarshal, Signature)]
struct JobEntry {
    id: u32,
    unit: String,
    _kind: String,
    state: String,
    _job: ObjectPath<String>,
    _unit: ObjectPath<String>,
}

#[derive(Unmarshal, Signature)]
struct UnitEntry {
    name: String,
    _description: String,
    _load: String,
    _active: String,
    _sub: String,
    _following: String,
    _path: ObjectPath<String>,
    _job: u32,
    _job_kind: String,
    _job_path: ObjectPath<String>,
}

type Error = rustbus::connection::Error;

fn connect() -> Result<RpcConn, Error> {
    // No Hello: this is a peer connection to PID 1, not a bus.
    Ok(RpcConn::new(DuplexConn::connect_to_bus(UnixAddr::new(SOCKET)?, false)?))
}

fn call<A: Marshal + Signature>(
    conn: &mut RpcConn,
    method: &str,
    arg: Option<A>,
) -> Result<rustbus::message_builder::MarshalledMessage, Error> {
    let mut msg = MessageBuilder::new().call(method).on(PATH).with_interface(MANAGER).build();
    if let Some(arg) = arg {
        msg.body.push_param(arg)?;
    }
    let serial = conn.send_message(&mut msg)?.write_all().map_err(force_finish_on_error)?;
    let reply = conn.wait_response(serial, TIMEOUT)?;
    // A job that ended between two calls answers with an error, which is not a broken connection.
    if reply.typ == MessageType::Error { Err(Error::TimedOut) } else { Ok(reply) }
}

fn jobs(conn: &mut RpcConn, method: &str, id: Option<u32>) -> Result<Vec<JobEntry>, Error> {
    Ok(call(conn, method, id)?.body.parser().get()?)
}

fn failed(conn: &mut RpcConn) -> Result<Vec<String>, Error> {
    let units: Vec<UnitEntry> = call(conn, "ListUnitsFiltered", Some(vec!["failed"]))?.body.parser().get()?;
    Ok(units.into_iter().map(|u| u.name).collect())
}

fn journal(unit: &str) -> Vec<String> {
    let out = Command::new("journalctl").args(["-b", "-u", unit, "-n", "2", "-o", "cat", "--no-pager", "-q"]).output();
    let Ok(out) = out else { return Vec::new() };
    // systemd's own lines about a unit open with its name, which the row above already gives.
    let own = format!("{unit}: ");
    String::from_utf8_lossy(&out.stdout).lines().map(|l| l.strip_prefix(&own).unwrap_or(l).to_string()).collect()
}

/// Polls PID 1 and sends a snapshot whenever one differs from the last. The connection drops
/// when systemd re-executes into the real root, and is made again there.
pub fn watch(tx: Sender<Snapshot>, shutdown: bool) {
    let mut last = Snapshot::default();
    let mut failures: Vec<Failure> = Vec::new();
    // At shutdown, what had already failed during the session is not this screen's news.
    let mut ignored: Option<HashSet<String>> = None;
    loop {
        let Ok(mut conn) = connect() else {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        };
        let mut waits: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut ids: HashSet<u32> = HashSet::new();
        let mut stale = true;
        let mut edges_at: Option<Instant> = None;
        let mut failed_at: Option<Instant> = None;
        while let Ok(list) = jobs(&mut conn, "ListJobs", None) {
            let now: HashSet<u32> = list.iter().map(|j| j.id).collect();
            if now != ids {
                stale = true;
                waits.retain(|id, _| now.contains(id));
                for after in waits.values_mut() {
                    after.retain(|id| now.contains(id));
                }
                ids = now;
            }
            // One call per waiting job, to a PID 1 that is busy booting: no more than twice a second.
            if stale && edges_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(500)) {
                for job in list.iter().filter(|j| j.state != "running") {
                    if let Ok(after) = jobs(&mut conn, "GetJobBefore", Some(job.id)) {
                        waits.insert(job.id, after.into_iter().map(|j| j.id).filter(|id| ids.contains(id)).collect());
                    }
                }
                stale = false;
                edges_at = Some(Instant::now());
            }
            if failed_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
                failed_at = Some(Instant::now());
                if let Ok(units) = failed(&mut conn) {
                    let skip = ignored
                        .get_or_insert_with(|| if shutdown { units.iter().cloned().collect() } else { HashSet::new() });
                    for unit in units {
                        if !skip.contains(&unit) && !failures.iter().any(|f| f.unit == unit) {
                            failures.push(Failure { lines: journal(&unit), unit });
                        }
                    }
                }
            }
            let snapshot = Snapshot {
                jobs: list.into_iter().map(|j| Job { id: j.id, running: j.state == "running", unit: j.unit }).collect(),
                waits: waits.clone(),
                failed: failures.clone(),
            };
            if snapshot != last {
                last = snapshot.clone();
                if tx.send(snapshot).is_err() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}
