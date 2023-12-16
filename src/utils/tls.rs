use anyhow::Context;
use anyhow::Result;
// use pki_types::CertificateDer;
use rustls_pemfile::certs;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

fn load_certs(path: &std::path::Path) -> std::io::Result<Vec<rustls::Certificate>> {
    // certs(&mut BufReader::new(File::open(path)?)).collect()

    let mut pem_certs = Vec::new();
    for cert in certs(&mut BufReader::new(File::open(path)?))? {
        pem_certs.push(rustls::Certificate(cert));
    }
    Ok(pem_certs)
}
pub fn read_tokio_tls_certs(cert_path: &Path) -> Result<Vec<rustls::Certificate>> {
    // let certs = std::fs::read(cert_path.clone()).context("failed to read certificate chain")?;
    // let certs = if cert_path.extension().map_or(false, |x| x == "der") {
    //     vec![tokio_rustls::rustls::Certificate(certs)]
    // } else {
    //     let mut pem_certs = Vec::new();
    //     for cert in rustls_pemfile::certs(&mut &*certs) {
    //         pem_certs.push(tokio_rustls::rustls::Certificate(Vec::from(cert?.as_ref())));
    //     }
    //     // rustls_pemfile::certs(&mut &*certs)
    //     //     .context("invalid PEM-encoded certificate")?
    //     //     .into_iter()
    //     //     .map(tokio_rustls::rustls::Certificate)
    //     //     .collect()
    //     pem_certs
    // };
    // Ok(certs)
    match load_certs(cert_path) {
        Ok(r) => Ok(r),
        Err(e) => Err(e.into()),
    }
}

pub fn read_pem_private_key(key_path: &Path) -> Result<rustls::PrivateKey> {
    let key = std::fs::read(key_path).context("failed to read private key")?;
    match rustls_pemfile::read_one(&mut &*key) {
        Ok(x) => match x.unwrap() {
            // rustls_pemfile::Item::Pkcs1Key(key) => {
            rustls_pemfile::Item::RSAKey(key) => {
                tracing::debug!("private key with PKCS #1 format");
                //PrivateKeyDer::Pkcs1(key)
                Ok(rustls::PrivateKey(key))
            }
            // rustls_pemfile::Item::Pkcs8Key(key) => {
            rustls_pemfile::Item::PKCS8Key(key) => {
                tracing::debug!("private key with PKCS #8 format");
                //PrivateKeyDer::Pkcs8(key)
                Ok(rustls::PrivateKey(key))
            }
            // rustls_pemfile::Item::Sec1Key(key) => {
            rustls_pemfile::Item::ECKey(key) => {
                tracing::debug!("private key with SEC1 format");
                //PrivateKeyDer::Sec1(key)
                Ok(rustls::PrivateKey(key))
            }
            rustls_pemfile::Item::X509Certificate(_) => {
                anyhow::bail!("you should provide a key file instead of cert");
            }
            _ => {
                anyhow::bail!("no private keys found");
            }
        },
        Err(_) => {
            anyhow::bail!("malformed private key");
        }
    }
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
