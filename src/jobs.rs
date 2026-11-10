use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(any(target_os = "linux", test))]
use std::time::Instant;

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: u32,
    pub unit: String,
    pub running: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Failure {
    pub unit: String,
    /// The unit's last journal lines, oldest first.
    pub lines: Vec<String>,
}

/// What systemd is doing at one instant.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub jobs: Vec<Job>,
    /// Job id to the ids of the jobs it waits for.
    pub waits: HashMap<u32, Vec<u32>>,
    pub failed: Vec<Failure>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// One entry per ancestor below the root: whether that ancestor has a later sibling, so its
    /// vertical guide continues through this row.
    pub guides: Vec<bool>,
    pub depth: usize,
    pub last: bool,
    pub label: String,
    pub running: bool,
    pub id: u32,
}

/// `dev-disk-by\x2duuid-1234.device` as `dev-disk-by-uuid-1234.device`, and a service without
/// its suffix.
pub fn label(unit: &str) -> String {
    let mut out = String::with_capacity(unit.len());
    let mut rest = unit.strip_suffix(".service").unwrap_or(unit);
    while let Some(at) = rest.find("\\x") {
        out.push_str(&rest[..at]);
        let hex = rest.get(at + 2..at + 4).and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(byte) if byte.is_ascii_graphic() => {
                out.push(byte as char);
                rest = &rest[at + 4..];
            }
            _ => {
                out.push_str("\\x");
                rest = &rest[at + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

struct Node {
    job: usize,
    kids: Vec<Node>,
    active: bool,
}

/// The pending jobs as a tree: a job's children are the jobs it waits for. A job that several
/// wait for hangs under the one nearest a root, and a waiting job with a single child gives its
/// place to that child, so the rows are the roots, the branch points and what is running.
pub fn rows(snap: &Snapshot) -> Vec<Row> {
    let index: HashMap<u32, usize> = snap.jobs.iter().enumerate().map(|(i, j)| (j.id, i)).collect();
    let awaited: HashSet<u32> = snap.waits.values().flatten().copied().collect();
    let mut roots: Vec<usize> = (0..snap.jobs.len()).filter(|&i| !awaited.contains(&snap.jobs[i].id)).collect();
    if roots.is_empty() {
        roots = (0..snap.jobs.len()).collect();
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); snap.jobs.len()];
    let mut seen: HashSet<usize> = roots.iter().copied().collect();
    let mut queue: VecDeque<usize> = roots.iter().copied().collect();
    while let Some(i) = queue.pop_front() {
        for id in snap.waits.get(&snap.jobs[i].id).into_iter().flatten() {
            if let Some(&k) = index.get(id)
                && seen.insert(k)
            {
                children[i].push(k);
                queue.push_back(k);
            }
        }
    }

    fn build(i: usize, root: bool, snap: &Snapshot, children: &[Vec<usize>]) -> Vec<Node> {
        let mut kids: Vec<Node> = children[i].iter().flat_map(|&k| build(k, false, snap, children)).collect();
        let running = snap.jobs[i].running;
        if !(root || running || kids.len() != 1) {
            return kids;
        }
        kids.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| snap.jobs[a.job].unit.cmp(&snap.jobs[b.job].unit)));
        let active = running || kids.iter().any(|k| k.active);
        vec![Node { job: i, kids, active }]
    }
    let mut top: Vec<Node> = roots.iter().flat_map(|&r| build(r, true, snap, &children)).collect();
    top.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| snap.jobs[a.job].unit.cmp(&snap.jobs[b.job].unit)));

    fn flatten(node: &Node, guides: &[bool], last: bool, snap: &Snapshot, out: &mut Vec<Row>) {
        let job = &snap.jobs[node.job];
        out.push(Row {
            guides: guides.to_vec(),
            depth: guides.len(),
            last,
            label: label(&job.unit),
            running: job.running,
            id: job.id,
        });
        let mut below = guides.to_vec();
        below.push(false);
        for (n, kid) in node.kids.iter().enumerate() {
            let last_kid = n + 1 == node.kids.len();
            *below.last_mut().expect("pushed above") = !last_kid;
            flatten(kid, &below, last_kid, snap, out);
        }
    }
    let mut out = Vec::new();
    for node in &top {
        flatten(node, &[], true, snap, &mut out);
    }
    out
}

/// How long each job has been running, and the slowest ones that finished.
#[cfg(any(target_os = "linux", test))]
#[derive(Default)]
pub struct Clock {
    started: HashMap<u32, (String, Instant)>,
    finished: Vec<(String, f32)>,
}

#[cfg(any(target_os = "linux", test))]
impl Clock {
    pub fn observe(&mut self, snap: &Snapshot, now: Instant) {
        let running: HashSet<u32> = snap.jobs.iter().filter(|j| j.running).map(|j| j.id).collect();
        for job in snap.jobs.iter().filter(|j| j.running) {
            self.started.entry(job.id).or_insert_with(|| (job.unit.clone(), now));
        }
        let done: Vec<u32> = self.started.keys().filter(|id| !running.contains(id)).copied().collect();
        for id in done {
            let (unit, at) = self.started.remove(&id).expect("key taken from the map");
            self.finished.push((label(&unit), now.duration_since(at).as_secs_f32()));
        }
        self.finished.sort_by(|a, b| b.1.total_cmp(&a.1));
        self.finished.truncate(32);
    }

    pub fn elapsed(&self, id: u32, now: Instant) -> Option<f32> {
        self.started.get(&id).map(|(_, at)| now.duration_since(*at).as_secs_f32())
    }

    /// Slowest first. A job gone within one poll never ran long enough to be worth a row.
    pub fn slowest(&self) -> impl Iterator<Item = &(String, f32)> {
        self.finished.iter().filter(|(_, secs)| *secs >= 0.3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u32, unit: &str, running: bool) -> Job {
        Job { id, unit: unit.into(), running }
    }

    fn snap(jobs: Vec<Job>, waits: &[(u32, &[u32])]) -> Snapshot {
        Snapshot { jobs, waits: waits.iter().map(|(k, v)| (*k, v.to_vec())).collect(), failed: Vec::new() }
    }

    #[test]
    fn label_unescapes_and_drops_the_service_suffix() {
        assert_eq!(label("NetworkManager.service"), "NetworkManager");
        assert_eq!(label("dev-disk-by\\x2duuid-12.device"), "dev-disk-by-uuid-12.device");
        assert_eq!(label("odd\\xzz.mount"), "odd\\xzz.mount");
    }

    #[test]
    fn a_chain_of_waiting_jobs_collapses_onto_what_runs() {
        let s = snap(
            vec![
                job(1, "graphical.target", false),
                job(2, "multi-user.target", false),
                job(3, "basic.target", false),
                job(4, "slow.service", true),
            ],
            &[(1, &[2]), (2, &[3]), (3, &[4])],
        );
        let rows = rows(&s);
        let got: Vec<(usize, &str)> = rows.iter().map(|r| (r.depth, r.label.as_str())).collect();
        assert_eq!(got, [(0, "graphical.target"), (1, "slow")]);
    }

    #[test]
    fn a_branch_point_stays_with_running_children_first() {
        let s = snap(
            vec![
                job(1, "graphical.target", false),
                job(2, "multi-user.target", false),
                job(3, "b.service", false),
                job(4, "a.service", true),
                job(5, "c.service", true),
            ],
            &[(1, &[2]), (2, &[3, 4]), (3, &[5])],
        );
        let rows = rows(&s);
        let got: Vec<(usize, &str, bool)> = rows.iter().map(|r| (r.depth, r.label.as_str(), r.last)).collect();
        assert_eq!(
            got,
            [(0, "graphical.target", true), (1, "multi-user.target", true), (2, "a", false), (2, "c", true)]
        );
        assert_eq!(rows[2].guides, [false, true]);
    }

    #[test]
    fn a_job_awaited_twice_shows_once_and_a_cycle_ends() {
        let s = snap(
            vec![job(1, "a.target", false), job(2, "b.target", false), job(3, "c.service", true)],
            &[(1, &[2, 3]), (2, &[3, 1])],
        );
        let rows = rows(&s);
        assert_eq!(rows.iter().filter(|r| r.label == "c").count(), 1);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn the_clock_times_a_job_from_first_seen_running_to_gone() {
        let t0 = Instant::now();
        let mut clock = Clock::default();
        clock.observe(&snap(vec![job(1, "slow.service", true)], &[]), t0);
        let t1 = t0 + std::time::Duration::from_secs(2);
        assert_eq!(clock.elapsed(1, t1), Some(2.0));
        clock.observe(&snap(vec![], &[]), t1);
        assert_eq!(clock.slowest().collect::<Vec<_>>(), [&("slow".to_string(), 2.0)]);
    }
}
