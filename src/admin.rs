use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::app_config::AppConfig;
use crate::tunnel::tunnel_config;
use crate::utils;

const HTML_TEMPLATE: &str = include_str!("../assets/admin_config_page.html");
const CSS: &str = include_str!("../assets/admin_config_page.css");

pub async fn start_admin_server(
    listen: &SocketAddr,
    metrics_registry: utils::MetricsRegistry,
    app_config: Arc<AppConfig>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!("Admin server listening on {}", listen);

    loop {
        let (mut stream, addr) = listener.accept().await?;
        let registry = metrics_registry.clone();
        let config = app_config.clone();

        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let n = match stream.read(&mut buf).await {
                Ok(n) if n > 0 => n,
                Ok(_) => return,
                Err(e) => {
                    tracing::warn!("Admin server read error from {}: {}", addr, e);
                    return;
                }
            };

            let request = String::from_utf8_lossy(&buf[..n]);
            let first_line = request.lines().next().unwrap_or("");
            let parts: Vec<&str> = first_line.split_whitespace().collect();
            let method = parts.first().copied().unwrap_or("GET");
            let path = parts.get(1).copied().unwrap_or("/");

            tracing::debug!("Admin server request from {}: {} {}", addr, method, path);

            let (status, content_type, body) = match (method, path) {
                ("GET", "/metrics") => {
                    let metrics = utils::format_metrics(&registry);
                    ("200 OK", "text/plain; charset=utf-8", metrics.into_bytes())
                }
                ("GET", "/config") => match render_config_page(&config, None).await {
                    Ok(html) => ("200 OK", "text/html; charset=utf-8", html.into_bytes()),
                    Err(e) => (
                        "500 Internal Server Error",
                        "text/plain; charset=utf-8",
                        format!("Error rendering page: {}", e).into_bytes(),
                    ),
                },
                ("POST", "/config") => {
                    let body_str = extract_body(&request);
                    let result = handle_config_save(&config, &body_str).await;
                    match result {
                        Ok(()) => match render_config_page(
                            &config,
                            Some("Configuration saved. Tunnel client will reconnect."),
                        )
                        .await
                        {
                            Ok(html) => ("200 OK", "text/html; charset=utf-8", html.into_bytes()),
                            Err(e) => (
                                "500 Internal Server Error",
                                "text/plain; charset=utf-8",
                                format!("Error: {}", e).into_bytes(),
                            ),
                        },
                        Err(e) => match render_config_page(&config, Some(&format!("Error: {}", e)))
                            .await
                        {
                            Ok(html) => ("200 OK", "text/html; charset=utf-8", html.into_bytes()),
                            Err(e2) => (
                                "500 Internal Server Error",
                                "text/plain; charset=utf-8",
                                format!("Error: {}", e2).into_bytes(),
                            ),
                        },
                    }
                }
                ("GET", "/") => {
                    let body = "rsnova admin server\n\nEndpoints:\n  /metrics - Server metrics\n  /config - Configuration editor\n";
                    (
                        "200 OK",
                        "text/plain; charset=utf-8",
                        body.as_bytes().to_vec(),
                    )
                }
                _ => {
                    let body = "404 Not Found\n\nAvailable endpoints:\n  /metrics - Server metrics\n  /config - Configuration editor\n";
                    (
                        "404 Not Found",
                        "text/plain; charset=utf-8",
                        body.as_bytes().to_vec(),
                    )
                }
            };

            let response = format!(
                "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                status,
                content_type,
                body.len()
            );

            if let Err(e) = stream.write_all(response.as_bytes()).await {
                tracing::warn!("Admin server write header error to {}: {}", addr, e);
                return;
            }
            if let Err(e) = stream.write_all(&body).await {
                tracing::warn!("Admin server write body error to {}: {}", addr, e);
            }
        });
    }
}

fn extract_body(request: &str) -> String {
    if let Some(idx) = request.find("\r\n\r\n") {
        request[idx + 4..].to_string()
    } else {
        String::new()
    }
}

fn url_decode(s: &str) -> String {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                result.push(byte);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            result.push(b' ');
            i += 1;
            continue;
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).to_string()
}

fn parse_form_value(body: &str, key: &str) -> Option<String> {
    for pair in body.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && url_decode(k) == key
        {
            return Some(url_decode(v));
        }
    }
    None
}

async fn render_config_page(config: &AppConfig, alert: Option<&str>) -> anyhow::Result<String> {
    let reloadable = config.reloadable.lock().await;
    let s = &config.static_args;
    let is_server = s.role == "server";
    let is_tunnel = s.is_tunnel;

    // --- Connection fields (differ by role/mode) ---
    let connection_fields = if is_server {
        format!(
            r#"<div class="form-row">
      <div class="form-group half"><label>Listen</label><input type="text" value="{listen}" disabled /></div>
      <div class="form-group half"><label>Admin Listen</label><input type="text" value="{admin}" disabled /></div>
    </div>
    <div class="form-row">
      <div class="form-group half"><label>Certificate</label><input type="text" value="{cert}" disabled /></div>
      <div class="form-group half"><label>Key</label><input type="text" value="{key}" disabled /></div>
    </div>
    <div class="form-row">
      <div class="form-group half"><label>TLS Host</label><input type="text" value="{tls_host}" disabled /></div>
      <div class="form-group half"><label>Threads</label><input type="text" value="{threads}" disabled /></div>
    </div>
    <div class="form-row">
      <div class="form-group third"><label>Idle Timeout</label><input type="text" value="{idle}" disabled /></div>
      <div class="form-group third"><label>Max Connections</label><input type="text" value="{max_conn}" disabled /></div>
    </div>"#,
            listen = html_escape(&s.listen),
            admin = html_escape(&s.admin_listen),
            cert = html_escape(&s.cert),
            key = html_escape(&s.key),
            tls_host = html_escape(&s.tls_host),
            threads = s.threads,
            idle = s.idle_timeout_secs,
            max_conn = s.max_connections,
        )
    } else if is_tunnel {
        format!(
            r#"<div class="form-row">
      <div class="form-group half"><label>Remote</label><input type="text" value="{remote}" disabled /></div>
      <div class="form-group half"><label>TLS Host</label><input type="text" value="{tls_host}" disabled /></div>
    </div>
    <div class="form-row">
      <div class="form-group half"><label>Certificate</label><input type="text" value="{cert}" disabled /></div>
      <div class="form-group half"><label>Threads</label><input type="text" value="{threads}" disabled /></div>
    </div>
    <div class="form-group"><label>Idle Timeout</label><input type="text" value="{idle}" disabled /></div>"#,
            remote = html_escape(s.remote.as_deref().unwrap_or("")),
            tls_host = html_escape(&s.tls_host),
            cert = html_escape(&s.cert),
            threads = s.threads,
            idle = s.idle_timeout_secs,
        )
    } else {
        // Client proxy mode
        format!(
            r#"<div class="form-row">
      <div class="form-group half"><label>Listen</label><input type="text" value="{listen}" disabled /></div>
      <div class="form-group half"><label>Remote</label><input type="text" value="{remote}" disabled /></div>
    </div>
    <div class="form-row">
      <div class="form-group half"><label>Certificate</label><input type="text" value="{cert}" disabled /></div>
      <div class="form-group half"><label>TLS Host</label><input type="text" value="{tls_host}" disabled /></div>
    </div>
    <div class="form-row">
      <div class="form-group third"><label>Concurrent</label><input type="text" value="{concurrent}" disabled /></div>
      <div class="form-group third"><label>Threads</label><input type="text" value="{threads}" disabled /></div>
      <div class="form-group third"><label>Idle Timeout</label><input type="text" value="{idle}" disabled /></div>
    </div>
    <div class="form-row">
      <div class="form-group third"><label>Max Connections</label><input type="text" value="{max_conn}" disabled /></div>
      <div class="form-group third"><label>Transparent Proxy</label><input type="text" value="{tproxy}" disabled /></div>
    </div>"#,
            listen = html_escape(&s.listen),
            remote = html_escape(s.remote.as_deref().unwrap_or("")),
            cert = html_escape(&s.cert),
            tls_host = html_escape(&s.tls_host),
            concurrent = s.concurrent,
            threads = s.threads,
            idle = s.idle_timeout_secs,
            max_conn = s.max_connections,
            tproxy = s.tproxy,
        )
    };

    // --- Tunnel section (only for client tunnel mode) ---
    let tunnel_section = if !is_server && is_tunnel {
        let tunnel_rows_html = reloadable
            .tunnel_entries
            .iter()
            .map(|e| {
                let local = html_escape(&e.local_addr);
                let remote = e.remote_port.to_string();
                let sni = e.sni.as_deref().unwrap_or("");
                format!(
                    r#"<div class="tunnel-row"><input type="text" class="t-local" placeholder="host:port" value="{}" /><input type="text" class="t-remote" placeholder="remote port" value="{}" /><input type="text" class="t-sni" placeholder="optional" value="{}" /><button type="button" class="btn btn-remove" onclick="this.parentElement.remove()">✕</button></div>"#,
                    local, remote, sni
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            r#"<div class="section-title">Tunnel</div>
    <div class="form-group">
      <label for="tunnel_client_id">Client ID</label>
      <input type="text" id="tunnel_client_id" name="tunnel_client_id" value="{client_id}" placeholder="unique identifier for this client" />
    </div>
    <div class="tunnel-header">
      <span class="th-local">Local Address</span>
      <span class="th-remote">Remote Port</span>
      <span class="th-sni">SNI</span>
      <span class="th-action"></span>
    </div>
    <div id="tunnel-rows">
      {tunnel_rows}
    </div>
    <button type="button" class="btn btn-add" onclick="addTunnelRow()">+ Add Tunnel</button>
    <input type="hidden" id="tunnel" name="tunnel" />"#,
            client_id = html_escape(&reloadable.tunnel_client_id),
            tunnel_rows = tunnel_rows_html,
        )
    } else {
        String::new()
    };

    let alert_html = match alert {
        Some(msg) if msg.starts_with("Error") => {
            format!("<div class=\"alert alert-error\">{}</div>", msg)
        }
        Some(msg) => {
            format!("<div class=\"alert alert-success\">{}</div>", msg)
        }
        None => String::new(),
    };

    let role_label = if is_server {
        "Server".to_string()
    } else if is_tunnel {
        "Client (Tunnel)".to_string()
    } else {
        "Client (Proxy)".to_string()
    };

    let html = HTML_TEMPLATE
        .replace("{{CSS}}", CSS)
        .replace("{{ROLE_LABEL}}", &role_label)
        .replace("{{ADMIN_LISTEN}}", &s.admin_listen)
        .replace("{{CONNECTION_FIELDS}}", &connection_fields)
        .replace("{{TUNNEL_SECTION}}", &tunnel_section)
        .replace("{{ALERT}}", &alert_html);

    Ok(html)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

async fn handle_config_save(config: &AppConfig, body: &str) -> anyhow::Result<()> {
    let tunnel_raw = parse_form_value(body, "tunnel").unwrap_or_default();
    let client_id = parse_form_value(body, "tunnel_client_id").unwrap_or_default();

    // Parse tunnel entries
    let mut entries = Vec::new();
    for line in tunnel_raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        entries.push(tunnel_config::parse_tunnel_arg(line)?);
    }

    if !entries.is_empty() && client_id.is_empty() {
        return Err(anyhow::anyhow!(
            "tunnel_client_id is required when tunnel entries are specified"
        ));
    }

    // Update reloadable config
    {
        let mut reloadable = config.reloadable.lock().await;
        reloadable.tunnel_entries = entries;
        reloadable.tunnel_client_id = client_id;
    }

    // Trigger reload
    config.trigger_reload().await;

    // Re-read direct-bypass rules file (if any) so an admin "save" also picks
    // up rules-file edits, not just tunnel config.
    let _ = config.direct_ctx.reload();

    tracing::info!("Configuration reloaded via admin page");
    Ok(())
}
