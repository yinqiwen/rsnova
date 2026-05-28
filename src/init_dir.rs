use anyhow::{anyhow, Result};
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

pub fn init_data_dir(output_dir: &Path, default_tls_host: &str, force: bool) -> Result<()> {
    fs::create_dir_all(output_dir)
        .map_err(|e| anyhow!("create output directory {:?} failed: {}", output_dir, e))?;

    if is_interactive() {
        init_data_dir_interactive(output_dir, default_tls_host, force)
    } else {
        init_data_dir_noninteractive(output_dir, default_tls_host, force)
    }
}

fn is_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

fn init_data_dir_noninteractive(output_dir: &Path, tls_host: &str, force: bool) -> Result<()> {
    rcgen(output_dir, tls_host, force)?;

    let cert = default_cert_path(output_dir);
    let key = default_key_path(output_dir);

    write_init_file(
        &output_dir.join("server.toml"),
        &render_server_toml(&cert, &key, "0.0.0.0:48100", "0.0.0.0:48102", "8000-9000"),
        force,
    )?;
    write_init_file(
        &output_dir.join("client_proxy.toml"),
        &render_client_proxy_toml(
            tls_host,
            &cert,
            "tls://server:48100",
            "0.0.0.0:48101",
            "0.0.0.0:48103",
        ),
        force,
    )?;
    write_init_file(
        &output_dir.join("client_tunnel.toml"),
        &render_client_tunnel_toml(
            tls_host,
            &cert,
            "tls://127.0.0.1:48100",
            "0.0.0.0:48104",
            "my-client",
            &["8080:80".to_string(), "8443:443".to_string()],
        ),
        force,
    )?;

    Ok(())
}

fn init_data_dir_interactive(output_dir: &Path, default_tls_host: &str, force: bool) -> Result<()> {
    let cert_path = output_dir.join("cert.pem");
    let key_path = output_dir.join("key.pem");
    let certs_exist = cert_path.exists() && key_path.exists();

    let mut tls_host = default_tls_host.to_string();

    if certs_exist && !force {
        if prompt_yes_no("Existing certificates found. Reuse them?", true)? {
            println!("Reusing existing certificates.");
        } else if prompt_yes_no("Generate new certificate and key?", true)? {
            tls_host = prompt_string("TLS hostname (for certificate)", default_tls_host)?;
            rcgen(output_dir, &tls_host, true)?;
        }
    } else if force || prompt_yes_no("Generate certificate and key?", true)? {
        tls_host = prompt_string("TLS hostname (for certificate)", default_tls_host)?;
        rcgen(output_dir, &tls_host, force)?;
    } else {
        println!("Skipping certificate generation.");
    }

    let default_cert = default_cert_path(output_dir);
    let default_key = default_key_path(output_dir);

    if should_write_config(&output_dir.join("server.toml"), "server.toml", force)? {
        let cert = prompt_string("  cert path", &default_cert)?;
        let key = prompt_string("  key path", &default_key)?;
        let listen = prompt_string("  listen address", "0.0.0.0:48100")?;
        let admin_listen = prompt_string("  admin listen address", "0.0.0.0:48102")?;
        let tunnel_port_range = prompt_string("  tunnel port range", "8000-9000")?;
        let content = render_server_toml(&cert, &key, &listen, &admin_listen, &tunnel_port_range);
        write_init_file(&output_dir.join("server.toml"), &content, true)?;
    }

    if should_write_config(
        &output_dir.join("client_proxy.toml"),
        "client_proxy.toml",
        force,
    )? {
        let cert = prompt_string("  cert path", &default_cert)?;
        let tls_host = prompt_string("  tls_host (runtime SNI)", &tls_host)?;
        let remote = prompt_string("  remote server", "tls://server:48100")?;
        let listen = prompt_string("  listen address", "0.0.0.0:48101")?;
        let admin_listen = prompt_string("  admin listen address", "0.0.0.0:48103")?;
        let content = render_client_proxy_toml(&tls_host, &cert, &remote, &listen, &admin_listen);
        write_init_file(&output_dir.join("client_proxy.toml"), &content, true)?;
    }

    if should_write_config(
        &output_dir.join("client_tunnel.toml"),
        "client_tunnel.toml",
        force,
    )? {
        println!("  Tunnel mapping formats: port | local:remote | host:local:remote | host:local:remote:sni");
        let cert = prompt_string("  cert path", &default_cert)?;
        let tls_host = prompt_string("  tls_host (runtime SNI)", &tls_host)?;
        let remote = prompt_string("  remote server", "tls://127.0.0.1:48100")?;
        let tunnel_client_id = prompt_string("  tunnel client ID", "my-client")?;
        let tunnel_raw = prompt_string("  tunnel port mappings (comma-separated)", "8080:80,8443:443")?;
        let admin_listen = prompt_string("  admin listen address", "0.0.0.0:48104")?;
        let tunnel_entries: Vec<String> = tunnel_raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if tunnel_entries.is_empty() {
            return Err(anyhow!("at least one tunnel port mapping is required"));
        }
        let content = render_client_tunnel_toml(
            &tls_host,
            &cert,
            &remote,
            &admin_listen,
            &tunnel_client_id,
            &tunnel_entries,
        );
        write_init_file(&output_dir.join("client_tunnel.toml"), &content, true)?;
    }

    Ok(())
}

fn should_write_config(path: &Path, name: &str, force: bool) -> Result<bool> {
    if path.exists() && !force {
        if !prompt_yes_no(&format!("{name} already exists. Overwrite?"), false)? {
            println!("Skipping {name}.");
            return Ok(false);
        }
    }
    Ok(prompt_yes_no(&format!("Generate {name}?"), true)?)
}

fn default_cert_path(output_dir: &Path) -> String {
    if use_docker_data_paths(output_dir) {
        "/data/cert.pem".to_string()
    } else {
        output_dir.join("cert.pem").display().to_string()
    }
}

fn default_key_path(output_dir: &Path) -> String {
    if use_docker_data_paths(output_dir) {
        "/data/key.pem".to_string()
    } else {
        output_dir.join("key.pem").display().to_string()
    }
}

fn use_docker_data_paths(output_dir: &Path) -> bool {
    if output_dir == Path::new("/data") {
        return true;
    }
    matches!(
        output_dir.file_name().and_then(|s| s.to_str()),
        Some("rsnova_data" | "data")
    )
}

fn prompt_yes_no(message: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "Y/n" } else { "y/N" };
    loop {
        print!("{message} [{hint}]: ");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let line = line.trim();
        if line.is_empty() {
            return Ok(default_yes);
        }
        match line.to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("Please enter y or n."),
        }
    }
}

fn prompt_string(message: &str, default: &str) -> Result<String> {
    print!("{message} [{default}]: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let line = line.trim();
    if line.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(line.to_string())
    }
}

fn rcgen(output_dir: &Path, tls_host: &str, force: bool) -> Result<()> {
    let cert_path = output_dir.join("cert.pem");
    let key_path = output_dir.join("key.pem");

    if cert_path.exists() && key_path.exists() && !force {
        println!(
            "Skipping existing certificates at {}",
            output_dir.display()
        );
        return Ok(());
    }

    println!(
        "Generating self-signed certificate at {} and {} with host: {}",
        cert_path.display(),
        key_path.display(),
        tls_host,
    );
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec![tls_host.into()])
            .map_err(|e| anyhow!("generate cert failed: {}", e))?;
    let key = signing_key.serialize_pem();
    let cert = cert.pem();

    fs::write(&cert_path, cert).map_err(|e| anyhow!("write cert failed: {}", e))?;
    fs::write(&key_path, key).map_err(|e| anyhow!("write key failed: {}", e))?;
    println!("Certificate generated successfully");
    Ok(())
}

fn write_init_file(path: &Path, content: &str, force: bool) -> Result<()> {
    if path.exists() && !force {
        println!("Skipping existing file: {}", path.display());
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create directory {:?} failed: {}", parent, e))?;
    }

    fs::write(path, content).map_err(|e| anyhow!("write {:?} failed: {}", path, e))?;
    println!("Wrote {}", path.display());
    Ok(())
}

fn render_server_toml(
    cert: &str,
    key: &str,
    listen: &str,
    admin_listen: &str,
    tunnel_port_range: &str,
) -> String {
    format!(
        r#"# === Server Configuration ===
# Server listens on TLS (TCP) + QUIC (UDP) simultaneously on the same port.
role = "server"
listen = "{listen}"
admin_listen = "{admin_listen}"
cert = "{cert}"
key = "{key}"
tunnel_port_range = "{tunnel_port_range}"

# === Optional (uncomment to customize) ===
# concurrent = 5
# threads = 2
# idle_timeout_secs = 120
# mux_stream_window = 262144
# max_connections = 256
# log = ""
# NOTE: If enabling file logging to /data/, remove :ro from the volume mount.
"#
    )
}

fn render_client_proxy_toml(
    tls_host: &str,
    cert: &str,
    remote: &str,
    listen: &str,
    admin_listen: &str,
) -> String {
    format!(
        r#"# === Client Proxy Configuration ===
role = "client"
remote = "{remote}"
listen = "{listen}"
admin_listen = "{admin_listen}"
cert = "{cert}"
tls_host = "{tls_host}"

# === Optional (uncomment to customize) ===
# concurrent = 5
# threads = 2
# idle_timeout_secs = 120
# mux_stream_window = 262144
# max_connections = 256
# log = ""
# NOTE: "remote" uses Docker DNS name "server". For standalone docker run,
# replace with actual IP or host.docker.internal.
# NOTE: If enabling file logging to /data/, remove :ro from the volume mount.
"#
    )
}

fn render_client_tunnel_toml(
    tls_host: &str,
    cert: &str,
    remote: &str,
    admin_listen: &str,
    tunnel_client_id: &str,
    tunnel_entries: &[String],
) -> String {
    let tunnel_list = tunnel_entries
        .iter()
        .map(|entry| format!("\"{entry}\""))
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        r#"# === Client Tunnel Configuration ===
role = "client"
remote = "{remote}"
admin_listen = "{admin_listen}"
cert = "{cert}"
tls_host = "{tls_host}"
tunnel_client_id = "{tunnel_client_id}"
# Formats: "port" | "local:remote" | "host:local:remote" | "host:local:remote:sni"
tunnel = [{tunnel_list}]

# === Optional (uncomment to customize) ===
# threads = 2
# idle_timeout_secs = 120
# mux_stream_window = 262144
# log = ""
# NOTE: If enabling file logging to /data/, remove :ro from the volume mount.
"#
    )
}

pub fn rcgen_current_dir(tls_host: &str) -> Result<()> {
    rcgen(Path::new("."), tls_host, true)
}
