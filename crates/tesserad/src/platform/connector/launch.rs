//! Finite observations of the ORIGINAL socket connector using its retained
//! pidfd. No process is opened from a peer-supplied PID. This is not a launch
//! witness, unit/MainPID authentication, descriptor-writer proof or permission.
use super::*;
use nix::fcntl::{AtFlags, OFlag, openat};
use nix::sys::stat::{Mode, fstatat};
use std::io::Read;
use std::path::{Component, Path};
use std::time::{Duration, Instant};

mod roles;
pub(crate) use roles::RoleTable;

const FILE_LIMIT: usize = 16 * 1024;
const PATH_LIMIT: usize = 4096;
const BUDGET: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    Dead,
    Unavailable,
    Capacity,
    Deadline,
    ProcMount,
    PidNamespace,
    Malformed,
    Changed,
    CgroupMount,
    ImageAliased,
    ImageUnrecognized,
    #[cfg(feature = "routing-shadow")]
    Credentials,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Image {
    Launcher,
    Runtime,
    ReportAdapter,
    ForwardedCli,
    Other,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) process: ProcessKey,
    pub(crate) pid_namespace: ProcessKey,
    pub(crate) image: ProcessKey,
    pub(crate) image_kind: Image,
    pub(crate) unified_cgroup: String,
    /// Finite metadata observation, not a trusted unit invocation identifier.
    /// This snapshot does not retain the directory or exclude cgroup ABA.
    pub(crate) cgroup: ProcessKey,
    /// Membership under the observed process cgroup, NOT a manager-verified
    /// unit root. Unit authority and absence outside it remain unavailable.
    pub(crate) observed_subtree_singleton: bool,
    #[cfg(test)]
    pub(crate) fixture_membership: bool,
}

/// Retained directories keep lookup keys from being recycled while the
/// diagnostic observation is queued. This is still no manager/unit proof.
#[derive(Debug)]
pub(crate) struct Observation {
    pub(crate) snapshot: Snapshot,
    _proc_root: File,
    process_dir: File,
    _cgroup_dir: File,
    pub(crate) parents: Vec<Ancestor>,
}
impl Observation {
    #[cfg(feature = "routing-shadow")]
    pub(crate) fn retain_cgroup(&self) -> io::Result<File> {
        self._cgroup_dir.try_clone()
    }
}

/// At most three parent hops, pinned by process handles. An observed chain is
/// neither a reviewed topology nor proof of who wrote a socket's bytes.
#[derive(Debug)]
pub(crate) struct Ancestor {
    pub(crate) process: RetainedProcess,
    pub(crate) image_kind: Image,
    pub(crate) unified_cgroup: String,
    image: ProcessKey,
    directory: File,
}
impl Ancestor {
    pub(crate) fn recheck(&self) -> Result<(), Failure> {
        self.process.still_live().map_err(|_| Failure::Dead)?;
        let stat =
            fstatat(&self.directory, "exe", AtFlags::empty()).map_err(|_| Failure::Unavailable)?;
        self.process.still_live().map_err(|_| Failure::Dead)?;
        if self.image
            != (ProcessKey {
                device: stat.st_dev,
                inode: stat.st_ino,
            })
        {
            return Err(Failure::Changed);
        }
        Ok(())
    }
}

struct Reader<'a> {
    process: &'a ConnectedProcess,
    start: Instant,
}
impl Reader<'_> {
    fn check(&self) -> Result<(), Failure> {
        self.process.still_live().map_err(|_| Failure::Dead)?;
        if self.start.elapsed() > BUDGET {
            return Err(Failure::Deadline);
        }
        Ok(())
    }
    fn io<T>(&self, f: impl FnOnce() -> io::Result<T>) -> Result<T, Failure> {
        self.check()?;
        let value = f();
        self.check()?;
        value.map_err(|_| Failure::Unavailable)
    }
    fn open(&self, path: &Path) -> Result<File, Failure> {
        self.io(|| File::open(path))
    }
    fn at(&self, dir: &File, name: &str) -> Result<File, Failure> {
        self.io(|| {
            openat(
                dir,
                name,
                OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(io::Error::from)
        })
    }
    fn text(&self, mut file: File) -> Result<String, Failure> {
        self.text_limited(&mut file, FILE_LIMIT)
    }
    fn text_limited(&self, file: &mut File, limit: usize) -> Result<String, Failure> {
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0_u8; 1024];
            let n = self.io(|| file.read(&mut buffer))?;
            if n == 0 {
                break;
            }
            if bytes.len().checked_add(n).is_none_or(|n| n > limit) {
                return Err(Failure::Capacity);
            }
            bytes.extend_from_slice(&buffer[..n]);
        }
        String::from_utf8(bytes).map_err(|_| Failure::Malformed)
    }
    fn key(&self, file: &File) -> Result<ProcessKey, Failure> {
        self.io(|| key(file.as_fd()))
    }
    fn image(&self, dir: &File) -> Result<ProcessKey, Failure> {
        // fstatat follows /proc/PID/exe only for metadata; executable bytes are
        // neither opened nor read. The process directory and pidfd stay held.
        let stat = self.io(|| fstatat(dir, "exe", AtFlags::empty()).map_err(io::Error::from))?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(Failure::Malformed);
        }
        Ok(ProcessKey {
            device: stat.st_dev,
            inode: stat.st_ino,
        })
    }
    fn subtree_members(
        &self,
        dir: &File,
        pid: i32,
        depth: usize,
        nodes: &mut usize,
        entries: &mut usize,
    ) -> Result<(usize, bool), Failure> {
        *nodes += 1;
        if depth > 32 || *nodes > 128 {
            return Err(Failure::Capacity);
        }
        let (mut count, mut same) = members(&self.text(self.at(dir, "cgroup.procs")?)?, pid)?;
        let path = format!("/proc/self/fd/{}", dir.as_raw_fd());
        let mut children = self.io(|| std::fs::read_dir(path))?;
        loop {
            let child = self.io(|| children.next().transpose())?;
            let Some(child) = child else {
                break;
            };
            *entries += 1;
            if *entries > 8192 {
                return Err(Failure::Capacity);
            }
            let name = child.file_name();
            let stat = self.io(|| {
                fstatat(dir, name.as_os_str(), AtFlags::AT_SYMLINK_NOFOLLOW)
                    .map_err(io::Error::from)
            })?;
            match stat.st_mode & libc::S_IFMT {
                libc::S_IFREG => (),
                libc::S_IFDIR => {
                    let next = self.io(|| {
                        openat(
                            dir,
                            name.as_os_str(),
                            OFlag::O_RDONLY
                                | OFlag::O_DIRECTORY
                                | OFlag::O_CLOEXEC
                                | OFlag::O_NOFOLLOW,
                            Mode::empty(),
                        )
                        .map(File::from)
                        .map_err(io::Error::from)
                    })?;
                    let (child_count, child_same) =
                        self.subtree_members(&next, pid, depth + 1, nodes, entries)?;
                    count = count.checked_add(child_count).ok_or(Failure::Capacity)?;
                    // The target must be in this observed cgroup's root file.
                    // A target present only below it contradicts /proc/PID/cgroup.
                    same &= child_same && child_count == 0;
                }
                _ => return Err(Failure::Malformed),
            }
        }
        Ok((count, same))
    }
}

fn pid_from_fdinfo(text: &str) -> Result<i32, Failure> {
    let mut pid = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("Pid:") {
            if pid.is_some() {
                return Err(Failure::Malformed);
            }
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Failure::Malformed);
            }
            pid = Some(value.parse::<i32>().map_err(|_| Failure::Malformed)?);
        }
    }
    match pid {
        Some(0) => Err(Failure::PidNamespace),
        Some(pid) if pid > 0 => Ok(pid),
        _ => Err(Failure::Malformed),
    }
}

/// Require exactly one unshifted cgroup2 mount at the path this collector
/// actually opens. A filesystem magic check alone cannot establish its root.
fn cgroup_mount_aligned(text: &str, root: ProcessKey) -> bool {
    let mut found = false;
    for line in text.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            return false;
        };
        let fields: Vec<_> = left.split_ascii_whitespace().collect();
        let tail: Vec<_> = right.split_ascii_whitespace().collect();
        if fields.len() < 6 || tail.len() != 3 {
            return false;
        }
        if fields[4] != "/sys/fs/cgroup" {
            continue;
        }
        if found || fields[3] != "/" || tail[0] != "cgroup2" {
            return false;
        }
        let expected = format!("{}:{}", libc::major(root.device), libc::minor(root.device));
        if fields[2] != expected {
            return false;
        }
        found = true;
    }
    found
}

fn cgroup_from_text(text: &str) -> Result<&str, Failure> {
    let mut group = None;
    for line in text.lines() {
        let path = line.strip_prefix("0::").ok_or(Failure::Malformed)?;
        if group.is_some()
            || path.len() > PATH_LIMIT
            || !path.starts_with('/')
            || path.contains('\0')
        {
            return Err(Failure::Malformed);
        }
        if path != "/"
            && path
                .split('/')
                .skip(1)
                .any(|s| s.is_empty() || s == "." || s == "..")
        {
            return Err(Failure::Malformed);
        }
        group = Some(path);
    }
    group.ok_or(Failure::Malformed)
}

fn members(text: &str, pid: i32) -> Result<(usize, bool), Failure> {
    let mut count = 0_usize;
    let mut same = true;
    for line in text.lines() {
        if line.is_empty() || !line.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Failure::Malformed);
        }
        let task = line.parse::<i32>().map_err(|_| Failure::Malformed)?;
        if task <= 0 {
            return Err(Failure::Malformed);
        }
        count += 1;
        same &= task == pid;
    }
    Ok((count, same))
}

#[cfg(test)]
fn singleton(text: &str, pid: i32) -> Result<bool, Failure> {
    members(text, pid).map(|(count, same)| count == 1 && same)
}

fn proc_namespace_aligned(text: &str, own_pid: u32) -> bool {
    let mut entries = text.lines().filter_map(|line| line.strip_prefix("NSpid:"));
    let Some(value) = entries.next() else {
        return false;
    };
    if entries.next().is_some() {
        return false;
    }
    let mut numbers = value.split_ascii_whitespace();
    numbers.next().and_then(|n| n.parse::<u32>().ok()) == Some(own_pid) && numbers.next().is_none()
}

fn parent_from_status(text: &str) -> Result<i32, Failure> {
    let mut values = text.lines().filter_map(|line| line.strip_prefix("PPid:"));
    let value = values.next().ok_or(Failure::Malformed)?.trim();
    if values.next().is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Failure::Malformed);
    }
    value.parse::<i32>().map_err(|_| Failure::Malformed)
}

fn parents(
    reader: &Reader<'_>,
    root: &File,
    child: &File,
    roles: &RoleTable,
) -> Result<Vec<Ancestor>, Failure> {
    let mut result: Vec<Ancestor> = Vec::new();
    for _ in 0..3 {
        let directory = result.last().map_or(child, |parent| &parent.directory);
        let status = reader.text(reader.at(directory, "status")?)?;
        let pid = parent_from_status(&status)?;
        if pid == 0 {
            break;
        }
        // PPid is read from our retained process directory, never callback
        // text. Re-read it after pinning, and retain both incarnations.
        let descriptor = reader.io(|| {
            // SAFETY: pidfd_open takes this checked positive process ID and
            // flags 0; on success the new descriptor is uniquely owned here.
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            use std::os::fd::FromRawFd;
            Ok(unsafe { OwnedFd::from_raw_fd(raw as i32) })
        })?;
        let parent = RetainedProcess {
            key: reader.io(|| process_key(descriptor.as_fd()))?,
            process: descriptor,
        };
        parent.still_live().map_err(|_| Failure::Dead)?;
        if parent_from_status(&reader.text(reader.at(directory, "status")?)?)? != pid {
            return Err(Failure::Changed);
        }
        if result.iter().any(|old| old.process.key == parent.key)
            || parent.key == reader.process.key
        {
            return Err(Failure::Malformed);
        }
        for old in &result {
            old.recheck()?;
        }
        let directory = reader.at(root, &pid.to_string())?;
        parent.still_live().map_err(|_| Failure::Dead)?;
        let image = reader.image(&directory)?;
        parent.still_live().map_err(|_| Failure::Dead)?;
        let _group = cgroup_from_text(&reader.text(reader.at(&directory, "cgroup")?)?)?.to_owned();
        parent.still_live().map_err(|_| Failure::Dead)?;
        let image_kind = roles.classify(image);
        result.push(Ancestor {
            process: parent,
            image,
            image_kind,
            directory,
            unified_cgroup: _group,
        });
        reader.check()?;
        if image_kind == Image::Runtime {
            break;
        }
    }
    for parent in &result {
        parent.recheck()?;
    }
    Ok(result)
}

impl ConnectedProcess {
    /// A negative classification is permitted only after a successful read
    /// through the original process handle. Observation failure is never a
    /// reason to reinterpret a runtime echo as an intentional host write.
    pub(crate) fn in_runtime_unit(&self) -> Result<bool, Failure> {
        let reader = Reader {
            process: self,
            start: Instant::now(),
        };
        let root = reader.open(Path::new("/proc"))?;
        if !proc_namespace_aligned(
            &reader.text(reader.at(&root, "self/status")?)?,
            std::process::id(),
        ) {
            return Err(Failure::PidNamespace);
        }
        let name = format!("self/fdinfo/{}", self.process.as_raw_fd());
        let pid = pid_from_fdinfo(&reader.text(reader.at(&root, &name)?)?)?;
        if self.credentials.pid != Some(pid) {
            return Err(Failure::PidNamespace);
        }
        let directory = reader.at(&root, &pid.to_string())?;
        let text = reader.text(reader.at(&directory, "cgroup")?)?;
        let group = cgroup_from_text(&text)?;
        reader.check()?;
        Ok(group.ends_with("/tessera-controller@main.service"))
    }

    /// Reads proc/cgroup metadata only. Liveness and a monotonic budget are
    /// checked around each syscall, including failed reads. Filesystem stalls
    /// are not independently supervised by this diagnostic collector.
    #[cfg(test)]
    pub(crate) fn launch_snapshot(&self) -> Result<Snapshot, Failure> {
        self.collect_role(&RoleTable::empty(), true)
            .map(|observation| observation.snapshot)
    }

    pub(crate) fn role_snapshot(&self, roles: &RoleTable) -> Result<Observation, Failure> {
        self.collect_role(roles, false)
    }

    fn collect_role(
        &self,
        roles: &RoleTable,
        collect_unknown: bool,
    ) -> Result<Observation, Failure> {
        let reader = Reader {
            process: self,
            start: Instant::now(),
        };
        let proc_root = reader.open(Path::new("/proc"))?;
        let magic = reader.io(|| nix::sys::statfs::fstatfs(&proc_root).map_err(io::Error::from))?;
        if magic.filesystem_type().0 != 0x9fa0 {
            return Err(Failure::ProcMount);
        }
        // NSpid is relative to this proc mount. A single value equal to our
        // actual PID refuses a proc mount from an ancestor PID namespace.
        let own_status = reader.text(reader.at(&proc_root, "self/status")?)?;
        if !proc_namespace_aligned(&own_status, std::process::id()) {
            return Err(Failure::PidNamespace);
        }
        // fdinfo's Pid is the retained pidfd's task in the reader's namespace,
        // not a PID copied out of callback text or used to reopen a pidfd.
        let fdinfo_name = format!("self/fdinfo/{}", self.process.as_raw_fd());
        let pid = pid_from_fdinfo(&reader.text(reader.at(&proc_root, &fdinfo_name)?)?)?;
        if self.credentials.pid != Some(pid) {
            return Err(Failure::PidNamespace);
        }
        let own_namespace = reader.open(Path::new("/proc/self/ns/pid"))?;
        let namespace_key = reader.key(&own_namespace)?;
        let process_dir = reader.io(|| {
            openat(
                &proc_root,
                pid.to_string().as_str(),
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(io::Error::from)
        })?;
        let process_namespace = reader.io(|| {
            openat(
                &process_dir,
                "ns/pid",
                OFlag::O_RDONLY | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(io::Error::from)
        })?;
        if reader.key(&process_namespace)? != namespace_key {
            return Err(Failure::PidNamespace);
        }
        let image = reader.image(&process_dir)?;
        let image_kind = roles.classify(image);
        if image_kind == Image::Other && !collect_unknown {
            // Ordinary portal/CLI Status polling does not walk a cgroup subtree.
            return Err(Failure::ImageUnrecognized);
        }
        let group_text = reader.text(reader.at(&process_dir, "cgroup")?)?;
        let group = cgroup_from_text(&group_text)?;
        let mut group_dir = reader.open(Path::new("/sys/fs/cgroup"))?;
        let magic = reader.io(|| nix::sys::statfs::fstatfs(&group_dir).map_err(io::Error::from))?;
        if magic.filesystem_type().0 != 0x6367_7270 {
            return Err(Failure::Unavailable);
        }
        let mountinfo =
            reader.text_limited(&mut reader.at(&proc_root, "self/mountinfo")?, 128 * 1024)?;
        if !cgroup_mount_aligned(&mountinfo, reader.key(&group_dir)?) {
            return Err(Failure::CgroupMount);
        }
        for part in Path::new(group).components() {
            match part {
                Component::RootDir => (),
                Component::Normal(name) => {
                    group_dir = reader.io(|| {
                        openat(
                            &group_dir,
                            name,
                            OFlag::O_RDONLY
                                | OFlag::O_DIRECTORY
                                | OFlag::O_CLOEXEC
                                | OFlag::O_NOFOLLOW,
                            Mode::empty(),
                        )
                        .map(File::from)
                        .map_err(io::Error::from)
                    })?;
                }
                _ => return Err(Failure::Malformed),
            }
        }
        #[cfg(test)]
        roles
            .walks
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (members, same) = reader.subtree_members(&group_dir, pid, 0, &mut 0, &mut 0)?;
        let cgroup = reader.key(&group_dir)?;
        let _parents = if matches!(image_kind, Image::ReportAdapter | Image::ForwardedCli) {
            parents(&reader, &proc_root, &process_dir, roles)?
        } else {
            Vec::new()
        };
        // Reject observed change, without claiming an atomic snapshot or
        // excluding an exec/cgroup ABA between these finite reads.
        if reader.image(&process_dir)? != image
            || reader.text(reader.at(&process_dir, "cgroup")?)? != group_text
            || pid_from_fdinfo(&reader.text(reader.at(&proc_root, &fdinfo_name)?)?)? != pid
        {
            return Err(Failure::Changed);
        }
        reader.check()?;
        Ok(Observation {
            snapshot: Snapshot {
                process: self.key,
                pid_namespace: namespace_key,
                image,
                image_kind,
                unified_cgroup: group.to_owned(),
                cgroup,
                observed_subtree_singleton: (members == 1 && same)
                    || cfg!(test) && {
                        #[cfg(test)]
                        {
                            roles.fixture_singleton
                        }
                        #[cfg(not(test))]
                        {
                            false
                        }
                    },
                #[cfg(test)]
                fixture_membership: roles.fixture_singleton,
            },
            _proc_root: proc_root,
            process_dir,
            _cgroup_dir: group_dir,
            parents: _parents,
        })
    }

    /// Minimal recheck after actor queue delay: retained pidfds and executable
    /// metadata only. The earlier cgroup/parent reads remain finite observations.
    pub(crate) fn recheck_role(&self, observation: &Observation) -> Result<(), Failure> {
        let reader = Reader {
            process: self,
            start: Instant::now(),
        };
        reader.check()?;
        if observation.snapshot.process != self.key {
            return Err(Failure::Changed);
        }
        if reader.image(&observation.process_dir)? != observation.snapshot.image {
            return Err(Failure::Changed);
        }
        if cgroup_from_text(&reader.text(reader.at(&observation.process_dir, "cgroup")?)?)?
            != observation.snapshot.unified_cgroup
            || reader.key(&observation._cgroup_dir)? != observation.snapshot.cgroup
        {
            return Err(Failure::Changed);
        }
        for parent in &observation.parents {
            parent.recheck()?;
        }
        reader.check()
    }

    /// Recheck absence of surviving members below the retained unit cgroup.
    /// The manager-verified launcher must still be its only process before exec.
    pub(crate) fn recheck_launcher_singleton(
        &self,
        observation: &Observation,
    ) -> Result<(), Failure> {
        self.recheck_role(observation)?;
        let reader = Reader {
            process: self,
            start: Instant::now(),
        };
        let pid = self.pid().ok_or(Failure::Unavailable)?;
        let (count, same) =
            reader.subtree_members(&observation._cgroup_dir, pid, 0, &mut 0, &mut 0)?;
        if count != 1 || !same {
            return Err(Failure::Changed);
        }
        reader.check()
    }

    /// Registration retains this launcher incarnation across its exec into
    /// the runtime. Both images must be root-owned installed roles, and the
    /// process must remain in the registered cgroup. This is not producer order.
    pub(crate) fn recheck_runtime_holder(
        &self,
        observation: &Observation,
        roles: &RoleTable,
    ) -> Result<(), Failure> {
        let reader = Reader {
            process: self,
            start: Instant::now(),
        };
        reader.check()?;
        if observation.snapshot.process != self.key
            || !matches!(
                roles.classify(reader.image(&observation.process_dir)?),
                Image::Launcher | Image::Runtime
            )
            || cgroup_from_text(&reader.text(reader.at(&observation.process_dir, "cgroup")?)?)?
                != observation.snapshot.unified_cgroup
            || reader.key(&observation._cgroup_dir)? != observation.snapshot.cgroup
        {
            return Err(Failure::Changed);
        }
        reader.check()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn unknown_role_does_not_walk_a_cgroup_and_parent_fields_are_exact() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        let process = ConnectedProcess::capture(socket.as_fd()).unwrap();
        let table = RoleTable::empty();
        assert_eq!(
            process.role_snapshot(&table).unwrap_err(),
            Failure::ImageUnrecognized
        );
        assert_eq!(table.walks.load(Ordering::SeqCst), 0);
        assert_eq!(parent_from_status("Name: fixture\nPPid:\t12\n"), Ok(12));
        assert_eq!(parent_from_status("PPid: 0"), Ok(0));
        for text in [
            "",
            "PPid: -1",
            "PPid: 1 2",
            "PPid: +1",
            "PPid: 1\nPPid: 1",
            "PPid: 2147483648",
        ] {
            assert_eq!(parent_from_status(text), Err(Failure::Malformed));
        }
    }

    pub(super) struct Scratch(pub(super) std::path::PathBuf);
    impl Scratch {
        pub(super) fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "tessera-process-observation-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn descendants_are_counted_and_symlinks_do_not_escape_the_root() {
        let scratch = Scratch::new();
        let (socket, _peer) = UnixStream::pair().unwrap();
        let process = ConnectedProcess::capture(socket.as_fd()).unwrap();
        let reader = Reader {
            process: &process,
            start: Instant::now(),
        };
        let pid = i32::try_from(std::process::id()).unwrap();
        std::fs::write(scratch.0.join("cgroup.procs"), format!("{pid}\n")).unwrap();
        std::fs::create_dir_all(scratch.0.join("child/grandchild")).unwrap();
        std::fs::write(scratch.0.join("child/cgroup.procs"), "").unwrap();
        std::fs::write(
            scratch.0.join("child/grandchild/cgroup.procs"),
            format!("{}\n", pid + 1),
        )
        .unwrap();
        let root = File::open(&scratch.0).unwrap();
        assert_eq!(
            reader.subtree_members(&root, pid, 0, &mut 0, &mut 0),
            Ok((2, false))
        );
        std::fs::write(scratch.0.join("child/grandchild/cgroup.procs"), "").unwrap();
        assert_eq!(
            reader.subtree_members(&root, pid, 0, &mut 0, &mut 0),
            Ok((1, true))
        );
        std::os::unix::fs::symlink("/", scratch.0.join("escape")).unwrap();
        assert_eq!(
            reader.subtree_members(&root, pid, 0, &mut 0, &mut 0),
            Err(Failure::Malformed)
        );
    }

    #[test]
    fn observation_text_time_and_subtree_budgets_refuse() {
        let scratch = Scratch::new();
        let (socket, _peer) = UnixStream::pair().unwrap();
        let process = ConnectedProcess::capture(socket.as_fd()).unwrap();
        let reader = Reader {
            process: &process,
            start: Instant::now(),
        };
        let path = scratch.0.join("large");
        std::fs::write(&path, vec![b'x'; FILE_LIMIT + 1]).unwrap();
        assert_eq!(
            reader.text(File::open(&path).unwrap()),
            Err(Failure::Capacity)
        );
        let expired = Reader {
            process: &process,
            start: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
        };
        assert_eq!(expired.open(&path).unwrap_err(), Failure::Deadline);
        let root = File::open(&scratch.0).unwrap();
        assert_eq!(
            reader.subtree_members(&root, 1, 33, &mut 0, &mut 0),
            Err(Failure::Capacity)
        );
        assert_eq!(
            reader.subtree_members(&root, 1, 0, &mut 128, &mut 0),
            Err(Failure::Capacity)
        );
    }

    #[test]
    fn pidfd_fields_are_exact_and_no_numeric_fallback_exists() {
        assert_eq!(
            pid_from_fdinfo("pos:\t0\nPid:\t123\nNSpid:\t123\n"),
            Ok(123)
        );
        for bad in [
            "",
            "Pid: -1",
            "Pid: 1\nPid: 1",
            "Pid: 1 2",
            "Pid: 2147483648",
            "Pid: +1",
        ] {
            assert_eq!(pid_from_fdinfo(bad), Err(Failure::Malformed));
        }
        assert_eq!(pid_from_fdinfo("Pid: 0"), Err(Failure::PidNamespace));
    }

    #[test]
    fn only_one_unified_cgroup_and_safe_absolute_components_are_supported() {
        assert_eq!(
            cgroup_from_text("0::/system.slice/example.service\n"),
            Ok("/system.slice/example.service")
        );
        assert_eq!(cgroup_from_text("0::/\n"), Ok("/"));
        for bad in [
            "",
            "1:cpu:/group",
            "0::relative",
            "0::/a/../b",
            "0::/a/./b",
            "0::/a//b",
            "0::/a/",
            "0::/a\n0::/a",
            "0::/a\0b",
        ] {
            assert_eq!(cgroup_from_text(bad), Err(Failure::Malformed));
        }
    }

    #[test]
    fn singleton_is_exact_and_duplicates_or_empty_never_prove_absence() {
        assert_eq!(singleton("123\n", 123), Ok(true));
        for non_singleton in ["", "124\n", "123\n123\n", "123\n124\n"] {
            assert_eq!(singleton(non_singleton, 123), Ok(false));
        }
        for bad in ["0\n", "-1\n", "1 2\n", "123\n\n", "2147483648\n"] {
            assert_eq!(singleton(bad, 123), Err(Failure::Malformed));
        }
    }

    #[test]
    fn a_proc_mount_from_an_ancestor_namespace_is_unavailable() {
        assert!(proc_namespace_aligned("Name:\ttest\nNSpid:\t123\n", 123));
        for bad in [
            "",
            "NSpid:\t124",
            "NSpid:\t1000\t123",
            "NSpid:\t123\nNSpid:\t123",
            "NSpid:\t0",
        ] {
            assert!(!proc_namespace_aligned(bad, 123));
        }
    }

    #[test]
    fn cgroup_mount_root_device_and_uniqueness_must_correspond() {
        let root = ProcessKey {
            device: libc::makedev(0, 29),
            inode: 1,
        };
        let aligned = "31 22 0:29 / /sys/fs/cgroup ro,nosuid - cgroup2 cgroup rw\n";
        assert!(cgroup_mount_aligned(aligned, root));
        for bad in [
            "".to_string(),
            aligned.replace(" / /sys", " /shifted /sys"),
            aligned.replace("0:29", "0:30"),
            aligned.replace("cgroup2", "tmpfs"),
            aligned.repeat(2),
            "malformed".to_string(),
        ] {
            assert!(!cgroup_mount_aligned(&bad, root), "{bad}");
        }
    }
}
