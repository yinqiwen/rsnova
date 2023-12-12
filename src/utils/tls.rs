use anyhow::Result;
use pki_types::CertificateDer;
use rustls_pemfile::certs;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

fn load_certs(path: &std::path::Path) -> std::io::Result<Vec<CertificateDer<'static>>> {
    certs(&mut BufReader::new(File::open(path)?)).collect()
}
pub fn read_tokio_tls_certs(cert_path: &Path) -> Result<Vec<CertificateDer<'static>>> {
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
