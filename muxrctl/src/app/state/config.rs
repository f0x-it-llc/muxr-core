//! Bind-address form state: the Config dialog's model and messages.

use std::net::Ipv4Addr;

/// The three explainer lines shown under the bind form.
///
/// Rendered verbatim by `tui/views/config_dialog.rs`; kept here so the copy
/// lives beside the state it explains rather than in the view.
pub const BIND_EXPLAIN: &[&str] = &[
    "127.0.0.1 — only this machine; a phone cannot reach it.",
    "0.0.0.0 — every interface; the QR advertises a real address for you.",
    "A specific IP — only that interface; the QR advertises it. Port: 50051.",
];

/// State for the Config dialog (bind address form + reachable-IP picker).
#[derive(Debug, Clone, Default)]
pub struct ConfigState {
    /// Editable host part of the bind address.
    pub host: String,
    /// Editable port part of the bind address.
    pub port: String,
    /// Non-loopback reachable IPv4 addresses discovered from interfaces.
    pub reachable_ips: Vec<Ipv4Addr>,
    /// The IP the picker's cursor rests on, if any.
    pub ip_cursor: Option<Ipv4Addr>,
    /// Whether the picker's option panel is open.
    pub ip_open: bool,
    /// Directory where TLS certs are stored (display only).
    pub cert_dir: String,
    /// Extra advertise SANs from the `MUXRD_SAN` env var, merged into the
    /// cert SANs alongside the reachable IPs (e.g. a tailnet IP not visible as a
    /// local interface inside a container). Loaded via `ConfigLoaded`.
    pub advertise_sans: Vec<String>,
    /// True while a LoadConfig or SaveBind task is in flight.
    pub loading: bool,
    /// Validation / save error shown in the dialog, if any.
    pub error: Option<String>,
    /// True between dispatching `SaveBind` and the `ActionOk` that confirms it,
    /// which is what closes the dialog — a save is only "done" once the write
    /// came back.
    pub pending_save: bool,
}

impl ConfigState {
    /// Return the current bind address as `"host:port"`.
    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Populate form fields from a resolved bind address string.
    pub fn apply_bind_addr(&mut self, addr: &str) {
        if let Some(colon) = addr.rfind(':') {
            self.host = addr[..colon].to_string();
            self.port = addr[colon + 1..].to_string();
        } else {
            self.host = addr.to_string();
            self.port = "50051".to_string();
        }
    }
}

/// Everything the Config dialog can ask for.
#[derive(Debug, Clone)]
pub enum ConfigMsg {
    /// The host field was edited; carries the whole new value.
    HostChanged(String),
    /// The port field was edited; carries the whole new value.
    PortChanged(String),
    /// The IP picker's panel opened (`true`) or closed (`false`).
    IpOpenChanged(bool),
    /// The picker's option cursor moved onto this IP.
    IpFocused(Ipv4Addr),
    /// An IP was committed: it becomes the bind host and the panel closes.
    IpPicked(Ipv4Addr),
    /// Validate and persist the bind address.
    Save,
    /// Close the dialog without saving.
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_state_apply_bind_addr() {
        let mut cs = ConfigState::default();
        cs.apply_bind_addr("0.0.0.0:50051");
        assert_eq!(cs.host, "0.0.0.0");
        assert_eq!(cs.port, "50051");
    }

    #[test]
    fn config_state_apply_bind_addr_without_port() {
        let mut cs = ConfigState::default();
        cs.apply_bind_addr("127.0.0.1");
        assert_eq!(cs.host, "127.0.0.1");
        assert_eq!(cs.port, "50051");
    }

    #[test]
    fn config_state_bind_addr_roundtrip() {
        let mut cs = ConfigState::default();
        cs.apply_bind_addr("192.168.1.5:50051");
        assert_eq!(cs.bind_addr(), "192.168.1.5:50051");
    }

    #[test]
    fn bind_explain_has_three_lines() {
        assert_eq!(BIND_EXPLAIN.len(), 3);
        assert!(BIND_EXPLAIN.iter().all(|line| !line.is_empty()));
    }
}
