use anyhow::{Context, Result};

use std::path::PathBuf;

pub fn read_tokio_tls_certs(cert_path: &PathBuf) -> Result<Vec<tokio_rustls::rustls::Certificate>> {
    let certs = std::fs::read(cert_path.clone()).context("failed to read certificate chain")?;
    let certs = if cert_path.extension().map_or(false, |x| x == "der") {
        vec![tokio_rustls::rustls::Certificate(certs)]
    } else {
        rustls_pemfile::certs(&mut &*certs)
            .context("invalid PEM-encoded certificate")?
            .into_iter()
            .map(tokio_rustls::rustls::Certificate)
            .collect()
    };
    Ok(certs)
}

// pub fn read_tls_certs(cert_path: &PathBuf) -> Result<Vec<rustls::Certificate>> {
//     let certs = std::fs::read(cert_path.clone()).context("failed to read certificate chain")?;
//     let certs = if cert_path.extension().map_or(false, |x| x == "der") {
//         vec![rustls::Certificate(certs)]
//     } else {
//         rustls_pemfile::certs(&mut &*certs)
//             .context("invalid PEM-encoded certificate")?
//             .into_iter()
//             .map(rustls::Certificate)
//             .collect()
//     };
//     Ok(certs)
// }
