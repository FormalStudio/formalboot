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
    let mut conn = RpcConn::new(DuplexConn::connect_to_bus(UnixAddr::new(SOCKET)?, false)?);
    // A call PID 1 reads in one go with the handshake's BEGIN sits unanswered until more bytes
    // follow it (systemd 261), which a busy PID 1 makes the usual case. The next call's bytes
    // get both answered, so the first is repeated until one is.
    let start = Instant::now();
    loop {
        let serial = send(&mut conn, "ListJobs", None::<u32>)?;
        match conn.wait_response(serial, Timeout::Duration(Duration::from_millis(50))) {
            Ok(_) => return Ok(conn),
            Err(Error::TimedOut) if start.elapsed() < Duration::from_secs(2) => {}
            Err(e) => return Err(e),
        }
    }
}

type Reply = rustbus::message_builder::MarshalledMessage;

fn send<A: Marshal + Signature>(conn: &mut RpcConn, method: &str, arg: Option<A>) -> Result<u32, Error> {
    let mut msg = MessageBuilder::new().call(method).on(PATH).with_interface(MANAGER).build();
    if let Some(arg) = arg {
        msg.body.push_param(arg)?;
    }
    conn.send_message(&mut msg)?.write_all().map_err(force_finish_on_error)
}

/// `None` for an error reply: a job that ended between two calls is not a broken connection.
fn receive(conn: &mut RpcConn, serial: u32) -> Result<Option<Reply>, Error> {
    let reply = conn.wait_response(serial, TIMEOUT)?;
    Ok((reply.typ != MessageType::Error).then_some(reply))
}

fn list_jobs(conn: &mut RpcConn) -> Result<Vec<JobEntry>, Error> {
    let serial = send(conn, "ListJobs", None::<u32>)?;
    Ok(receive(conn, serial)?.ok_or(Error::TimedOut)?.body.parser().get()?)
}

/// The jobs each of `ids` waits for. Every call is written before the first reply is read, so
/// the sweep costs PID 1 one pass over its socket, not a round trip per job.
fn blockers(conn: &mut RpcConn, ids: &[u32]) -> Result<Vec<(u32, Vec<u32>)>, Error> {
    let serials: Vec<u32> = ids.iter().map(|&id| send(conn, "GetJobBefore", Some(id))).collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for (&id, serial) in ids.iter().zip(serials) {
        if let Some(reply) = receive(conn, serial)? {
            let before: Vec<JobEntry> = reply.body.parser().get()?;
            out.push((id, before.into_iter().map(|j| j.id).collect()));
        }
    }
    Ok(out)
}

fn failed(conn: &mut RpcConn) -> Result<Vec<String>, Error> {
    let serial = send(conn, "ListUnitsFiltered", Some(vec!["failed"]))?;
    let units: Vec<UnitEntry> = receive(conn, serial)?.ok_or(Error::TimedOut)?.body.parser().get()?;
    Ok(units.into_iter().map(|u| u.name).collect())
}

/// The last two lines the journal has for the unit that say something the row does not.
fn journal(unit: &str) -> Vec<String> {
    let out = Command::new("journalctl").args(["-b", "-u", unit, "-n", "8", "-o", "cat", "--no-pager", "-q"]).output();
    let Ok(out) = out else { return Vec::new() };
    let own = format!("{unit}: ");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<String> = text
        .lines()
        .map(|l| l.strip_prefix(&own).unwrap_or(l))
        .filter(|l| !l.starts_with("Failed to start ") && !l.starts_with("Failed with result "))
        .map(str::to_string)
        .collect();
    lines.drain(..lines.len().saturating_sub(2));
    lines
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
        let mut swept: Option<Instant> = None;
        let mut failed_at: Option<Instant> = None;
        while let Ok(list) = list_jobs(&mut conn) {
            let ids: HashSet<u32> = list.iter().map(|j| j.id).collect();
            waits.retain(|id, _| ids.contains(id));
            let waiting: Vec<u32> = list.iter().filter(|j| j.state != "running").map(|j| j.id).collect();
            // A job's blockers are asked for once, when it first shows. One enqueued later can
            // come to block a job already asked about, which a sweep of them all picks up.
            let sweep = swept.is_none_or(|t| t.elapsed() >= Duration::from_secs(2));
            let ask: Vec<u32> = waiting.iter().copied().filter(|id| sweep || !waits.contains_key(id)).collect();
            if !ask.is_empty() {
                let Ok(found) = blockers(&mut conn, &ask) else { break };
                waits.extend(found);
                if sweep {
                    swept = Some(Instant::now());
                }
            }
            for before in waits.values_mut() {
                before.retain(|id| ids.contains(id));
            }
            if failed_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
                failed_at = Some(Instant::now());
                if let Ok(units) = failed(&mut conn) {
                    let skip = ignored
                        .get_or_insert_with(|| if shutdown { units.iter().cloned().collect() } else { HashSet::new() });
                    failures.retain(|f| units.contains(&f.unit));
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
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
