//! Reading `[services.*]`.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

use super::{RawDuration, ShellValue};
use crate::services::{Listener, Ready, Service, ServiceKind};
use crate::task::Location;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawService {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    serve: Option<RawServe>,
    #[serde(default)]
    deps: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    shell: Option<ShellValue>,
    #[serde(default)]
    ready: Option<RawReady>,
    #[serde(default)]
    ready_timeout: Option<RawDuration>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServe {
    dir: String,
    #[serde(default)]
    listen: Vec<RawListener>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawListener {
    #[serde(default)]
    name: Option<String>,
    addr: String,
    #[serde(default)]
    tls: Option<RawTls>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTls {
    cert: String,
    key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReady {
    #[serde(default)]
    tcp: Option<String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    log: Option<String>,
    #[serde(default)]
    http: Option<String>,
}

pub(super) fn convert(name: &str, raw: RawService, at: &Location) -> Result<Service, String> {
    if !super::steps::valid_var_name(name) {
        return Err(format!(
            "`{name}` cannot name a service: a name is letters, digits, `_` and `-`"
        ));
    }
    let kind = match (raw.command, raw.serve) {
        (Some(command), None) => ServiceKind::Command {
            command,
            shell: raw.shell.map(ShellValue::into_argv),
        },
        (None, Some(serve)) => {
            if raw.shell.is_some() || raw.ready.is_some() {
                return Err(
                    "a `serve` service runs no command, so it takes no `shell` or `ready`".into(),
                );
            }
            let mut listeners = Vec::new();
            for l in serve.listen {
                if l.name
                    .as_deref()
                    .is_some_and(|n| !super::steps::valid_var_name(n))
                {
                    return Err(format!(
                        "`{}` cannot name a listener: a name is letters, digits, `_` and `-`",
                        l.name.unwrap_or_default()
                    ));
                }
                listeners.push(Listener {
                    name: l.name,
                    addr: l.addr,
                    tls: l.tls.map(|t| (t.cert, t.key)),
                });
            }
            if listeners.is_empty() {
                listeners.push(Listener {
                    name: None,
                    addr: "127.0.0.1:0".into(),
                    tls: None,
                });
            }
            let mut seen = std::collections::BTreeSet::new();
            for n in listeners.iter().filter_map(|l| l.name.as_ref()) {
                if !seen.insert(n) || ["port", "addr", "url"].contains(&n.as_str()) {
                    return Err(format!("listener name `{n}` is used twice or taken"));
                }
            }
            ServiceKind::Serve {
                dir: serve.dir,
                listeners,
            }
        }
        (Some(_), Some(_)) => return Err("a service has a `command` or `serve`, not both".into()),
        (None, None) => return Err("a service needs a `command` or `serve`".into()),
    };
    let ready = match raw.ready {
        None => None,
        Some(r) => {
            let given: Vec<Ready> = [
                r.tcp.map(Ready::Tcp),
                r.file.map(Ready::File),
                r.log.map(Ready::Log),
                r.http.map(Ready::Http),
            ]
            .into_iter()
            .flatten()
            .collect();
            match <[Ready; 1]>::try_from(given) {
                Ok([one]) => {
                    if let Ready::Log(p) = &one {
                        crate::steps::compile(p)?;
                    }
                    if let Ready::Http(u) = &one {
                        if !u.starts_with("http://") {
                            return Err("`ready.http` is an http:// URL".into());
                        }
                    }
                    Some(one)
                }
                Err(_) => {
                    return Err(
                        "`ready` is one of { tcp = \"host:port\" }, { file = \"…\" }, { log = '…' } or { http = \"http://…\" }"
                            .into(),
                    )
                }
            }
        }
    };
    let ready_timeout = raw
        .ready_timeout
        .map(|d| d.parse())
        .transpose()
        .map_err(|e| format!("ready_timeout: {e}"))?
        .unwrap_or(Duration::from_secs(30));
    Ok(Service {
        name: name.to_string(),
        description: raw.description,
        kind,
        deps: raw.deps,
        cwd: raw.cwd,
        env: raw.env,
        ready,
        ready_timeout,
        source: Some(at.clone()),
    })
}
