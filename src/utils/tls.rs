use anyhow::Context;
use anyhow::Result;
use pki_types::CertificateDer;
use pki_types::PrivateKeyDer;
use pki_types::PrivatePkcs8KeyDer;
use std::path::Path;

pub fn read_tokio_tls_certs(cert_path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let cert_chain = std::fs::read(cert_path).context("failed to read certificate chain")?;
    let cert_chain = if cert_path.extension().is_some_and(|x| x == "der") {
        vec![CertificateDer::from(cert_chain)]
    } else {
        rustls_pemfile::certs(&mut &*cert_chain)
            .collect::<Result<_, _>>()
            .context("invalid PEM-encoded certificate")?
    };
    Ok(cert_chain)
}

pub fn read_private_key(key_path: &Path) -> Result<PrivateKeyDer<'static>> {
    let key = std::fs::read(key_path).context("failed to read private key")?;
    let key = if key_path.extension().is_some_and(|x| x == "der") {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key))
    } else {
        rustls_pemfile::private_key(&mut &*key)
            .context("malformed PKCS #1 private key")?
            .ok_or_else(|| anyhow::Error::msg("no private keys found"))?
    };
    Ok(key)
}
