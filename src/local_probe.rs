use std::{
    net::{Ipv6Addr, SocketAddr, UdpSocket},
    sync::atomic::{AtomicU16, Ordering},
    time::Duration,
};

#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

use tokio::{process::Command, time::timeout};
use tracing::{info, warn};

use crate::{
    client::{ClientError, Ipv6Probe, ProbeResult},
    config::ProbeConfig,
};

const PROBE_DOMAIN: &str = "openwrt.org";
static DNS_QUERY_ID: AtomicU16 = AtomicU16::new(1);

#[derive(Debug, Clone)]
pub struct LocalProbeClient {
    config: ProbeConfig,
}

impl LocalProbeClient {
    pub fn new(config: ProbeConfig) -> Self {
        Self { config }
    }

    async fn resolve_probe_address(&self) -> Result<Ipv6Addr, ClientError> {
        let interface = self.config.interface.clone();
        let dns_server = self
            .config
            .dns_server
            .parse::<SocketAddr>()
            .map_err(|error| ClientError::LocalDns(error.to_string()))?;
        let timeout = Duration::from_secs(self.config.dns_timeout_seconds);
        tokio::task::spawn_blocking(move || {
            resolve_aaaa_on_interface(&interface, dns_server, PROBE_DOMAIN, timeout)
        })
        .await
        .map_err(|error| ClientError::LocalDns(error.to_string()))?
        .map_err(ClientError::LocalDns)
    }
}

#[async_trait::async_trait]
impl Ipv6Probe for LocalProbeClient {
    async fn check_ipv6(&self) -> Result<ProbeResult, ClientError> {
        let started = tokio::time::Instant::now();
        let address = self.resolve_probe_address().await?;
        let timeout_seconds = self.config.ping_timeout_seconds;
        let interface = self.config.interface.clone();

        let ping_args = [
            "-I".to_owned(),
            interface.clone(),
            "-c".to_owned(),
            "1".to_owned(),
            "-W".to_owned(),
            timeout_seconds.to_string(),
            address.to_string(),
        ];
        let mut command = Command::new("ping6");
        command
            .args(&ping_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command.kill_on_drop(true);

        let (ping_program, child) = match command.spawn() {
            Ok(child) => ("ping6", child),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut fallback = Command::new("ping");
                fallback
                    .arg("-6")
                    .args(&ping_args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                fallback.kill_on_drop(true);
                let child = fallback.spawn().map_err(|fallback_error| {
                    ClientError::LocalPingSpawn(format!(
                        "ping6 and ping -6 on {interface}: {fallback_error}"
                    ))
                })?;
                ("ping -6", child)
            }
            Err(error) => {
                return Err(ClientError::LocalPingSpawn(format!(
                    "ping6 on {interface}: {error}"
                )))
            }
        };
        let output = timeout(
            Duration::from_secs(timeout_seconds),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| ClientError::LocalPingTimeout(timeout_seconds))?
        .map_err(|error| ClientError::LocalPingSpawn(error.to_string()))?;

        let result = ProbeResult {
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            duration: started.elapsed(),
        };
        if result.success {
            info!(
                interface = %interface,
                dns_server = %self.config.dns_server,
                target = %address,
                ping_program,
                exit_code = ?result.exit_code,
                duration_ms = result.duration.as_millis() as u64,
                "local IPv6 probe completed"
            );
        } else {
            warn!(
                interface = %interface,
                dns_server = %self.config.dns_server,
                target = %address,
                ping_program,
                exit_code = ?result.exit_code,
                stderr = %result.stderr,
                "local IPv6 probe failed"
            );
        }
        Ok(result)
    }
}

fn resolve_aaaa_on_interface(
    interface: &str,
    dns_server: SocketAddr,
    domain: &str,
    timeout: Duration,
) -> Result<Ipv6Addr, String> {
    let bind_address = if dns_server.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind_address).map_err(|error| error.to_string())?;
    socket
        .set_read_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;
    socket
        .set_write_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;
    bind_socket_to_interface(&socket, interface)?;

    let id = DNS_QUERY_ID.fetch_add(1, Ordering::Relaxed);
    let query = build_dns_query(id, domain)?;
    socket
        .send_to(&query, dns_server)
        .map_err(|error| error.to_string())?;

    let mut response = [0u8; 4096];
    let (length, _) = socket
        .recv_from(&mut response)
        .map_err(|error| error.to_string())?;
    parse_dns_response(&response[..length], id)
}

#[cfg(target_os = "linux")]
fn bind_socket_to_interface(socket: &UdpSocket, interface: &str) -> Result<(), String> {
    let interface = std::ffi::CString::new(interface)
        .map_err(|_| "probe interface contains a NUL byte".to_owned())?;
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            interface.as_ptr().cast(),
            interface.as_bytes_with_nul().len() as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn bind_socket_to_interface(_socket: &UdpSocket, _interface: &str) -> Result<(), String> {
    Err("interface-bound DNS probing is supported only on Linux".into())
}

fn build_dns_query(id: u16, domain: &str) -> Result<Vec<u8>, String> {
    let mut packet = Vec::with_capacity(64);
    packet.extend_from_slice(&id.to_be_bytes());
    packet.extend_from_slice(&0x0100u16.to_be_bytes());
    packet.extend_from_slice(&1u16.to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes());
    packet.extend_from_slice(&0u16.to_be_bytes());

    for label in domain.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err("invalid DNS name".into());
        }
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0);
    packet.extend_from_slice(&28u16.to_be_bytes());
    packet.extend_from_slice(&1u16.to_be_bytes());
    Ok(packet)
}

fn parse_dns_response(packet: &[u8], id: u16) -> Result<Ipv6Addr, String> {
    if packet.len() < 12 {
        return Err("DNS response is too short".into());
    }
    if u16::from_be_bytes([packet[0], packet[1]]) != id {
        return Err("DNS response transaction ID mismatch".into());
    }
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    let response_code = flags & 0x000f;
    if response_code != 0 {
        return Err(format!("DNS response code {response_code}"));
    }

    let question_count = u16::from_be_bytes([packet[4], packet[5]]) as usize;
    let answer_count = u16::from_be_bytes([packet[6], packet[7]]) as usize;
    let mut offset = 12;
    for _ in 0..question_count {
        offset = skip_dns_name(packet, offset)?;
        offset = offset.checked_add(4).ok_or("DNS question overflow")?;
        if offset > packet.len() {
            return Err("DNS question is truncated".into());
        }
    }

    for _ in 0..answer_count {
        offset = skip_dns_name(packet, offset)?;
        if offset.checked_add(10).ok_or("DNS answer overflow")? > packet.len() {
            return Err("DNS answer is truncated".into());
        }
        let record_type = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let class = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]);
        let data_length = u16::from_be_bytes([packet[offset + 8], packet[offset + 9]]) as usize;
        offset += 10;
        if offset.checked_add(data_length).ok_or("DNS data overflow")? > packet.len() {
            return Err("DNS record data is truncated".into());
        }
        if record_type == 28 && class == 1 && data_length == 16 {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&packet[offset..offset + 16]);
            return Ok(Ipv6Addr::from(octets));
        }
        offset += data_length;
    }
    Err("DNS response contains no AAAA record".into())
}

fn skip_dns_name(packet: &[u8], mut offset: usize) -> Result<usize, String> {
    loop {
        if offset >= packet.len() {
            return Err("DNS name is truncated".into());
        }
        let length = packet[offset];
        if length & 0xc0 == 0xc0 {
            if offset + 2 > packet.len() {
                return Err("DNS compression pointer is truncated".into());
            }
            return Ok(offset + 2);
        }
        if length & 0xc0 != 0 {
            return Err("invalid DNS label length".into());
        }
        offset += 1;
        if length == 0 {
            return Ok(offset);
        }
        offset = offset
            .checked_add(length as usize)
            .ok_or("DNS name overflow")?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_and_parses_aaaa_response() {
        let query = build_dns_query(7, "openwrt.org").unwrap();
        assert_eq!(u16::from_be_bytes([query[0], query[1]]), 7);
        assert_eq!(
            u16::from_be_bytes([query[query.len() - 4], query[query.len() - 3]]),
            28
        );

        let mut response = query;
        response[2] = 0x81;
        response[3] = 0x80;
        response[6] = 0;
        response[7] = 1;
        response.extend_from_slice(&[0xc0, 0x0c]);
        response.extend_from_slice(&28u16.to_be_bytes());
        response.extend_from_slice(&1u16.to_be_bytes());
        response.extend_from_slice(&60u32.to_be_bytes());
        response.extend_from_slice(&16u16.to_be_bytes());
        response.extend_from_slice(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);

        assert_eq!(
            parse_dns_response(&response, 7).unwrap(),
            "2001:db8::1".parse::<Ipv6Addr>().unwrap()
        );
    }
}
