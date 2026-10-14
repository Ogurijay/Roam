//! 局域网自动发现（mDNS）：被控端广播自己，主机在布局页面里列出"附近的设备"。

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use roam_proto::Os;
use serde::Serialize;

const SERVICE: &str = "_roam._udp.local.";

/// 广播中的服务，丢弃时自动停止广播。
pub struct Advertiser {
    daemon: ServiceDaemon,
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

pub fn advertise(id: &str, name: &str, ticket: &str) -> Result<Advertiser> {
    let daemon = ServiceDaemon::new()?;
    let short = &id[..id.len().min(10)];
    // 实例名最长 63 字节
    let mut instance = format!("{name} {short}");
    while instance.len() > 63 {
        instance.remove(0);
    }
    let os = format!("{:?}", Os::current());
    let props = [
        ("id", id),
        ("name", name),
        ("os", os.as_str()),
        ("t", ticket),
    ];
    let info = ServiceInfo::new(
        SERVICE,
        &instance,
        &format!("roam-{short}.local."),
        "",
        1,
        &props[..],
    )?
    .enable_addr_auto();
    daemon.register(info)?;
    Ok(Advertiser { daemon })
}

#[derive(Debug, Clone, Serialize)]
pub struct Discovered {
    pub id: String,
    pub name: String,
    pub os: Option<Os>,
    pub ticket: String,
}

/// 局域网里正在广播的被控设备，按 ID 索引。
#[derive(Clone, Default)]
pub struct Nearby {
    inner: Arc<Mutex<HashMap<String, (String, Discovered)>>>,
}

impl Nearby {
    pub fn list(&self) -> Vec<Discovered> {
        let mut list: Vec<_> = self
            .inner
            .lock()
            .unwrap()
            .values()
            .map(|(_, d)| d.clone())
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }
}

/// 在后台线程中持续浏览，返回的守护进程句柄需要保持存活。
pub fn browse(nearby: Nearby) -> Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    let events = daemon.browse(SERVICE)?;
    std::thread::Builder::new()
        .name("roam-mdns".into())
        .spawn(move || {
            while let Ok(event) = events.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let get = |k: &str| info.get_property_val_str(k).map(str::to_string);
                        let (Some(id), Some(ticket)) = (get("id"), get("t")) else {
                            continue;
                        };
                        let os = match get("os").as_deref() {
                            Some("Windows") => Some(Os::Windows),
                            Some("MacOs") => Some(Os::MacOs),
                            Some("Linux") => Some(Os::Linux),
                            _ => None,
                        };
                        let found = Discovered {
                            name: get("name").unwrap_or_else(|| id[..10].to_string()),
                            id: id.clone(),
                            os,
                            ticket,
                        };
                        let fullname = info.get_fullname().to_string();
                        nearby.inner.lock().unwrap().insert(id, (fullname, found));
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        nearby
                            .inner
                            .lock()
                            .unwrap()
                            .retain(|_, (name, _)| *name != fullname);
                    }
                    _ => {}
                }
            }
        })?;
    Ok(daemon)
}
