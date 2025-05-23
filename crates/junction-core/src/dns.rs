//! DNS and DNS accessories.
//!
//! # Resolvers
//!
//! This module include the DNS resolvers that the Junction client uses to look
//! up addresses for LOGICAL_DNS and STRICT_DNS xDS clusters. Resolution is done
//! with an in-client resolver so that behavior is consistent between systems.
//!
//! See resolver documentation for details.
//!
//! # System Configuration
//!
//! This module also exposes functions for reading system resolver
//! configuration, which is used to make Junction name resolution behavior match
//! system resolver behavior where appropriate.

use std::{
    collections::{btree_map, BTreeMap, BTreeSet},
    io,
    net::SocketAddr,
    path::Path,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use junction_api::Hostname;
use rand::Rng;
use tokio::sync::Notify;

use crate::endpoints::EndpointGroup;

/// An error that occurred while parsing a system DNS configuration.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    #[error("config not found: {path}")]
    NotFound { path: String },

    #[error("{path}:{line}: {message}")]
    Invalid {
        path: String,
        line: usize,
        message: String,
    },

    #[error(transparent)]
    Other(#[from] std::io::Error),
}

/// An extremely simple subset of a system DNS resolver configuration.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SystemConfig {
    pub(crate) ndots: u8,
    pub(crate) search: Vec<Hostname>,
}

/// Load a [SystemConfig] from the given path. You probaly want to read
/// `/etc/resolv.conf` but the option is here just in case you don't.
pub(crate) fn load_config<P: AsRef<Path>>(path: P) -> Result<SystemConfig, ConfigError> {
    let content = match std::fs::read(path.as_ref()) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ConfigError::NotFound {
                path: path.as_ref().display().to_string(),
            })
        }
        Err(e) => return Err(e.into()),
    };

    parse_resolv_conf(path, &content)
}

/// An extremely simple resolv.conf parser.
///
/// This is included here so we don't have to take another dependency - the
/// complexity of parsing two options out of a text file felt quite low, and as
/// of writing, the crate(s) available for it didn't appear to be doing anything
/// more sophisticated than this or have extensive fuzz testing regimes, etc.
fn parse_resolv_conf(path: impl AsRef<Path>, content: &[u8]) -> Result<SystemConfig, ConfigError> {
    // split on newline
    let lines = content.split(|&b| b == b'\n');

    let mut ndots = 1u8;
    let mut search = vec![];

    for (i, line) in lines.enumerate() {
        let line = line.trim_ascii();

        match line.first() {
            // skip comments
            Some(b';') | Some(b'#') => continue,
            // skip empty lines
            None => continue,
            _ => (),
        }

        let parts: Vec<_> = line.split(|b| b.is_ascii_whitespace()).collect();
        match parts.as_slice() {
            [b"options", options @ ..] => {
                for option in options {
                    if let Some(n) = option.strip_prefix(b"ndots:") {
                        ndots = parse_as_str(n).map_err(|()| ConfigError::Invalid {
                            path: path.as_ref().display().to_string(),
                            line: i,
                            message: format!("invalid ndots: '{}'", String::from_utf8_lossy(n)),
                        })?;
                    }
                }
            }
            [b"search", hostnames @ ..] => {
                let hostnames: Result<Vec<_>, _> =
                    hostnames.iter().map(|bs| Hostname::try_from(*bs)).collect();

                match hostnames {
                    Ok(hostnames) => search = hostnames,
                    Err(e) => {
                        return Err(ConfigError::Invalid {
                            path: path.as_ref().display().to_string(),
                            line: i,
                            message: format!("search path contains invalid hostname: {e}"),
                        })
                    }
                }
            }
            // ignore any other directives, even if they're badly formed. we
            // don't care about em
            _ => (),
        }
    }

    Ok(SystemConfig { ndots, search })
}

fn parse_as_str<T: std::str::FromStr>(bs: &[u8]) -> Result<T, ()> {
    let as_str = std::str::from_utf8(bs).map_err(|_| ())?;
    as_str.parse().map_err(|_| ())
}

/// A blocking resolver that uses the stdlib to resolve hostnames to addresses.
///
/// Names are resolved regularly in the background. If the addresses behind a
/// name change, or a resolution error occurs, the results are broadcast over
/// a channel (see the `subscribe` method) to all subscribers.
///
/// Behind the scenes, this resolver uses a fixed pool of threads to
/// periodically resolve all of the addresses for a name. On every resolution,
/// the returned set of IP addresses is treated as the entire set of addresses
/// that make up a name and overwrites any previous addreses.  This roughly
/// corresponds to Envoy's [STRICT_DNS] approach to resolution.
///
/// A StdlibResolver spawns its own pool of worker threads in the background
/// that exit when the resolver is dropped.
///
/// [STRICT_DNS]: https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/upstream/service_discovery#strict-dns
#[derive(Clone, Debug)]
pub(crate) struct StdlibResolver {
    inner: Arc<StdlibResolverInner>,
}

#[derive(Debug)]
struct StdlibResolverInner {
    lookup_interval: Duration,
    lookup_jitter: Duration,

    // a mutex/condvar pair wrapped around resolver state. see ResolverState for
    // exactly what that does. nothing here should be much more than locking
    // around accessing that struct.
    cond: Condvar,
    tasks: Mutex<ResolverState>,

    // a notify to let async callers know names have changed while they're
    // waiting. not specific to any name - will definitely get busy and have
    // spurious wakeups at hgih volumes of names.
    async_notify: Notify,

    // the the number of worker threads the resolver was started with. if the
    // number of references to this struct ever drops, it's time for the
    // resolver threads to shut down.
    worker_count: usize,
}

macro_rules! no_poison {
    ($guard:expr) => {
        $guard.expect("SystemResolver was poisoned: this is a bug in Junction")
    };
}

impl Drop for StdlibResolver {
    fn drop(&mut self) {
        // self.shutdown();
    }
}

impl StdlibResolver {
    pub(crate) fn new_with(
        lookup_interval: Duration,
        lookup_jitter: Duration,
        threads: usize,
    ) -> Self {
        let inner = StdlibResolverInner {
            lookup_interval,
            lookup_jitter,
            tasks: Mutex::new(ResolverState::default()),
            cond: Condvar::new(),
            async_notify: Notify::new(),
            worker_count: threads,
        };
        let resolver = StdlibResolver {
            inner: Arc::new(inner),
        };

        for _ in 0..threads {
            let resolver = resolver.clone();
            std::thread::spawn(move || resolver.run());
        }

        resolver
    }

    pub(crate) fn get_endpoints(
        &self,
        hostname: &Hostname,
        port: u16,
    ) -> Option<Arc<EndpointGroup>> {
        let tasks = no_poison!(self.inner.tasks.lock());
        tasks.get_endpoints(hostname, port)
    }

    pub(crate) async fn get_endpoints_await(
        &self,
        hostname: &Hostname,
        port: u16,
    ) -> Option<Arc<EndpointGroup>> {
        // fast path: the endpoints are in the map.
        if let Some(endpoints) = self.get_endpoints(hostname, port) {
            return Some(endpoints);
        }

        // slow path: we're waiting
        //
        // on every run through the loop, we need to, IN ORDER, do the
        // following:
        //
        // - register for notifications
        // - try to get a name in the map, returning it if present
        // - wait for the next notification
        //
        // given this order of events, and the guarantees on Notify, there is no
        // interleaving of events where we can miss a notification.
        //
        // SAFETY: this assumes that the writer half of a notify is always using
        // notify_waiters and not ever using notify_one.
        //
        // for an example that uses notify_one() instead, see Notify
        //
        // https://docs.rs/tokio/latest/tokio/sync/futures/struct.Notified.html#method.enable
        let changed = self.inner.async_notify.notified();
        tokio::pin!(changed);
        loop {
            // check the map
            if let Some(entry) = self.get_endpoints(hostname, port) {
                return Some(entry);
            }

            // wait for a change
            changed.as_mut().await;

            // this uses Pin::set so we're not allocating/deallocating a new
            // wakeup future every time.
            changed.set(self.inner.async_notify.notified());
        }
    }

    pub(crate) fn subscribe(&self, name: Hostname, port: u16) {
        let mut tasks = no_poison!(self.inner.tasks.lock());

        // on a new subscribtion, notify both the background workers and any
        // async tasks waiting on a get.
        if tasks.pin(name, port) {
            self.inner.cond.notify_all();
            self.inner.async_notify.notify_waiters();
        }
    }

    pub(crate) fn unsubscribe(&self, name: &Hostname, port: u16) {
        let mut tasks = no_poison!(self.inner.tasks.lock());
        tasks.remove(name, port);
        self.inner.cond.notify_all();
    }

    pub(crate) fn set_names(&self, new_names: impl IntoIterator<Item = (Hostname, u16)>) {
        let new_names = new_names.into_iter();

        let mut tasks = no_poison!(self.inner.tasks.lock());
        if tasks.update_all(new_names) {
            self.inner.cond.notify_all();
        }
    }

    pub(crate) fn run(&self) {
        let thread_id = std::thread::current().id();
        macro_rules! worker_trace {
            ($($item:tt)*) => {
                tracing::trace!(
                    ?thread_id,
                    worker_count = self.inner.worker_count,
                    strong_count = Arc::strong_count(&self.inner),
                    $(
                        $item
                    )*
                )
            };
        }

        worker_trace!("starting");
        loop {
            // grab the next name
            let Some(name) = self.next_name() else {
                worker_trace!("exiting");
                return;
            };

            // do the DNS lookup
            //
            // this always uses 80 and then immediately discards the port. we
            // don't actually care about what the port is here.
            worker_trace!(%name, "starting lookup");
            let addr = (&name[..], 80);
            let answer = std::net::ToSocketAddrs::to_socket_addrs(&addr).map(|answer| {
                // TODO: we're filtering out every v6 address here. this isn't
                // corect long term - we need to define how we want to control
                // v4/v6 at the api level.
                answer.filter(|a| a.is_ipv4()).collect()
            });

            // save the answer
            worker_trace!(%name, "resolved");
            self.insert_answer(name, Instant::now(), answer);
        }
    }

    fn is_shutdown(&self) -> bool {
        Arc::strong_count(&self.inner) <= self.inner.worker_count
    }

    fn next_name(&self) -> Option<Hostname> {
        let mut tasks = no_poison!(self.inner.tasks.lock());

        loop {
            if self.is_shutdown() {
                return None;
            }

            // claim a name older than the cutoff
            let before = Instant::now() - self.inner.lookup_interval;
            if let Some(name) = tasks.next_name(before) {
                return Some(name.clone());
            }

            // if there's nothing to do, sleep until there is. add a little
            // bit of jitter here to spread out the load this puts on upstream
            // dns servers.
            //
            // if there's no task, just sleep until notified
            let wait_time = tasks.min_resolved_at().map(|t| {
                let d = t.saturating_duration_since(Instant::now());
                d + self.inner.lookup_interval + rng_jitter(self.inner.lookup_jitter)
            });

            tracing::trace!(?wait_time, "waiting for new name");
            match wait_time {
                Some(wait_time) => {
                    (tasks, _) = no_poison!(self.inner.cond.wait_timeout(tasks, wait_time));
                }
                None => tasks = no_poison!(self.inner.cond.wait(tasks)),
            }
        }
    }

    fn insert_answer(
        &self,
        name: Hostname,
        resolved_at: Instant,
        answer: io::Result<Vec<SocketAddr>>,
    ) {
        // grab the lock in a tight scope
        {
            let mut tasks = no_poison!(self.inner.tasks.lock());
            tasks.insert_answer(&name, resolved_at, answer);
        }

        // after the lock is free:
        //
        // notify all async callers in get_await. since there's no
        // blocking get_* method, there are no sync callers to notify.
        self.inner.async_notify.notify_waiters();
    }
}

fn rng_jitter(max: Duration) -> Duration {
    let secs = crate::rand::with_thread_rng(|rng| rng.gen_range(0.0..max.as_secs_f64()));

    Duration::from_secs_f64(secs)
}

#[derive(Debug, Default)]
struct ResolverState(BTreeMap<Hostname, NameInfo>);

#[derive(Debug, Default)]
struct NameInfo {
    ports: BTreeMap<u16, PortInfo>,
    in_flight: bool,
    resolved_at: Option<Instant>,
    last_addrs: Option<Vec<SocketAddr>>,
    last_error: Option<io::Error>,
}

#[derive(Debug, Default)]
struct PortInfo {
    pinned: bool,
    endpoint_group: Option<Arc<EndpointGroup>>,
}

impl PortInfo {
    fn set_addrs(&mut self, port: u16, addrs: &[SocketAddr]) {
        let addrs = addrs.iter().cloned().map(|mut addr| {
            addr.set_port(port);
            addr
        });
        self.endpoint_group = Some(Arc::new(EndpointGroup::from_dns_addrs(addrs)))
    }
}

impl NameInfo {
    fn merge_answer(&mut self, now: Instant, answer: io::Result<Vec<SocketAddr>>) {
        // always update time
        self.resolved_at = Some(now);

        // update eitehr the endpoints or error based on the answer
        match answer {
            Ok(mut addrs) => {
                self.last_error = None;

                // normalize addrs
                addrs.sort();

                // if the addrs have changed, update both the raw addrs and the
                // EndpointGroup for each port.
                if Some(&addrs) != self.last_addrs.as_ref() {
                    for (port, port_info) in self.ports.iter_mut() {
                        port_info.set_addrs(*port, &addrs);
                    }
                    self.last_addrs = Some(addrs);
                }
            }
            Err(e) => self.last_error = Some(e),
        }
    }

    fn resolved_before(&self, t: Instant) -> bool {
        match self.resolved_at {
            Some(resolved_at) => resolved_at < t,
            None => true,
        }
    }
}

impl ResolverState {
    fn next_name(&mut self, before: Instant) -> Option<&Hostname> {
        let mut min: Option<(_, &mut NameInfo)> = None;

        for (name, state) in &mut self.0 {
            if state.in_flight {
                continue;
            }

            match state.resolved_at {
                Some(t) => {
                    if t <= before && min.as_ref().map_or(true, |(_, s)| s.resolved_before(t)) {
                        min = Some((name, state))
                    }
                }
                None => {
                    state.in_flight = true;
                    return Some(name);
                }
            }
        }

        min.map(|(name, state)| {
            state.in_flight = true;
            name
        })
    }

    fn min_resolved_at(&self) -> Option<Instant> {
        self.0.values().filter_map(|state| state.resolved_at).min()
    }

    fn get_endpoints(&self, hostname: &Hostname, port: u16) -> Option<Arc<EndpointGroup>> {
        let name_info = self.0.get(hostname)?;
        let port_info = name_info.ports.get(&port)?;
        port_info.endpoint_group.clone()
    }

    fn insert_answer(
        &mut self,
        hostname: &Hostname,
        resolved_at: Instant,
        answer: io::Result<Vec<SocketAddr>>,
    ) {
        // only update if there's still state for this name.
        //
        // if there's no state for this name it's because the set of target
        // names changed and we're not interested anymore.
        if let Some(state) = self.0.get_mut(hostname) {
            state.in_flight = false;
            state.merge_answer(resolved_at, answer);
        }
    }

    fn pin(&mut self, hostname: Hostname, port: u16) -> bool {
        let (mut created, name_info) = match self.0.entry(hostname) {
            btree_map::Entry::Vacant(entry) => (true, entry.insert(Default::default())),
            btree_map::Entry::Occupied(entry) => (false, entry.into_mut()),
        };

        let (port_created, port_info) = match name_info.ports.entry(port) {
            btree_map::Entry::Vacant(entry) => (true, entry.insert(Default::default())),
            btree_map::Entry::Occupied(entry) => (false, entry.into_mut()),
        };
        created |= port_created;
        port_info.pinned = true;

        if let Some(addrs) = &name_info.last_addrs {
            port_info.set_addrs(port, addrs);
        }

        created
    }

    fn remove(&mut self, hostname: &Hostname, port: u16) {
        let mut remove = false;
        if let Some(entry) = self.0.get_mut(hostname) {
            entry.ports.remove(&port);
            remove = entry.ports.is_empty();
        };

        if remove {
            self.0.remove(hostname);
        }
    }

    fn update_all(&mut self, new_names: impl IntoIterator<Item = (Hostname, u16)>) -> bool {
        // build an index of name -> [port] for the union of all names in the
        // new set of names and the old set of names.
        //
        // this currently involves recloning every key in this map, but that
        // should be ok since we expect hostname clones to be (relatively)
        // cheap.
        let mut names: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for name in self.0.keys() {
            names.insert(name.clone(), Vec::new());
        }
        for (name, port) in new_names {
            names.entry(name).or_default().push(port);
        }

        // iterate through the names index, for every set of ports, modify the
        // name info so it only contains the listed ports or any existing pinned
        // ports. the APIs for removing an entry we've already creatd here are not
        // good, so don't actually do removal in this step.
        let mut changed = false;
        for (name, new_ports) in &names {
            let name_info = self.0.entry(name.clone()).or_default();

            let mut to_remove = BTreeSet::new();
            for (port, port_info) in &name_info.ports {
                if port_info.pinned {
                    continue;
                }
                to_remove.insert(*port);
            }

            for port in new_ports {
                to_remove.remove(port);
                if let btree_map::Entry::Vacant(e) = name_info.ports.entry(*port) {
                    changed = true;
                    e.insert(PortInfo::default());
                }
            }

            for port in to_remove {
                changed |= name_info.ports.remove(&port).is_some();
            }
        }

        // take another pass through to remove every (k, v) pair where v
        // doesn't actually have any ports to keep track of.
        self.0.retain(|_, info| !info.ports.is_empty());

        changed
    }

    #[cfg(test)]
    fn names_and_ports(&self) -> Vec<(&str, Vec<u16>)> {
        self.0
            .iter()
            .map(|(name, info)| {
                let name = name.as_ref();
                let ports = info.ports.keys().cloned().collect();
                (name, ports)
            })
            .collect()
    }
}

#[cfg(test)]
mod test {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    #[test]
    fn test_resolv_conf_macos() {
        let macos_resolv = b"
#
# macOS Notice
#
# This file is not consulted for DNS hostname resolution, address
# resolution, or the DNS query routing mechanism used by most
# processes on this system.
#
# To view the DNS configuration used by this system, use:
#   scutil --dns
#
# SEE ALSO
#   dns-sd(1), scutil(8)
#
# This file is automatically generated.
#
search localdomain
nameserver 123.456.789.123
";
        assert_eq!(
            SystemConfig {
                ndots: 1,
                search: vec![Hostname::from_static("localdomain")],
            },
            parse_resolv_conf("/kube/etc/resolv.conf", macos_resolv).unwrap()
        );
    }

    #[test]
    fn test_resolv_conf_kube() {
        let kube_resolv = b"
nameserver 192.168.194.138
; another comment
nameserver fd07:b51a:cc66:a:8000::a # after stuff
# a weird inline comment
search default.svc.cluster.local svc.cluster.local cluster.local
options extra:hello ndots:5 not-valid
";

        assert_eq!(
            SystemConfig {
                ndots: 5,
                search: [
                    "default.svc.cluster.local",
                    "svc.cluster.local",
                    "cluster.local",
                ]
                .into_iter()
                .map(Hostname::from_static)
                .collect()
            },
            parse_resolv_conf("/kube/etc/resolv.conf", kube_resolv).unwrap()
        );
    }

    #[test]
    fn test_resolv_conf_invalid_search() {
        let conf = b"
search default.svc$$$cluster.local svc.cluster.local cluster.local
options ndots:5";
        let err = parse_resolv_conf("bad", conf).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { line: 1, .. }));
    }

    #[test]
    fn test_resolv_conf_invalid_ndots() {
        let conf = b"
search default.svc.cluster.local svc.cluster.local cluster.local
options ndots:1 ndots:a-potato ndots:3";
        let err = parse_resolv_conf("bad", conf).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { line: 2, .. }));
    }

    #[inline]
    fn update_all(
        resolver: &mut ResolverState,
        names: impl IntoIterator<Item = (&'static str, u16)>,
    ) {
        resolver.update_all(
            names
                .into_iter()
                .map(|(name, port)| (Hostname::from_static(name), port)),
        );
    }

    #[test]
    fn test_answers() {
        let mut resolver = ResolverState::default();

        update_all(
            &mut resolver,
            [("www.junctionlabs.io", 80), ("www.junctionlabs.io", 443)],
        );

        resolver.insert_answer(
            &Hostname::from_static("www.junctionlabs.io"),
            Instant::now(),
            // the port here shouldn't matter
            Ok(vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1234)]),
        );

        assert_eq!(
            resolver
                .get_endpoints(&Hostname::from_static("www.junctionlabs.io"), 80)
                .as_deref(),
            Some(&EndpointGroup::from_dns_addrs(vec![SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                80,
            )])),
        );
        assert_eq!(
            resolver
                .get_endpoints(&Hostname::from_static("www.junctionlabs.io"), 443)
                .as_deref(),
            Some(&EndpointGroup::from_dns_addrs(vec![SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                443,
            )])),
        );
        assert_eq!(
            resolver
                .get_endpoints(&Hostname::from_static("www.junctionlabs.io"), 1234)
                .as_deref(),
            None,
        );
    }

    #[test]
    fn test_resolver_tasks_next() {
        let mut resolver = ResolverState::default();

        update_all(
            &mut resolver,
            [
                ("doesnotexistihopereallybad.com", 80),
                ("www.junctionlabs.io", 80),
                ("www.junctionlabs.io", 443),
            ],
        );

        let now = Instant::now();
        // there should be two tasks available, the fourth next_name should
        // return nothing.
        assert!(resolver.next_name(now).is_some());
        assert!(resolver.next_name(now).is_some());
        assert!(resolver.next_name(now).is_none());

        assert_eq!(
            resolver.names_and_ports(),
            &[
                ("doesnotexistihopereallybad.com", vec![80]),
                ("www.junctionlabs.io", vec![80, 443]),
            ]
        );

        // resolve one name.
        resolver.insert_answer(
            &Hostname::from_static("www.junctionlabs.io"),
            now,
            Ok(vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 80)]),
        );
        // with a timestamp in the past, there should be no name available
        assert!(resolver.next_name(now - Duration::from_millis(1)).is_none());
        // with a timestamp in the future, there should be one name available
        assert!(resolver.next_name(now + Duration::from_millis(1)).is_some());
        assert!(resolver.next_name(now + Duration::from_millis(1)).is_none());

        assert_eq!(
            resolver.names_and_ports(),
            &[
                ("doesnotexistihopereallybad.com", vec![80]),
                ("www.junctionlabs.io", vec![80, 443]),
            ]
        );
    }

    #[test]
    fn test_pinned_name() {
        let mut resolver = ResolverState::default();

        resolver.pin(Hostname::from_static("important.com"), 1234);

        update_all(&mut resolver, [("www.example.com", 80)]);
        assert_eq!(
            resolver.names_and_ports(),
            &[("important.com", vec![1234]), ("www.example.com", vec![80]),]
        );

        update_all(&mut resolver, [("www.newthing.com", 80)]);
        assert_eq!(
            resolver.names_and_ports(),
            &[
                ("important.com", vec![1234]),
                ("www.newthing.com", vec![80]),
            ]
        );

        update_all(&mut resolver, [("www.newthing.com", 443)]);
        assert_eq!(
            resolver.names_and_ports(),
            &[
                ("important.com", vec![1234]),
                ("www.newthing.com", vec![443]),
            ]
        );

        resolver.remove(&Hostname::from_static("important.com"), 1234);
        update_all(&mut resolver, [("www.newthing.com", 443)]);
        assert_eq!(
            resolver.names_and_ports(),
            &[("www.newthing.com", vec![443]),]
        );
    }

    #[test]
    fn test_pin_new_port() {
        let mut resolver = ResolverState::default();

        update_all(
            &mut resolver,
            [("www.junctionlabs.io", 80), ("www.junctionlabs.io", 443)],
        );

        resolver.insert_answer(
            &Hostname::from_static("www.junctionlabs.io"),
            Instant::now(),
            // the port here shouldn't matter
            Ok(vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1234)]),
        );

        assert!(
            !resolver.pin(Hostname::from_static("www.junctionlabs.io"), 443),
            "should not return true when the same port is inserted"
        );
        assert!(
            resolver.pin(Hostname::from_static("www.junctionlabs.io"), 7777),
            "should return true when a new port is inserted"
        );

        let endpoints: Vec<_> = resolver
            .get_endpoints(&Hostname::from_static("www.junctionlabs.io"), 7777)
            .unwrap()
            .iter()
            .cloned()
            .collect();
        assert_eq!(
            endpoints,
            vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7777,)]
        );
    }

    #[test]
    fn test_reset_drops_inflight() {
        let mut resolver = ResolverState::default();

        update_all(&mut resolver, [("www.example.com", 8910)]);

        let now = Instant::now();

        // take one name
        assert!(resolver.next_name(now).is_some());

        // reset while the name is in-flight. should have one more name to take.
        update_all(&mut resolver, [("www.junctionlabs.io", 8910)]);
        assert!(resolver.next_name(now).is_some());
        assert!(resolver.next_name(now).is_none());

        // inserting the old answer shouldn't do anything
        resolver.insert_answer(
            &Hostname::from_static("www.example.com"),
            now,
            Ok(vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 80)]),
        );
        assert_eq!(
            resolver.names_and_ports(),
            &[("www.junctionlabs.io", vec![8910])]
        );
    }
}
