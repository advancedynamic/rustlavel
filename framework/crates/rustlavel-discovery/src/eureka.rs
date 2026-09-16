//! Eureka's wire format, so a Spring service can register here unmodified.
//!
//! The shape is not pretty and it is not ours to improve. `{"$": 8080,
//! "@enabled": "true"}` is what a port looks like because the JSON is a
//! mechanical translation of the XML, and `dataCenterInfo` carries a Java class
//! name that clients check. Deviating from any of it in the name of taste means
//! a client that silently will not register, so every oddity below is
//! deliberate and the comments say which are load-bearing.
//!
//! What is implemented is the part a client uses: register, heartbeat, cancel,
//! status, and the two reads. Deltas are not — `versions__delta` is answered
//! with a full registry, which is correct and merely less efficient, and a
//! client that asks for a delta and receives everything behaves properly.

use rustlavel_core::Json;

use crate::instance::{Instance, Status, now};

/// Read the instance out of a registration body.
///
/// Every field a client may omit has a default that fails safe: no status is
/// `Starting` rather than `Up`, so an instance never begins taking traffic
/// because a field was missing.
pub fn read_instance(body: &Json) -> Option<Instance> {
    // Clients post `{"instance": {...}}`; replication between peers sends the
    // inner object alone. Accept both rather than making the caller know.
    let body = body.get("instance").unwrap_or(body);

    let app = body.get("app").and_then(Json::as_str)?.to_ascii_uppercase();
    let host = body
        .get("hostName")
        .and_then(Json::as_str)
        .or_else(|| body.get("ipAddr").and_then(Json::as_str))?
        .to_string();

    let port = port_of(body, "port").unwrap_or(80);
    let secure_port = port_of(body, "securePort");
    let secure = secure_port.is_some();

    let mut instance = Instance::new(app, host, secure_port.unwrap_or(port)).secure(secure);

    if let Some(id) = body.get("instanceId").and_then(Json::as_str) {
        instance = instance.id(id);
    }
    instance.status = body
        .get("status")
        .and_then(Json::as_str)
        .map(Status::parse)
        // No status at all is `Starting`, never `Up`. An instance must not
        // begin taking traffic because a client left a field out.
        .unwrap_or(Status::Starting);

    if let Some(lease) = body.get("leaseInfo")
        && let Some(interval) = lease.get("renewalIntervalInSecs").and_then(Json::as_f64)
    {
        instance = instance.renew_every(interval as u64);
    }

    if let Some(Json::Object(entries)) = body.get("metadata") {
        for (key, value) in entries {
            if let Some(text) = value.as_str() {
                instance = instance.meta(key.clone(), text);
            }
        }
    }

    Some(instance)
}

/// `{"$": 8080, "@enabled": "true"}`, and a plain number for clients that send
/// one.
///
/// Disabled ports are read as absent: `@enabled: "false"` on `securePort` is
/// how most instances say they have no TLS, and treating the number beside it
/// as usable would send https to a port that speaks http.
fn port_of(body: &Json, field: &str) -> Option<u16> {
    let value = body.get(field)?;

    if let Some(number) = value.as_f64() {
        return Some(number as u16);
    }

    let enabled = value
        .get("@enabled")
        .and_then(|flag| flag.as_str().map(|text| text == "true").or_else(|| flag.as_bool()))
        .unwrap_or(true);
    if !enabled {
        return None;
    }
    value.get("$").and_then(Json::as_f64).map(|number| number as u16)
}

/// One instance, as Eureka renders it.
pub fn instance_json(instance: &Instance) -> Json {
    let (port, secure_port) = match instance.secure {
        true => (80, instance.port),
        false => (instance.port, 443),
    };

    Json::object([
        ("instanceId", Json::from(instance.id.as_str())),
        ("hostName", Json::from(instance.host.as_str())),
        ("app", Json::from(instance.service.as_str())),
        ("ipAddr", Json::from(instance.host.as_str())),
        ("status", Json::from(instance.status.as_str())),
        ("overriddenStatus", Json::from("UNKNOWN")),
        ("port", enabled_port(port, !instance.secure)),
        ("securePort", enabled_port(secure_port, instance.secure)),
        ("countryId", Json::from(1.0)),
        // The class name is load-bearing: a client that does not recognise it
        // refuses the record. It is not decoration and must not be tidied.
        (
            "dataCenterInfo",
            Json::object([
                ("@class", Json::from("com.netflix.appinfo.InstanceInfo$DefaultDataCenterInfo")),
                ("name", Json::from("MyOwn")),
            ]),
        ),
        (
            "leaseInfo",
            Json::object([
                ("renewalIntervalInSecs", Json::from(instance.renew_interval as f64)),
                ("durationInSecs", Json::from((instance.renew_interval * 3) as f64)),
                ("lastRenewalTimestamp", Json::from((instance.last_seen * 1000) as f64)),
            ]),
        ),
        (
            "metadata",
            Json::object(
                instance
                    .metadata
                    .iter()
                    .map(|(key, value)| (key.as_str(), Json::from(value.as_str())))
                    .collect::<Vec<_>>(),
            ),
        ),
        ("homePageUrl", Json::from(format!("{}/", instance.url()).as_str())),
        ("statusPageUrl", Json::from(format!("{}/info", instance.url()).as_str())),
        ("healthCheckUrl", Json::from(format!("{}/health", instance.url()).as_str())),
        ("vipAddress", Json::from(instance.service.as_str())),
        ("secureVipAddress", Json::from(instance.service.as_str())),
        ("lastUpdatedTimestamp", Json::from((now() * 1000) as f64)),
    ])
}

fn enabled_port(port: u16, enabled: bool) -> Json {
    Json::object([
        ("$", Json::from(port as f64)),
        // A string, not a boolean. It came from an XML attribute and clients
        // compare it as text.
        ("@enabled", Json::from(if enabled { "true" } else { "false" })),
    ])
}

/// `GET /eureka/apps` — every application and its instances.
pub fn applications_json(instances: &[Instance]) -> Json {
    let mut names: Vec<&str> = instances.iter().map(|i| i.service.as_str()).collect();
    names.sort_unstable();
    names.dedup();

    let applications: Vec<Json> = names
        .iter()
        .map(|name| {
            let mine: Vec<Json> = instances
                .iter()
                .filter(|instance| instance.service == *name)
                .map(instance_json)
                .collect();
            Json::object([("name", Json::from(*name)), ("instance", Json::Array(mine))])
        })
        .collect();

    Json::object([(
        "applications",
        Json::object([
            ("versions__delta", Json::from("1")),
            ("apps__hashcode", Json::from(hashcode(instances).as_str())),
            ("application", Json::Array(applications)),
        ]),
    )])
}

/// `GET /eureka/apps/{app}` — one application.
pub fn application_json(name: &str, instances: &[Instance]) -> Json {
    Json::object([(
        "application",
        Json::object([
            ("name", Json::from(name)),
            ("instance", Json::Array(instances.iter().map(instance_json).collect())),
        ]),
    )])
}

/// `UP_3_DOWN_1_`, the shape a client compares to decide whether anything moved.
///
/// Counts by status, in sorted order — that is the whole of it, and a client
/// that sees the same string skips the fetch.
fn hashcode(instances: &[Instance]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for instance in instances {
        let status = instance.status.as_str();
        match counts.iter_mut().find(|(held, _)| *held == status) {
            Some((_, count)) => *count += 1,
            None => counts.push((status, 1)),
        }
    }
    counts.sort_unstable();
    counts.iter().map(|(status, count)| format!("{status}_{count}_")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spring_registration() -> Json {
        // What a Spring Cloud client actually posts, trimmed to the fields that
        // matter here.
        Json::parse(
            r#"{"instance":{
                "instanceId":"orders:8761",
                "hostName":"10.0.0.7",
                "app":"ORDERS",
                "ipAddr":"10.0.0.7",
                "status":"UP",
                "port":{"$":8080,"@enabled":"true"},
                "securePort":{"$":443,"@enabled":"false"},
                "dataCenterInfo":{"@class":"com.netflix.appinfo.InstanceInfo$DefaultDataCenterInfo","name":"MyOwn"},
                "leaseInfo":{"renewalIntervalInSecs":10,"durationInSecs":30},
                "metadata":{"zone":"jakarta-a"}
            }}"#,
        )
        .unwrap()
    }

    #[test]
    fn a_spring_registration_is_read() {
        let instance = read_instance(&spring_registration()).expect("a readable registration");

        assert_eq!(instance.service, "ORDERS");
        assert_eq!(instance.id, "orders:8761");
        assert_eq!(instance.host, "10.0.0.7");
        assert_eq!(instance.port, 8080);
        assert_eq!(instance.status, Status::Up);
        assert_eq!(instance.renew_interval, 10);
        assert_eq!(instance.zone(), Some("jakarta-a"));
        assert!(!instance.secure, "`@enabled: false` was read as a usable TLS port");
    }

    /// Replication between peers sends the inner object alone.
    #[test]
    fn the_wrapper_is_optional() {
        let inner = spring_registration().get("instance").cloned().unwrap();
        assert_eq!(read_instance(&inner).unwrap().id, "orders:8761");
    }

    /// An instance must not begin taking traffic because a client left a field
    /// out.
    #[test]
    fn a_registration_with_no_status_is_starting_rather_than_up() {
        let body = Json::parse(r#"{"app":"orders","hostName":"h","port":{"$":80,"@enabled":"true"}}"#).unwrap();
        assert_eq!(read_instance(&body).unwrap().status, Status::Starting);
    }

    /// `@enabled: "false"` on `securePort` is how most instances say they have
    /// no TLS. Reading the number beside it would send https at a port
    /// speaking http.
    #[test]
    fn a_disabled_port_is_absent_rather_than_usable() {
        let body = Json::parse(
            r#"{"app":"a","hostName":"h","port":{"$":8080,"@enabled":"true"},"securePort":{"$":8443,"@enabled":"false"}}"#,
        )
        .unwrap();
        let instance = read_instance(&body).unwrap();
        assert!(!instance.secure);
        assert_eq!(instance.port, 8080);
    }

    #[test]
    fn a_registration_without_a_name_or_a_host_is_refused() {
        for body in [r#"{"hostName":"h"}"#, r#"{"app":"a"}"#, "{}"] {
            assert!(read_instance(&Json::parse(body).unwrap()).is_none(), "{body}");
        }
    }

    /// The class name is checked by clients. It is not decoration.
    #[test]
    fn the_rendered_instance_carries_the_fields_clients_check() {
        let rendered = instance_json(&Instance::new("orders", "10.0.0.7", 8080));
        let text = rendered.to_string();

        assert!(text.contains("com.netflix.appinfo.InstanceInfo$DefaultDataCenterInfo"), "{text}");
        assert!(text.contains(r#""@enabled":"true""#), "@enabled must be a string: {text}");
        assert!(text.contains(r#""app":"ORDERS""#), "{text}");
        assert!(text.contains(r#""vipAddress":"ORDERS""#), "{text}");
    }

    /// What a client posts must come back out the same, or it will re-register
    /// on every poll believing something changed.
    #[test]
    fn a_registration_survives_a_round_trip() {
        let original = read_instance(&spring_registration()).unwrap();
        let round_tripped = read_instance(&instance_json(&original)).unwrap();

        assert_eq!(round_tripped.service, original.service);
        assert_eq!(round_tripped.id, original.id);
        assert_eq!(round_tripped.host, original.host);
        assert_eq!(round_tripped.port, original.port);
        assert_eq!(round_tripped.status, original.status);
        assert_eq!(round_tripped.renew_interval, original.renew_interval);
        assert_eq!(round_tripped.zone(), original.zone());
    }

    #[test]
    fn the_applications_document_groups_instances_by_name() {
        let instances = vec![
            Instance::new("orders", "10.0.0.1", 8080),
            Instance::new("orders", "10.0.0.2", 8080),
            Instance::new("billing", "10.0.0.3", 8080),
        ];
        let text = applications_json(&instances).to_string();

        assert!(text.contains(r#""name":"ORDERS""#), "{text}");
        assert!(text.contains(r#""name":"BILLING""#), "{text}");
        assert!(text.contains("apps__hashcode"), "{text}");
    }

    /// The hashcode is how a client decides nothing moved and skips a fetch. It
    /// has to change when something does.
    #[test]
    fn the_hashcode_follows_the_registry() {
        let up = vec![Instance::new("orders", "10.0.0.1", 8080)];
        let mut down = up.clone();
        down[0].status = Status::Down;

        assert_eq!(hashcode(&up), "UP_1_");
        assert_ne!(hashcode(&up), hashcode(&down));
        assert_eq!(hashcode(&[]), "");
    }
}
