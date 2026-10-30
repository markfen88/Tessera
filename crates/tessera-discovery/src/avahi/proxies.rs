//! D-Bus proxies for the parts of the Avahi API we use
//! (`org.freedesktop.Avahi.*`, documented in Avahi's D-Bus API reference).
//!
//! Only the server is a real proxy; see [`Object`] for the rest.

#![allow(missing_docs)]

use zbus::zvariant::OwnedObjectPath;

pub use server::ServerProxy;

/// `AvahiProtocol`: IPv4.
pub const PROTO_INET: i32 = 0;
/// `AvahiProtocol`: IPv6.
pub const PROTO_INET6: i32 = 1;
/// `AvahiProtocol` / `AvahiIfIndex`: unspecified.
pub const UNSPEC: i32 = -1;

/// `AvahiPublishFlags`: do not publish the reverse (PTR) record of an address.
pub const PUBLISH_NO_REVERSE: u32 = 16;

/// `AvahiServerState`: running.
pub const SERVER_RUNNING: i32 = 2;

/// `AvahiEntryGroupState`: all entries are established.
pub const GROUP_ESTABLISHED: i32 = 2;
/// `AvahiEntryGroupState`: a name collision happened.
pub const GROUP_COLLISION: i32 = 3;
/// `AvahiEntryGroupState`: the group failed.
pub const GROUP_FAILURE: i32 = 4;

pub mod server {
    use zbus::zvariant::OwnedObjectPath;

    #[zbus::proxy(
        interface = "org.freedesktop.Avahi.Server",
        default_service = "org.freedesktop.Avahi",
        default_path = "/",
        gen_blocking = false
    )]
    pub trait Server {
        fn get_state(&self) -> zbus::Result<i32>;
        fn entry_group_new(&self) -> zbus::Result<OwnedObjectPath>;
        fn get_alternative_service_name(&self, name: &str) -> zbus::Result<String>;
        fn get_alternative_host_name(&self, name: &str) -> zbus::Result<String>;
        fn service_browser_new(
            &self,
            interface: i32,
            protocol: i32,
            type_: &str,
            domain: &str,
            flags: u32,
        ) -> zbus::Result<OwnedObjectPath>;
        #[allow(clippy::too_many_arguments)]
        fn service_resolver_new(
            &self,
            interface: i32,
            protocol: i32,
            name: &str,
            type_: &str,
            domain: &str,
            aprotocol: i32,
            flags: u32,
        ) -> zbus::Result<OwnedObjectPath>;
        #[zbus(signal)]
        fn state_changed(&self, state: i32, error: &str) -> zbus::Result<()>;
    }
}

/// An Avahi object (entry group, browser, resolver) driven by plain method
/// calls. Not a `zbus` proxy: a proxy for a well-known name subscribes to
/// name-owner changes and per-path signals, i.e. it costs match rules, and the
/// bus allows few per connection (the system bus of Debian: 512), which would
/// cap us at a few hundred entry groups or resolvers. Signals are taken from
/// one path-less match rule per interface instead.
#[derive(Debug, Clone)]
pub struct Object {
    conn: zbus::Connection,
    path: OwnedObjectPath,
    interface: &'static str,
}

impl Object {
    fn new(conn: &zbus::Connection, path: OwnedObjectPath, interface: &'static str) -> Self {
        Self {
            conn: conn.clone(),
            path,
            interface,
        }
    }

    /// The object path.
    #[must_use]
    pub fn path(&self) -> &str {
        self.path.as_str()
    }

    async fn call<B>(&self, member: &str, body: &B) -> zbus::Result<()>
    where
        B: serde::ser::Serialize + zbus::zvariant::DynamicType,
    {
        self.conn
            .call_method(
                Some("org.freedesktop.Avahi"),
                self.path.as_ref(),
                Some(self.interface),
                member,
                body,
            )
            .await
            .map(|_| ())
    }

    /// `Free`.
    pub async fn free(&self) -> zbus::Result<()> {
        self.call("Free", &()).await
    }
}

/// `org.freedesktop.Avahi.EntryGroup`.
#[derive(Debug, Clone)]
pub struct EntryGroup(Object);

impl EntryGroup {
    pub fn new(conn: &zbus::Connection, path: OwnedObjectPath) -> Self {
        Self(Object::new(conn, path, "org.freedesktop.Avahi.EntryGroup"))
    }

    pub fn path(&self) -> &str {
        self.0.path()
    }

    pub async fn free(&self) -> zbus::Result<()> {
        self.0.free().await
    }

    pub async fn commit(&self) -> zbus::Result<()> {
        self.0.call("Commit", &()).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn add_service(
        &self,
        interface: i32,
        protocol: i32,
        flags: u32,
        name: &str,
        type_: &str,
        domain: &str,
        host: &str,
        port: u16,
        txt: &[Vec<u8>],
    ) -> zbus::Result<()> {
        self.0
            .call(
                "AddService",
                &(
                    interface, protocol, flags, name, type_, domain, host, port, txt,
                ),
            )
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_service_txt(
        &self,
        interface: i32,
        protocol: i32,
        flags: u32,
        name: &str,
        type_: &str,
        domain: &str,
        txt: &[Vec<u8>],
    ) -> zbus::Result<()> {
        self.0
            .call(
                "UpdateServiceTxt",
                &(interface, protocol, flags, name, type_, domain, txt),
            )
            .await
    }

    pub async fn add_address(
        &self,
        interface: i32,
        protocol: i32,
        flags: u32,
        name: &str,
        address: &str,
    ) -> zbus::Result<()> {
        self.0
            .call("AddAddress", &(interface, protocol, flags, name, address))
            .await
    }
}

/// `org.freedesktop.Avahi.ServiceBrowser`.
#[derive(Debug, Clone)]
pub struct ServiceBrowser(Object);

impl ServiceBrowser {
    pub fn new(conn: &zbus::Connection, path: OwnedObjectPath) -> Self {
        Self(Object::new(
            conn,
            path,
            "org.freedesktop.Avahi.ServiceBrowser",
        ))
    }

    pub fn path(&self) -> &str {
        self.0.path()
    }
}

/// `org.freedesktop.Avahi.ServiceResolver`.
#[derive(Debug, Clone)]
pub struct ServiceResolver(Object);

impl ServiceResolver {
    pub fn new(conn: &zbus::Connection, path: OwnedObjectPath) -> Self {
        Self(Object::new(
            conn,
            path,
            "org.freedesktop.Avahi.ServiceResolver",
        ))
    }

    pub async fn free(&self) -> zbus::Result<()> {
        self.0.free().await
    }

    pub fn path(&self) -> &str {
        self.0.path()
    }
}
